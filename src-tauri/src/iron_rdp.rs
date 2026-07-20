use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ironrdp::input::{Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp_client::{
    config::{ClipboardType, ConfigBuilder, Destination},
    rdp::{RdpClient, RdpInputEvent, RdpOutputEvent},
};
#[cfg(target_os = "windows")]
use ironrdp_cliprdr::backend::ClipboardMessage;
#[cfg(target_os = "windows")]
use ironrdp_cliprdr_native::{
    clear_file_clipboard, file_clipboard_progress, file_clipboard_ready, set_file_clipboard_paths,
};
use serde::{Deserialize, Serialize};
use tauri::{ipc::Channel, State};
use tokio::sync::mpsc;

const RDP_FRAME_INTERVAL: Duration = Duration::from_millis(50);
const RDP_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const RDP_MIN_WIDTH: u16 = 320;
const RDP_MIN_HEIGHT: u16 = 200;
const RDP_MAX_WIDTH: u16 = 2560;
const RDP_MAX_HEIGHT: u16 = 1600;

#[derive(Clone)]
struct IronRdpSessionHandle {
    input: mpsc::UnboundedSender<RdpInputEvent>,
    input_state: Arc<Mutex<Database>>,
    generation: Arc<()>,
}

#[derive(Default)]
pub struct IronRdpSessions {
    sessions: Arc<Mutex<HashMap<String, IronRdpSessionHandle>>>,
}

#[derive(Default)]
pub struct RdpFileTransfers {
    transfers: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum IronRdpStatusEvent {
    Connecting,
    Connected { width: u16, height: u16 },
    Error { code: String, message: String },
    Closed { message: String },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RdpFileTransferResult {
    copied_files: usize,
    destination: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RdpFileTransferProgress {
    total_bytes: u64,
    transferred_bytes: u64,
    bytes_per_second: u64,
    copied_files: usize,
    total_files: usize,
    current_file: String,
    completed: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RdpClipboardFileOffer {
    total_files: usize,
    total_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RdpClipboardFileProgress {
    total_files: usize,
    total_bytes: u64,
    transferred_bytes: u64,
    current_file: String,
    completed: bool,
    accepted: Option<bool>,
}

#[cfg(target_os = "windows")]
struct FileTransferProgressState {
    total_bytes: u64,
    transferred_bytes: u64,
    copied_files: usize,
    total_files: usize,
    last_sample_at: Instant,
    last_sample_bytes: u64,
    bytes_per_second: u64,
    completed: bool,
    cancelled: Arc<AtomicBool>,
    on_progress: Channel<RdpFileTransferProgress>,
}

#[cfg(target_os = "windows")]
impl FileTransferProgressState {
    fn ensure_active(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::Relaxed) {
            Err("文件传输已取消".to_string())
        } else {
            Ok(())
        }
    }

    fn emit(&mut self, current_file: &Path, force: bool) {
        let elapsed = self.last_sample_at.elapsed();
        if !force && elapsed < Duration::from_millis(120) {
            return;
        }
        if !elapsed.is_zero() {
            let bytes = self
                .transferred_bytes
                .saturating_sub(self.last_sample_bytes);
            if bytes > 0 {
                self.bytes_per_second = (bytes as f64 / elapsed.as_secs_f64()).round() as u64;
            }
        }
        self.last_sample_at = Instant::now();
        self.last_sample_bytes = self.transferred_bytes;
        let _ = self.on_progress.send(RdpFileTransferProgress {
            total_bytes: self.total_bytes,
            transferred_bytes: self.transferred_bytes,
            bytes_per_second: self.bytes_per_second,
            copied_files: self.copied_files,
            total_files: self.total_files,
            current_file: current_file.to_string_lossy().into_owned(),
            completed: self.completed,
        });
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum IronRdpInput {
    MouseMove {
        x: u16,
        y: u16,
    },
    MouseButton {
        button: u8,
        down: bool,
    },
    Wheel {
        delta_x: i16,
        delta_y: i16,
    },
    Key {
        code: u8,
        extended: bool,
        down: bool,
    },
    Text {
        text: String,
    },
    Resize {
        width: u16,
        height: u16,
        #[serde(rename = "scaleFactor")]
        scale_factor: u32,
    },
    ReleaseAll,
    CtrlAltDelete,
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn rdp_connect(
    sessions: State<'_, IronRdpSessions>,
    session_id: String,
    host: String,
    port: u16,
    username: String,
    password: String,
    domain: String,
    security: String,
    _ignore_certificate: bool,
    width: u16,
    height: u16,
    on_status: Channel<IronRdpStatusEvent>,
    on_frame: Channel<Vec<u8>>,
) -> Result<(), String> {
    validate_connection(&session_id, &host, port, &username, &password)?;
    crate::diag_log(
        "rdp",
        format!("connect session={session_id} target={host}:{port} security={security}"),
    );

    let width = width.clamp(RDP_MIN_WIDTH, RDP_MAX_WIDTH);
    let height = height.clamp(RDP_MIN_HEIGHT, RDP_MAX_HEIGHT);
    let mut builder = ConfigBuilder::new()
        .with_destination(Destination::from_parts(host.clone(), port))
        .with_username(username)
        .with_password(password)
        .with_domain(domain)
        .with_client_build(22_621)
        .with_client_dir(r"C:\Windows\System32\mstscax.dll")
        .with_client_name("XunDuTerminal")
        .with_platform(ironrdp::pdu::rdp::capability_sets::MajorPlatformType::WINDOWS)
        .with_desktop_width(width)
        .with_desktop_height(height)
        .with_desktop_scale_factor(100)
        .with_color_depth(32)
        .with_autologon(true)
        .with_server_pointer(true)
        .with_pointer_software_rendering(true)
        .with_compression(true)
        .with_clipboard(ClipboardType::Enable)
        .with_fake_events_interval(Duration::from_secs(30));

    builder = match security.to_ascii_lowercase().as_str() {
        "nla" => builder.with_credssp(true).with_tls(false),
        "tls" => builder.with_credssp(false).with_tls(true),
        // Auto starts with NLA. Advertising TLS and CredSSP together lets servers
        // select HYBRID_EX, which is less widely interoperable than regular NLA.
        _ => builder.with_credssp(true).with_tls(false),
    };

    let config = builder
        .build()
        .map_err(|error| format!("RDP configuration failed: {error:#}"))?;
    let (output_sender, output_receiver) = mpsc::channel::<RdpOutputEvent>(8);
    let client = RdpClient::new(config, output_sender);
    let input = client.input_sender();
    let generation = Arc::new(());
    let handle = IronRdpSessionHandle {
        input: input.clone(),
        input_state: Arc::new(Mutex::new(Database::new())),
        generation: generation.clone(),
    };

    if let Some(previous) = sessions
        .sessions
        .lock()
        .map_err(|_| "RDP session store is unavailable".to_string())?
        .insert(session_id.clone(), handle)
    {
        let _ = previous.input.send(RdpInputEvent::Close);
    }

    let _ = on_status.send(IronRdpStatusEvent::Connecting);
    let session_store = sessions.sessions.clone();
    let output_session_id = session_id.clone();
    tauri::async_runtime::spawn(async move {
        forward_rdp_output(
            output_receiver,
            input,
            on_status,
            on_frame,
            session_store,
            output_session_id,
            generation,
        )
        .await;
    });
    std::thread::Builder::new()
        .name(format!("xundu-rdp-{session_id}"))
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => run_rdp_client(runtime, client),
                Err(error) => log::error!("Failed to start RDP runtime: {error}"),
            }
        })
        .map_err(|error| format!("Failed to start RDP session thread: {error}"))?;

    Ok(())
}

fn run_rdp_client(runtime: tokio::runtime::Runtime, client: RdpClient) {
    #[cfg(target_os = "windows")]
    runtime.block_on(async move {
        let message_pump = async {
            loop {
                pump_windows_messages();
                tokio::time::sleep(Duration::from_millis(8)).await;
            }
        };
        tokio::select! {
            _ = client.run() => {},
            _ = message_pump => {},
        }
    });

    #[cfg(not(target_os = "windows"))]
    runtime.block_on(client.run());
}

#[cfg(target_os = "windows")]
fn pump_windows_messages() {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    let mut message = MSG::default();
    // The hidden CLIPRDR window is owned by this RDP thread, so its queue must
    // be pumped on the same thread as the client future.
    unsafe {
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

#[tauri::command]
pub fn rdp_input(
    sessions: State<'_, IronRdpSessions>,
    session_id: String,
    input: IronRdpInput,
) -> Result<(), String> {
    let handle = sessions
        .sessions
        .lock()
        .map_err(|_| "RDP session store is unavailable".to_string())?
        .get(&session_id)
        .cloned()
        .ok_or_else(|| "RDP session is not connected".to_string())?;

    match input {
        IronRdpInput::Resize {
            width,
            height,
            scale_factor,
        } => send_input(
            &handle,
            RdpInputEvent::Resize {
                width: width.clamp(RDP_MIN_WIDTH, RDP_MAX_WIDTH),
                height: height.clamp(RDP_MIN_HEIGHT, RDP_MAX_HEIGHT),
                scale_factor: scale_factor.clamp(100, 500),
                physical_size: None,
            },
        ),
        IronRdpInput::ReleaseAll => {
            let events = handle
                .input_state
                .lock()
                .map_err(|_| "RDP input state is unavailable".to_string())?
                .release_all();
            if events.is_empty() {
                Ok(())
            } else {
                send_input(&handle, RdpInputEvent::FastPath(events))
            }
        }
        IronRdpInput::MouseMove { x, y } => {
            apply_operations(&handle, [Operation::MouseMove(MousePosition { x, y })])
        }
        IronRdpInput::MouseButton { button, down } => {
            let button = MouseButton::from_web_button(button)
                .ok_or_else(|| "Unsupported mouse button".to_string())?;
            apply_operations(
                &handle,
                [if down {
                    Operation::MouseButtonPressed(button)
                } else {
                    Operation::MouseButtonReleased(button)
                }],
            )
        }
        IronRdpInput::Wheel { delta_x, delta_y } => {
            let mut operations = Vec::with_capacity(2);
            if delta_x != 0 {
                operations.push(Operation::WheelRotations(WheelRotations {
                    is_vertical: false,
                    rotation_units: delta_x,
                }));
            }
            if delta_y != 0 {
                operations.push(Operation::WheelRotations(WheelRotations {
                    is_vertical: true,
                    rotation_units: delta_y,
                }));
            }
            apply_operations(&handle, operations)
        }
        IronRdpInput::Key {
            code,
            extended,
            down,
        } => {
            let scancode = Scancode::from_u8(extended, code);
            apply_operations(
                &handle,
                [if down {
                    Operation::KeyPressed(scancode)
                } else {
                    Operation::KeyReleased(scancode)
                }],
            )
        }
        IronRdpInput::Text { text } => {
            let operations = text.chars().flat_map(|character| {
                [
                    Operation::UnicodeKeyPressed(character),
                    Operation::UnicodeKeyReleased(character),
                ]
            });
            apply_operations(&handle, operations)
        }
        IronRdpInput::CtrlAltDelete => apply_operations(
            &handle,
            [
                Operation::KeyPressed(Scancode::from_u8(false, 0x1d)),
                Operation::KeyPressed(Scancode::from_u8(false, 0x38)),
                Operation::KeyPressed(Scancode::from_u8(true, 0x53)),
                Operation::KeyReleased(Scancode::from_u8(true, 0x53)),
                Operation::KeyReleased(Scancode::from_u8(false, 0x38)),
                Operation::KeyReleased(Scancode::from_u8(false, 0x1d)),
            ],
        ),
    }
}

#[tauri::command]
pub fn rdp_clipboard_file_paths() -> Result<Vec<String>, String> {
    #[cfg(target_os = "windows")]
    {
        clipboard_file_paths_windows()
    }

    #[cfg(not(target_os = "windows"))]
    {
        Err("File clipboard transfer is currently available on Windows only".to_string())
    }
}

#[tauri::command]
pub fn rdp_clipboard_sequence_number() -> u32 {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::System::DataExchange::GetClipboardSequenceNumber;
        unsafe { GetClipboardSequenceNumber() }
    }

    #[cfg(not(target_os = "windows"))]
    {
        0
    }
}

#[tauri::command]
pub fn rdp_offer_clipboard_files(
    sessions: State<'_, IronRdpSessions>,
    session_id: String,
    paths: Vec<String>,
) -> Result<RdpClipboardFileOffer, String> {
    #[cfg(target_os = "windows")]
    {
        if !file_clipboard_ready() {
            return Err("RDP 文件剪贴板通道尚未就绪".to_string());
        }
        let descriptors = set_file_clipboard_paths(paths)?;
        let progress = file_clipboard_progress();
        let sessions = sessions
            .sessions
            .lock()
            .map_err(|_| "RDP session state is unavailable".to_string())?;
        let handle = sessions
            .get(&session_id)
            .ok_or_else(|| "RDP session is not connected".to_string())?;
        send_input(
            handle,
            RdpInputEvent::Clipboard(ClipboardMessage::SendInitiateFileCopy(descriptors)),
        )?;
        Ok(RdpClipboardFileOffer {
            total_files: progress.total_files,
            total_bytes: progress.total_bytes,
        })
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (sessions, session_id, paths);
        Err("RDP file clipboard is currently available on Windows only".to_string())
    }
}

#[tauri::command]
pub fn rdp_file_clipboard_progress() -> Result<RdpClipboardFileProgress, String> {
    #[cfg(target_os = "windows")]
    {
        let progress = file_clipboard_progress();
        Ok(RdpClipboardFileProgress {
            total_files: progress.total_files,
            total_bytes: progress.total_bytes,
            transferred_bytes: progress.served_bytes,
            current_file: progress.current_file,
            completed: progress.total_files == 0 || progress.served_bytes >= progress.total_bytes,
            accepted: progress.accepted,
        })
    }

    #[cfg(not(target_os = "windows"))]
    {
        Err("RDP file clipboard is currently available on Windows only".to_string())
    }
}

#[tauri::command]
pub async fn rdp_upload_files(
    transfers: State<'_, RdpFileTransfers>,
    transfer_id: String,
    paths: Vec<String>,
    host: String,
    username: String,
    password: String,
    domain: String,
    on_progress: Channel<RdpFileTransferProgress>,
) -> Result<RdpFileTransferResult, String> {
    let transfer_id = transfer_id.trim().to_string();
    if transfer_id.is_empty() {
        return Err("文件传输任务标识不能为空".to_string());
    }
    if paths.is_empty() {
        return Err("没有可传输的文件".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        let cancelled = Arc::new(AtomicBool::new(false));
        let active_transfers = transfers.transfers.clone();
        {
            let mut active = active_transfers
                .lock()
                .map_err(|_| "文件传输任务状态不可用".to_string())?;
            if let Some(previous) = active.insert(transfer_id.clone(), cancelled.clone()) {
                previous.store(true, Ordering::Relaxed);
            }
        }
        let task_cancelled = cancelled.clone();
        let result = tauri::async_runtime::spawn_blocking(move || {
            upload_files_windows(
                paths,
                host,
                username,
                password,
                domain,
                task_cancelled,
                on_progress,
            )
        })
        .await
        .unwrap_or_else(|error| Err(format!("文件传输任务异常结束: {error}")));
        if let Ok(mut active) = active_transfers.lock() {
            let current = active
                .get(&transfer_id)
                .map(|flag| Arc::ptr_eq(flag, &cancelled))
                .unwrap_or(false);
            if current {
                active.remove(&transfer_id);
            }
        }
        result
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (
            transfers,
            transfer_id,
            paths,
            host,
            username,
            password,
            domain,
            on_progress,
        );
        Err("File transfer is currently available on Windows only".to_string())
    }
}

#[tauri::command]
pub fn rdp_cancel_file_transfer(
    transfers: State<'_, RdpFileTransfers>,
    transfer_id: String,
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    clear_file_clipboard();
    let cancelled = transfers
        .transfers
        .lock()
        .map_err(|_| "文件传输任务状态不可用".to_string())?
        .remove(transfer_id.trim());
    if let Some(cancelled) = cancelled {
        cancelled.store(true, Ordering::Relaxed);
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn clipboard_file_paths_windows() -> Result<Vec<String>, String> {
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    let mut opened = false;
    for _ in 0..8 {
        if unsafe { OpenClipboard(None) }.is_ok() {
            opened = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if !opened {
        return Err("系统剪贴板正被其他程序占用".to_string());
    }

    let result = (|| {
        if unsafe { IsClipboardFormatAvailable(u32::from(CF_HDROP.0)) }.is_err() {
            return Ok(Vec::new());
        }
        let handle = unsafe { GetClipboardData(u32::from(CF_HDROP.0)) }
            .map_err(|error| format!("读取文件剪贴板失败: {error}"))?;
        let drop = HDROP(handle.0);
        let count = unsafe { DragQueryFileW(drop, u32::MAX, None) };
        let mut paths = Vec::with_capacity(count as usize);
        for index in 0..count {
            let length = unsafe { DragQueryFileW(drop, index, None) };
            if length == 0 {
                continue;
            }
            let mut buffer = vec![0u16; length as usize + 1];
            let written = unsafe { DragQueryFileW(drop, index, Some(&mut buffer)) } as usize;
            if written > 0 {
                paths.push(String::from_utf16_lossy(&buffer[..written]));
            }
        }
        Ok(paths)
    })();

    let _ = unsafe { CloseClipboard() };
    result
}

#[cfg(target_os = "windows")]
fn upload_files_windows(
    paths: Vec<String>,
    host: String,
    username: String,
    password: String,
    domain: String,
    cancelled: Arc<AtomicBool>,
    on_progress: Channel<RdpFileTransferProgress>,
) -> Result<RdpFileTransferResult, String> {
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{ERROR_SESSION_CREDENTIAL_CONFLICT, NO_ERROR};
    use windows::Win32::NetworkManagement::WNet::{
        WNetAddConnection2W, WNetCancelConnection2W, CONNECT_TEMPORARY, NETRESOURCEW,
        RESOURCETYPE_DISK,
    };

    let host = host.trim();
    let username = username.trim();
    if host.is_empty() || username.is_empty() || password.is_empty() {
        return Err("远程桌面主机、用户名或密码不完整".to_string());
    }

    let share = format!(r"\\{}\C$", host);
    let login = if domain.trim().is_empty() || username.contains('\\') || username.contains('@') {
        username.to_string()
    } else {
        format!(r"{}\{}", domain.trim(), username)
    };
    let mut share_wide = to_wide(&share);
    let password_wide = to_wide(&password);
    let login_wide = to_wide(&login);
    let resource = NETRESOURCEW {
        dwType: RESOURCETYPE_DISK,
        lpRemoteName: PWSTR(share_wide.as_mut_ptr()),
        ..Default::default()
    };
    let status = unsafe {
        WNetAddConnection2W(
            &resource,
            PCWSTR(password_wide.as_ptr()),
            PCWSTR(login_wide.as_ptr()),
            CONNECT_TEMPORARY,
        )
    };
    let created_connection = status == NO_ERROR;

    let result = (|| {
        ensure_transfer_active(&cancelled)?;
        let profile_name = username
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(username)
            .split('@')
            .next()
            .unwrap_or(username);
        let candidates = [
            PathBuf::from(format!(r"{}\Users\{}\Desktop", share, profile_name)),
            PathBuf::from(format!(
                r"{}\Users\{}\OneDrive\Desktop",
                share, profile_name
            )),
            PathBuf::from(format!(r"{}\Users\Public\Desktop", share)),
        ];
        let destination = candidates
            .into_iter()
            .find(|path| path.is_dir())
            .ok_or_else(|| {
                if status == ERROR_SESSION_CREDENTIAL_CONFLICT {
                    "Windows 已使用其他账号连接该服务器共享，请先断开旧的网络共享后重试".to_string()
                } else if status != NO_ERROR {
                    format!("无法访问远程桌面文件夹，SMB 错误码 {}", status.0)
                } else {
                    "未找到远程用户桌面目录".to_string()
                }
            })?;

        let sources = paths
            .into_iter()
            .map(|source| {
                fs::canonicalize(&source)
                    .map_err(|error| format!("无法读取本地文件 {source}: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut total_bytes = 0u64;
        let mut total_files = 0usize;
        for source in &sources {
            measure_path_recursive(source, &cancelled, &mut total_bytes, &mut total_files)?;
        }
        let mut progress = FileTransferProgressState {
            total_bytes,
            transferred_bytes: 0,
            copied_files: 0,
            total_files,
            last_sample_at: Instant::now(),
            last_sample_bytes: 0,
            bytes_per_second: 0,
            completed: false,
            cancelled,
            on_progress,
        };
        progress.emit(Path::new(""), true);

        for source in sources {
            let file_name = source
                .file_name()
                .ok_or_else(|| format!("无法识别本地文件名: {}", source.display()))?;
            copy_path_recursive(&source, &destination.join(file_name), &mut progress)?;
        }
        progress.completed = true;
        progress.emit(Path::new(""), true);

        Ok(RdpFileTransferResult {
            copied_files: progress.copied_files,
            destination: destination.to_string_lossy().into_owned(),
        })
    })();

    if created_connection {
        let _ = unsafe {
            WNetCancelConnection2W(PCWSTR(share_wide.as_ptr()), Default::default(), false)
        };
    }
    result
}

#[cfg(target_os = "windows")]
fn measure_path_recursive(
    source: &Path,
    cancelled: &AtomicBool,
    total_bytes: &mut u64,
    total_files: &mut usize,
) -> Result<(), String> {
    ensure_transfer_active(cancelled)?;
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("无法读取 {}: {error}", source.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("暂不传输符号链接: {}", source.display()));
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(source)
            .map_err(|error| format!("无法读取目录 {}: {error}", source.display()))?
        {
            let entry = entry.map_err(|error| format!("读取目录项失败: {error}"))?;
            measure_path_recursive(&entry.path(), cancelled, total_bytes, total_files)?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        return Err(format!("不支持的文件类型: {}", source.display()));
    }
    *total_bytes = total_bytes.saturating_add(metadata.len());
    *total_files = total_files.saturating_add(1);
    Ok(())
}

#[cfg(target_os = "windows")]
fn copy_path_recursive(
    source: &Path,
    destination: &Path,
    progress: &mut FileTransferProgressState,
) -> Result<(), String> {
    progress.ensure_active()?;
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("无法读取 {}: {error}", source.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("暂不传输符号链接: {}", source.display()));
    }
    if metadata.is_dir() {
        fs::create_dir_all(destination)
            .map_err(|error| format!("无法创建远程目录 {}: {error}", destination.display()))?;
        for entry in fs::read_dir(source)
            .map_err(|error| format!("无法读取目录 {}: {error}", source.display()))?
        {
            let entry = entry.map_err(|error| format!("读取目录项失败: {error}"))?;
            copy_path_recursive(
                &entry.path(),
                &destination.join(entry.file_name()),
                progress,
            )?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        return Err(format!("不支持的文件类型: {}", source.display()));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("无法创建远程目录 {}: {error}", parent.display()))?;
    }
    let mut reader = File::open(source)
        .map_err(|error| format!("无法打开本地文件 {}: {error}", source.display()))?;
    let staged = transfer_staging_path(destination, "part");
    let mut writer = File::create(&staged)
        .map_err(|error| format!("无法创建远程临时文件 {}: {error}", staged.display()))?;
    let mut buffer = vec![0u8; 512 * 1024];
    let copy_result = (|| {
        loop {
            progress.ensure_active()?;
            let read = reader
                .read(&mut buffer)
                .map_err(|error| format!("读取 {} 失败: {error}", source.display()))?;
            if read == 0 {
                break;
            }
            writer
                .write_all(&buffer[..read])
                .map_err(|error| format!("传输 {} 失败: {error}", source.display()))?;
            progress.transferred_bytes = progress.transferred_bytes.saturating_add(read as u64);
            progress.emit(source, false);
        }
        writer
            .flush()
            .map_err(|error| format!("写入远程文件 {} 失败: {error}", destination.display()))?;
        drop(writer);
        progress.ensure_active()?;
        commit_staged_file(&staged, destination)
    })();
    if copy_result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    copy_result?;
    progress.copied_files = progress.copied_files.saturating_add(1);
    progress.emit(source, true);
    Ok(())
}

#[cfg(target_os = "windows")]
fn ensure_transfer_active(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        Err("文件传输已取消".to_string())
    } else {
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn transfer_staging_path(destination: &Path, suffix: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    destination.with_file_name(format!(".{name}.xundu-{nonce}.{suffix}"))
}

#[cfg(target_os = "windows")]
fn commit_staged_file(staged: &Path, destination: &Path) -> Result<(), String> {
    if !destination.exists() {
        return fs::rename(staged, destination)
            .map_err(|error| format!("提交远程文件 {} 失败: {error}", destination.display()));
    }
    if !destination.is_file() {
        return Err(format!(
            "远程目标存在同名文件夹，无法覆盖: {}",
            destination.display()
        ));
    }

    let backup = transfer_staging_path(destination, "backup");
    fs::rename(destination, &backup)
        .map_err(|error| format!("备份远程文件 {} 失败: {error}", destination.display()))?;
    if let Err(error) = fs::rename(staged, destination) {
        let _ = fs::rename(&backup, destination);
        return Err(format!(
            "提交远程文件 {} 失败: {error}",
            destination.display()
        ));
    }
    let _ = fs::remove_file(backup);
    Ok(())
}

#[cfg(target_os = "windows")]
fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[tauri::command]
pub fn rdp_disconnect(
    sessions: State<'_, IronRdpSessions>,
    session_id: String,
) -> Result<(), String> {
    if let Some(handle) = sessions
        .sessions
        .lock()
        .map_err(|_| "RDP session store is unavailable".to_string())?
        .remove(&session_id)
    {
        let _ = handle.input.send(RdpInputEvent::Close);
    }
    Ok(())
}

async fn forward_rdp_output(
    mut output: mpsc::Receiver<RdpOutputEvent>,
    input: mpsc::UnboundedSender<RdpInputEvent>,
    on_status: Channel<IronRdpStatusEvent>,
    on_frame: Channel<Vec<u8>>,
    sessions: Arc<Mutex<HashMap<String, IronRdpSessionHandle>>>,
    session_id: String,
    generation: Arc<()>,
) {
    let mut ticker = tokio::time::interval(RDP_FRAME_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut latest_frame = None;
    let mut connected = false;
    let mut finished = false;
    let startup_timeout = tokio::time::sleep(RDP_CONNECT_TIMEOUT);
    tokio::pin!(startup_timeout);

    while !finished || latest_frame.is_some() {
        tokio::select! {
            event = output.recv(), if !finished => {
                match event {
                    Some(RdpOutputEvent::Image { buffer, width, height }) => {
                        if !connected {
                            connected = true;
                            crate::diag_log(
                                "rdp",
                                format!("connected session={session_id} size={}x{}", width.get(), height.get()),
                            );
                            let _ = on_status.send(IronRdpStatusEvent::Connected {
                                width: width.get(),
                                height: height.get(),
                            });
                        }
                        latest_frame = Some(pack_frame(width.get(), height.get(), buffer));
                    }
                    Some(RdpOutputEvent::ConnectionFailure(error)) => {
                        let report = error.report().to_string();
                        let code = classify_rdp_error(&report, connected);
                        crate::diag_log("rdp", format!("failed session={session_id} code={code}"));
                        let _ = on_status.send(IronRdpStatusEvent::Error {
                            code: code.to_string(),
                            message: sanitize_rdp_error(&report, connected),
                        });
                        finished = true;
                    }
                    Some(RdpOutputEvent::Terminated(result)) => {
                        match result {
                            Ok(reason) => {
                                let _ = on_status.send(IronRdpStatusEvent::Closed {
                                    message: format!("RDP session closed: {reason}"),
                                });
                            }
                            Err(error) => {
                                let report = error.report().to_string();
                                let code = classify_rdp_error(&report, connected);
                                crate::diag_log("rdp", format!("terminated session={session_id} code={code}"));
                                let _ = on_status.send(IronRdpStatusEvent::Error {
                                    code: code.to_string(),
                                    message: sanitize_rdp_error(&report, connected),
                                });
                            }
                        }
                        finished = true;
                    }
                    Some(RdpOutputEvent::PointerDefault | RdpOutputEvent::PointerHidden) => {}
                    Some(RdpOutputEvent::PointerPosition { .. } | RdpOutputEvent::PointerBitmap(_)) => {}
                    None => {
                        if !connected {
                            crate::diag_log("rdp", format!("worker_stopped session={session_id}"));
                            let _ = on_status.send(IronRdpStatusEvent::Error {
                                code: "rdp_worker_stopped".to_string(),
                                message: "RDP connection worker stopped before the desktop was ready".to_string(),
                            });
                        }
                        finished = true;
                    }
                }
            }
            _ = &mut startup_timeout, if !connected && !finished => {
                crate::diag_log("rdp", format!("startup_timeout session={session_id}"));
                let _ = on_status.send(IronRdpStatusEvent::Error {
                    code: "timeout".to_string(),
                    message: "RDP connection timed out".to_string(),
                });
                let _ = input.send(RdpInputEvent::Close);
                finished = true;
            }
            _ = ticker.tick(), if latest_frame.is_some() => {
                if let Some(frame) = latest_frame.take() {
                    if on_frame.send(frame).is_err() {
                        let _ = input.send(RdpInputEvent::Close);
                        finished = true;
                    }
                }
            }
        }
    }

    if let Ok(mut sessions) = sessions.lock() {
        let should_remove = sessions
            .get(&session_id)
            .map(|handle| Arc::ptr_eq(&handle.generation, &generation))
            .unwrap_or(false);
        if should_remove {
            sessions.remove(&session_id);
        }
    }
}

fn send_input(handle: &IronRdpSessionHandle, event: RdpInputEvent) -> Result<(), String> {
    handle
        .input
        .send(event)
        .map_err(|_| "RDP session is not connected".to_string())
}

fn apply_operations(
    handle: &IronRdpSessionHandle,
    operations: impl IntoIterator<Item = Operation>,
) -> Result<(), String> {
    let events = handle
        .input_state
        .lock()
        .map_err(|_| "RDP input state is unavailable".to_string())?
        .apply(operations);
    if events.is_empty() {
        Ok(())
    } else {
        send_input(handle, RdpInputEvent::FastPath(events))
    }
}

fn validate_connection(
    session_id: &str,
    host: &str,
    port: u16,
    username: &str,
    password: &str,
) -> Result<(), String> {
    if session_id.trim().is_empty() || host.trim().is_empty() {
        return Err("RDP session and host are required".to_string());
    }
    if port == 0 || username.trim().is_empty() || password.is_empty() {
        return Err("RDP port, username and password are required".to_string());
    }
    Ok(())
}

fn classify_rdp_error(message: &str, connected: bool) -> &'static str {
    let normalized = message.to_ascii_lowercase();
    if normalized.contains("authentication")
        || normalized.contains("credentials")
        || normalized.contains("logon")
        || normalized.contains("credssp") && normalized.contains("failed")
    {
        "authentication_failed"
    } else if normalized.contains("timed out") || normalized.contains("timeout") {
        "timeout"
    } else if normalized.contains("connection refused") {
        "connection_refused"
    } else if connected && is_frame_decode_failure(&normalized) {
        "frame_decode_failed"
    } else if normalized.contains("unexpected eof")
        || normalized.contains("close_notify")
        || normalized.contains("peer closed connection")
    {
        if connected {
            "remote_closed"
        } else {
            "remote_closed_during_startup"
        }
    } else {
        "rdp_failed"
    }
}

fn sanitize_rdp_error(message: &str, connected: bool) -> String {
    match classify_rdp_error(message, connected) {
        "authentication_failed" => "RDP authentication failed".to_string(),
        "timeout" => "RDP connection timed out".to_string(),
        "connection_refused" => "The remote host refused the RDP connection".to_string(),
        "frame_decode_failed" => {
            "The remote server returned an incomplete desktop frame".to_string()
        }
        "remote_closed" => "The remote server closed the RDP connection".to_string(),
        "remote_closed_during_startup" => {
            "The remote server closed the connection while initializing the desktop".to_string()
        }
        _ => {
            let detail = message
                .rsplit("caused by:")
                .next()
                .unwrap_or(message)
                .split("https://docs.rs/")
                .next()
                .unwrap_or(message)
                .trim();
            let detail = detail
                .strip_prefix('[')
                .and_then(|value| value.split_once("] ").map(|(_, rest)| rest))
                .unwrap_or(detail);
            let detail = detail.chars().take(240).collect::<String>();
            if detail.is_empty() {
                "RDP connection failed".to_string()
            } else {
                detail
            }
        }
    }
}

fn is_frame_decode_failure(message: &str) -> bool {
    message.contains("not enough bytes provided to decode")
}

fn pack_frame(width: u16, height: u16, pixels: Vec<u32>) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + pixels.len() * 4);
    frame.extend_from_slice(&width.to_le_bytes());
    frame.extend_from_slice(&height.to_le_bytes());
    for pixel in pixels {
        let [_, red, green, blue] = pixel.to_be_bytes();
        frame.extend_from_slice(&[red, green, blue, 255]);
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::{classify_rdp_error, pack_frame, sanitize_rdp_error, IronRdpInput};
    #[cfg(target_os = "windows")]
    use super::{
        clear_file_clipboard, commit_staged_file, ensure_transfer_active, file_clipboard_progress,
        set_file_clipboard_paths,
    };
    #[cfg(target_os = "windows")]
    use std::{
        fs,
        sync::atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn packs_ironrdp_pixels_as_rgba() {
        let frame = pack_frame(2, 1, vec![0x0011_2233, 0x00aa_bbcc]);
        assert_eq!(&frame[..4], &[2, 0, 1, 0]);
        assert_eq!(&frame[4..], &[0x11, 0x22, 0x33, 255, 0xaa, 0xbb, 0xcc, 255]);
    }

    #[test]
    fn classifies_and_hides_internal_tls_eof_details() {
        let raw = "[read frame @ C:\\Users\\name\\.cargo\\registry\\ironrdp-client\\src\\rdp.rs:766] custom error, caused by: peer closed connection without sending TLS close_notify: https://docs.rs/rustls/latest/rustls/manual/_03_howto/index.html#unexpected-eof";
        assert_eq!(
            classify_rdp_error(raw, false),
            "remote_closed_during_startup"
        );
        let message = sanitize_rdp_error(raw, false);
        assert_eq!(
            message,
            "The remote server closed the connection while initializing the desktop"
        );
        assert!(!message.contains(".cargo"));
        assert!(!message.contains("docs.rs"));
    }

    #[test]
    fn classifies_incomplete_desktop_frames_after_connection() {
        let raw = "[read frame] not enough bytes provided to decode: received 339 bytes, expected 66203 bytes";
        assert_eq!(classify_rdp_error(raw, true), "frame_decode_failed");
        assert_eq!(
            sanitize_rdp_error(raw, true),
            "The remote server returned an incomplete desktop frame"
        );
        assert_eq!(classify_rdp_error(raw, false), "rdp_failed");
    }

    #[test]
    fn accepts_camel_case_resize_scale_factor() {
        let input: IronRdpInput = serde_json::from_str(
            r#"{"type":"resize","width":1280,"height":720,"scaleFactor":100}"#,
        )
        .expect("resize input should deserialize");
        match input {
            IronRdpInput::Resize {
                width,
                height,
                scale_factor,
            } => {
                assert_eq!((width, height, scale_factor), (1280, 720, 100));
            }
            _ => panic!("unexpected input variant"),
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn staged_file_replaces_existing_file_only_after_completion() {
        let root = std::env::temp_dir().join(format!("xundu-rdp-stage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create staging fixture");
        let destination = root.join("report.txt");
        let staged = root.join("report.part");
        fs::write(&destination, b"old").expect("write old destination");
        fs::write(&staged, b"complete").expect("write staged file");

        commit_staged_file(&staged, &destination).expect("commit staged file");

        assert_eq!(
            fs::read(&destination).expect("read destination"),
            b"complete"
        );
        assert!(!staged.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn staged_file_never_replaces_a_same_named_directory() {
        let root = std::env::temp_dir().join(format!("xundu-rdp-dir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create directory fixture");
        let destination = root.join("assets");
        let staged = root.join("assets.part");
        fs::create_dir_all(&destination).expect("create destination directory");
        fs::write(&staged, b"content").expect("write staged file");

        assert!(commit_staged_file(&staged, &destination).is_err());
        assert!(destination.is_dir());
        assert!(staged.is_file());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn cancelled_transfer_is_detected_before_next_chunk() {
        let cancelled = AtomicBool::new(false);
        assert!(ensure_transfer_active(&cancelled).is_ok());
        cancelled.store(true, Ordering::Relaxed);
        assert_eq!(
            ensure_transfer_active(&cancelled).expect_err("cancel should stop transfer"),
            "文件传输已取消"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn native_file_clipboard_describes_nested_directories() {
        let root = std::env::temp_dir().join(format!("xundu-cliprdr-{}", std::process::id()));
        let nested = root.join("folder");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&nested).expect("create clipboard fixture");
        fs::write(nested.join("hello.txt"), b"hello").expect("write clipboard fixture");

        let descriptors = set_file_clipboard_paths(vec![nested.to_string_lossy().into_owned()])
            .expect("describe clipboard files");
        let progress = file_clipboard_progress();

        assert_eq!(descriptors.len(), 2);
        assert_eq!(descriptors[0].name, "folder");
        assert_eq!(descriptors[1].name, "hello.txt");
        assert_eq!(descriptors[1].relative_path.as_deref(), Some("folder"));
        assert_eq!(progress.total_files, 1);
        assert_eq!(progress.total_bytes, 5);
        clear_file_clipboard();
        let _ = fs::remove_dir_all(root);
    }
}
