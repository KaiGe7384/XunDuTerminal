mod credential_store;
mod iron_rdp;

use credential_store::{
    credential_delete_many, credential_get_many, credential_store_many, credential_vault_status,
};
use iron_rdp::{
    rdp_cancel_file_transfer, rdp_clipboard_file_paths, rdp_clipboard_sequence_number, rdp_connect,
    rdp_disconnect, rdp_file_clipboard_progress, rdp_input, rdp_offer_clipboard_files,
    rdp_upload_files, IronRdpSessions, RdpFileTransfers,
};
use portable_pty::{native_pty_system, Child as PtyChild, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use ssh2::{Channel, CheckResult, KnownHostFileKind, MethodType, Session};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::{
    collections::{hash_map::RandomState, HashMap, HashSet},
    env,
    ffi::{OsStr, OsString},
    fs,
    fs::{File, OpenOptions},
    hash::{BuildHasher, Hash, Hasher},
    io::{Read, Seek, SeekFrom, Write},
    net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Condvar, Mutex, OnceLock,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use sysinfo::{Disks, Networks, ProcessesToUpdate, System};
use tauri::{ipc::Channel as IpcChannel, AppHandle, Emitter, Manager, State};

#[derive(Clone)]
struct SshSessionHandle {
    sender: Sender<SshWorkerCommand>,
    alive: Arc<AtomicBool>,
    stats: Arc<SshSessionStats>,
}

struct SshSessionStats {
    connected_at_ms: u64,
    last_read_ms: AtomicU64,
    last_write_ms: AtomicU64,
    total_read: AtomicUsize,
    total_written: AtomicUsize,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SshAuthProfile {
    host: String,
    user: String,
    port: u16,
    auth_method: String,
    private_key_path: Option<String>,
    password: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportedSshProfile {
    name: String,
    host: String,
    user: String,
    port: u16,
    group: String,
    auth: String,
    private_key_path: Option<String>,
    password: String,
}

#[derive(Default)]
struct SshConfigBlock {
    aliases: Vec<String>,
    host_name: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity_file: Option<String>,
}

static SSH_AUTH_REGISTRY: OnceLock<Mutex<HashMap<String, SshAuthProfile>>> = OnceLock::new();

fn ssh_auth_registry() -> &'static Mutex<HashMap<String, SshAuthProfile>> {
    SSH_AUTH_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ssh_auth_key(host: &str, user: &str, port: u16) -> String {
    format!(
        "{}@{}:{}",
        user.trim(),
        host.trim().to_ascii_lowercase(),
        port
    )
}

fn normalized_auth_method(value: &str) -> &'static str {
    match value.trim().to_ascii_lowercase().as_str() {
        "key" | "privatekey" | "private-key" => "key",
        "agent" | "ssh-agent" => "agent",
        _ => "password",
    }
}

fn validate_ssh_auth_profile(profile: &SshAuthProfile) -> Result<(), String> {
    validate_ssh_part(&profile.host, "host")?;
    validate_ssh_part(&profile.user, "user")?;
    if profile.port == 0 {
        return Err("SSH port must be between 1 and 65535".into());
    }
    match normalized_auth_method(&profile.auth_method) {
        "password" if profile.password.is_empty() => {
            Err("Password is required for SSH password authentication".into())
        }
        "key"
            if profile
                .private_key_path
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty() =>
        {
            Err("A private key path is required for SSH key authentication".into())
        }
        _ => Ok(()),
    }
}

fn register_ssh_auth_profile(profile: SshAuthProfile) -> Result<(), String> {
    validate_ssh_auth_profile(&profile)?;
    let key = ssh_auth_key(&profile.host, &profile.user, profile.port);
    ssh_auth_registry()
        .lock()
        .map_err(|_| "SSH authentication registry is unavailable".to_string())?
        .insert(key, profile);
    Ok(())
}

fn resolve_ssh_auth_profile(host: &str, user: &str, password: &str, port: u16) -> SshAuthProfile {
    let key = ssh_auth_key(host, user, port);
    ssh_auth_registry()
        .lock()
        .ok()
        .and_then(|profiles| profiles.get(&key).cloned())
        .unwrap_or_else(|| SshAuthProfile {
            host: host.to_string(),
            user: user.to_string(),
            port,
            auth_method: "Password".into(),
            private_key_path: None,
            password: password.to_string(),
        })
}

#[tauri::command]
fn ssh_register_auth_profiles(profiles: Vec<SshAuthProfile>) -> Result<(), String> {
    let mut next = HashMap::new();
    for profile in profiles {
        validate_ssh_auth_profile(&profile)?;
        next.insert(
            ssh_auth_key(&profile.host, &profile.user, profile.port),
            profile,
        );
    }
    *ssh_auth_registry()
        .lock()
        .map_err(|_| "SSH authentication registry is unavailable".to_string())? = next;
    Ok(())
}

#[tauri::command]
fn ssh_import_config() -> Result<Vec<ImportedSshProfile>, String> {
    let home = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| "Cannot locate the user profile".to_string())?;
    let config_path = home.join(".ssh").join("config");
    if !config_path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(&config_path)
        .map_err(|error| format!("Failed to read {}: {error}", config_path.display()))?;
    let default_user = env::var("USERNAME")
        .or_else(|_| env::var("USER"))
        .unwrap_or_else(|_| "root".into());
    let mut block = SshConfigBlock::default();
    let mut profiles = Vec::new();

    for raw_line in content.lines() {
        let line = raw_line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let split_at = line.find(char::is_whitespace).unwrap_or(line.len());
        let key = line[..split_at].trim().to_ascii_lowercase();
        let value = line[split_at..].trim().trim_matches(['"', '\'']);
        if key == "host" {
            append_ssh_config_profiles(&mut profiles, &block, &home, &default_user);
            block = SshConfigBlock {
                aliases: value
                    .split_whitespace()
                    .filter(|alias| {
                        !alias.starts_with('!') && !alias.contains('*') && !alias.contains('?')
                    })
                    .map(str::to_string)
                    .collect(),
                ..Default::default()
            };
            continue;
        }
        if block.aliases.is_empty() {
            continue;
        }
        match key.as_str() {
            "hostname" => block.host_name = Some(value.to_string()),
            "user" => block.user = Some(value.to_string()),
            "port" => block.port = value.parse::<u16>().ok().filter(|port| *port > 0),
            "identityfile" if block.identity_file.is_none() => {
                block.identity_file = Some(value.to_string())
            }
            _ => {}
        }
    }
    append_ssh_config_profiles(&mut profiles, &block, &home, &default_user);
    Ok(profiles)
}

fn append_ssh_config_profiles(
    profiles: &mut Vec<ImportedSshProfile>,
    block: &SshConfigBlock,
    home: &Path,
    default_user: &str,
) {
    for alias in &block.aliases {
        let host = block.host_name.as_deref().unwrap_or(alias).trim();
        if host.is_empty() {
            continue;
        }
        let private_key_path = block.identity_file.as_deref().map(|path| {
            let home_text = home.to_string_lossy();
            let expanded = path.replace("%d", &home_text);
            expanded
                .strip_prefix("~/")
                .or_else(|| expanded.strip_prefix("~\\"))
                .map(|relative| home.join(relative).to_string_lossy().into_owned())
                .unwrap_or(expanded)
        });
        profiles.push(ImportedSshProfile {
            name: alias.clone(),
            host: host.to_string(),
            user: block
                .user
                .clone()
                .unwrap_or_else(|| default_user.to_string()),
            port: block.port.unwrap_or(22),
            group: "SSH Config".into(),
            auth: if private_key_path.is_some() {
                "Key"
            } else {
                "Agent"
            }
            .into(),
            private_key_path,
            password: String::new(),
        });
    }
}

#[derive(Clone)]
struct RemoteAuxSessionHandle {
    sender: Sender<RemoteAuxCommand>,
    alive: Arc<AtomicBool>,
}

enum SshWorkerCommand {
    Write(Vec<u8>),
    Resize(u32, u32),
    Disconnect,
}

enum RemoteAuxCommand {
    Exec {
        command: String,
        input: Option<String>,
        response: Sender<Result<String, String>>,
    },
}

#[derive(Default)]
struct SshSessions {
    sessions: Arc<Mutex<HashMap<String, SshSessionHandle>>>,
}

#[derive(Default)]
struct RemoteAuxSessions {
    sessions: Arc<Mutex<HashMap<String, RemoteAuxSessionHandle>>>,
}

#[derive(Default)]
struct FileDownloadTransfers {
    transfers: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

struct SshTunnelProcess {
    child: Child,
    started_at: u64,
    mode: String,
    listen_port: u16,
}

#[derive(Default)]
struct SshTunnelProcesses {
    processes: Arc<Mutex<HashMap<String, SshTunnelProcess>>>,
}

impl Drop for SshTunnelProcesses {
    fn drop(&mut self) {
        if let Ok(mut processes) = self.processes.lock() {
            for (_, process) in processes.iter_mut() {
                let _ = process.child.kill();
                let _ = process.child.wait();
            }
            processes.clear();
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SshTunnelStatus {
    id: String,
    pid: u32,
    started_at: u64,
    mode: String,
    listen_port: u16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SshCommandResult {
    output: String,
    exit_code: i32,
    duration_ms: u64,
    timed_out: bool,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct RemoteDownloadResumeMetadata {
    remote_path: String,
    size: u64,
    modified: u64,
}

#[derive(Default)]
struct LocalShellSessions {
    processes: Arc<Mutex<HashMap<String, LocalShellProcess>>>,
}

#[derive(Default)]
struct LocalStatsCache {
    last: Arc<Mutex<Option<(Instant, LocalSystemStats)>>>,
}

#[derive(Default)]
struct RemoteStatsCache {
    stats: Arc<Mutex<HashMap<String, (Instant, LocalSystemStats)>>>,
}

struct LocalProcessSampler {
    sampler: Arc<Mutex<System>>,
}

impl Default for LocalProcessSampler {
    fn default() -> Self {
        Self {
            sampler: Arc::new(Mutex::new(System::new_all())),
        }
    }
}

struct RemoteAuxLimiter {
    in_flight: Arc<(Mutex<usize>, Condvar)>,
    limit: Arc<Mutex<usize>>,
}

#[derive(Default)]
struct SshConnectLimiter {
    in_flight: Arc<(Mutex<usize>, Condvar)>,
}

struct RemoteAuxPermit {
    in_flight: Arc<(Mutex<usize>, Condvar)>,
}

struct SshConnectPermit {
    in_flight: Arc<(Mutex<usize>, Condvar)>,
}

impl Default for RemoteAuxLimiter {
    fn default() -> Self {
        Self {
            in_flight: Arc::new((Mutex::new(0), Condvar::new())),
            limit: Arc::new(Mutex::new(DEFAULT_REMOTE_AUX_COMMAND_LIMIT)),
        }
    }
}

impl RemoteAuxLimiter {
    fn from_parts(in_flight: Arc<(Mutex<usize>, Condvar)>, limit: Arc<Mutex<usize>>) -> Self {
        Self { in_flight, limit }
    }

    fn acquire(&self) -> Result<RemoteAuxPermit, String> {
        let (lock, cvar) = &*self.in_flight;
        let mut count = lock
            .lock()
            .map_err(|_| "Remote helper queue is poisoned".to_string())?;

        while *count >= self.current_limit()? {
            count = cvar
                .wait(count)
                .map_err(|_| "Remote helper queue is poisoned".to_string())?;
        }

        *count += 1;
        Ok(RemoteAuxPermit {
            in_flight: self.in_flight.clone(),
        })
    }

    fn current_limit(&self) -> Result<usize, String> {
        self.limit
            .lock()
            .map(|limit| *limit)
            .map_err(|_| "Remote helper limit is poisoned".to_string())
    }

    fn set_limit(&self, limit: usize) -> Result<usize, String> {
        let normalized = limit.clamp(MIN_REMOTE_AUX_COMMAND_LIMIT, MAX_REMOTE_AUX_COMMAND_LIMIT);
        let mut current = self
            .limit
            .lock()
            .map_err(|_| "Remote helper limit is poisoned".to_string())?;
        *current = normalized;
        let (_, cvar) = &*self.in_flight;
        cvar.notify_all();
        Ok(normalized)
    }
}

impl Drop for RemoteAuxPermit {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.in_flight;
        if let Ok(mut count) = lock.lock() {
            *count = (*count).saturating_sub(1);
            cvar.notify_one();
        }
    }
}

impl SshConnectLimiter {
    fn acquire_from(in_flight: Arc<(Mutex<usize>, Condvar)>) -> Result<SshConnectPermit, String> {
        let (lock, cvar) = &*in_flight;
        let deadline = Instant::now() + SSH_HANDSHAKE_QUEUE_TIMEOUT;
        let mut count = lock
            .lock()
            .map_err(|_| "SSH connection queue is poisoned".to_string())?;

        while *count >= SSH_HANDSHAKE_CONCURRENCY_LIMIT {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err("SSH connection queue timed out; please retry".to_string());
            };
            let (next_count, wait_result) = cvar
                .wait_timeout(count, remaining)
                .map_err(|_| "SSH connection queue is poisoned".to_string())?;
            count = next_count;
            if wait_result.timed_out() && *count >= SSH_HANDSHAKE_CONCURRENCY_LIMIT {
                return Err("SSH connection queue timed out; please retry".to_string());
            }
        }

        *count += 1;
        drop(count);
        Ok(SshConnectPermit { in_flight })
    }
}

impl Drop for SshConnectPermit {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.in_flight;
        if let Ok(mut count) = lock.lock() {
            *count = (*count).saturating_sub(1);
            cvar.notify_one();
        }
    }
}

#[derive(Clone)]
struct LocalShellProcess {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    child: Arc<Mutex<Box<dyn PtyChild + Send + Sync>>>,
}

#[derive(Serialize, Clone)]
struct SshPayload {
    session_id: String,
    data: String,
}

#[derive(Serialize, Clone)]
struct SshStatusPayload {
    session_id: String,
    message: String,
}

#[derive(Serialize, Clone)]
struct SshHealthPayload {
    session_id: String,
    connected: bool,
    idle_ms: u64,
    write_idle_ms: u64,
    connected_ms: u64,
    total_read: usize,
    total_written: usize,
}

type LocalPayload = SshPayload;
type LocalStatusPayload = SshStatusPayload;
const LOCAL_DIR_ENTRY_LIMIT: usize = 800;
const FILE_EDITOR_MAX_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_REMOTE_AUX_COMMAND_LIMIT: usize = 30;
const MIN_REMOTE_AUX_COMMAND_LIMIT: usize = 1;
const MAX_REMOTE_AUX_COMMAND_LIMIT: usize = 100;
const TERMINAL_EVENT_FLUSH_BYTES: usize = 32 * 1024;
const TERMINAL_EVENT_FLUSH_MS: u64 = 16;
const TERMINAL_INTERACTIVE_FLUSH_BYTES: usize = 512;
const SSH_HANDSHAKE_CONCURRENCY_LIMIT: usize = 2;
const SSH_HANDSHAKE_QUEUE_TIMEOUT: Duration = Duration::from_secs(15);
const SSH_PTY_AUTH_TIMEOUT: Duration = Duration::from_secs(30);
const SSH_CONNECT_RETRY_COUNT: usize = 3;
const SSH_HOST_KEY_PREFERENCE: &str =
    "ssh-ed25519,ecdsa-sha2-nistp256,ecdsa-sha2-nistp384,ecdsa-sha2-nistp521,rsa-sha2-512,rsa-sha2-256,ssh-ed25519-cert-v01@openssh.com,ecdsa-sha2-nistp256-cert-v01@openssh.com,ecdsa-sha2-nistp384-cert-v01@openssh.com,ecdsa-sha2-nistp521-cert-v01@openssh.com,rsa-sha2-512-cert-v01@openssh.com,rsa-sha2-256-cert-v01@openssh.com,ssh-rsa,ssh-rsa-cert-v01@openssh.com";
// OpenSSH's PTY transport stays stable under rapid full-duplex terminal input.
const USE_NATIVE_SSH_CHANNEL: bool = false;
const REMOTE_AUX_EXEC_TIMEOUT: Duration = Duration::from_secs(15);
const REMOTE_AUX_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
const REMOTE_AUX_EXEC_ATTEMPTS: usize = 1;
const REMOTE_AUX_CONNECT_RETRY_COUNT: usize = 1;
const SSH_IO_TIMEOUT_MS: u32 = 12_000;
const DIAGNOSTICS_MAX_BYTES: u64 = 5 * 1024 * 1024;
static DIAGNOSTIC_HASH_STATE: OnceLock<RandomState> = OnceLock::new();
const DEFAULT_APP_UPDATE_MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/KaiGe7384/XunDuTerminal/main/deploy/xunduterminal/latest.json";
const QQ_GROUP_ONE_URL: &str = "mqqapi://card/show_pslcard?src_type=internal&version=1&uin=1090339570&card_type=group&source=qrcode";
const QQ_GROUP_TWO_URL: &str = "mqqapi://card/show_pslcard?src_type=internal&version=1&uin=262430517&card_type=group&source=qrcode";

async fn run_blocking<T, F>(task: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(task)
        .await
        .map_err(|error| format!("Background task failed: {error:?}"))?
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppUpdateManifest {
    version: String,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default, alias = "release_url", alias = "url")]
    release_url: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppUpdateCheck {
    current_version: String,
    latest_version: Option<String>,
    update_available: bool,
    status: String,
    notes: Option<String>,
    release_url: Option<String>,
}

fn unavailable_update_check() -> AppUpdateCheck {
    AppUpdateCheck {
        current_version: env!("CARGO_PKG_VERSION").to_string(),
        latest_version: None,
        update_available: false,
        status: "unavailable".into(),
        notes: None,
        release_url: None,
    }
}

fn parse_update_version(value: &str) -> Result<semver::Version, semver::Error> {
    semver::Version::parse(value.trim().trim_start_matches(['v', 'V']))
}

fn check_app_update_sync() -> Result<AppUpdateCheck, String> {
    let manifest_url =
        option_env!("XUNDU_UPDATE_MANIFEST_URL").unwrap_or(DEFAULT_APP_UPDATE_MANIFEST_URL);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(8))
        .user_agent(concat!("XunDuTerminal/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("无法初始化更新检查：{error}"))?;
    let response = match client.get(manifest_url).send() {
        Ok(response) => response,
        Err(_) => return Ok(unavailable_update_check()),
    };
    if !response.status().is_success() {
        return Ok(unavailable_update_check());
    }
    if response.content_length().unwrap_or_default() > 256 * 1024 {
        return Ok(unavailable_update_check());
    }
    let mut body = Vec::new();
    if response
        .take(256 * 1024 + 1)
        .read_to_end(&mut body)
        .is_err()
        || body.len() > 256 * 1024
    {
        return Ok(unavailable_update_check());
    }
    let manifest = match serde_json::from_slice::<AppUpdateManifest>(&body) {
        Ok(manifest) => manifest,
        Err(_) => return Ok(unavailable_update_check()),
    };
    let current_version = parse_update_version(env!("CARGO_PKG_VERSION"))
        .map_err(|error| format!("当前版本号无效：{error}"))?;
    let latest_version = match parse_update_version(&manifest.version) {
        Ok(version) => version,
        Err(_) => return Ok(unavailable_update_check()),
    };
    let update_available = latest_version > current_version;
    let release_url = manifest
        .release_url
        .filter(|url| is_allowed_source_repository_url(url));

    Ok(AppUpdateCheck {
        current_version: current_version.to_string(),
        latest_version: Some(latest_version.to_string()),
        update_available,
        status: if update_available {
            "available".into()
        } else {
            "current".into()
        },
        notes: manifest.notes.filter(|notes| !notes.trim().is_empty()),
        release_url,
    })
}

#[tauri::command]
async fn check_app_update() -> Result<AppUpdateCheck, String> {
    run_blocking(check_app_update_sync).await
}

fn is_allowed_external_url(url: &str) -> bool {
    let normalized = url.trim().to_ascii_lowercase();
    normalized == "https://xunduyun.com"
        || normalized.starts_with("https://xunduyun.com/")
        || normalized == "https://www.xunduyun.com"
        || normalized.starts_with("https://www.xunduyun.com/")
        || is_allowed_source_repository_url(&normalized)
        || normalized == QQ_GROUP_ONE_URL
        || normalized == QQ_GROUP_TWO_URL
}

fn is_allowed_source_repository_url(url: &str) -> bool {
    let normalized = url.trim().to_ascii_lowercase();
    let Some(path) = normalized.strip_prefix("https://github.com/") else {
        return false;
    };
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if segments.len() < 2
        || segments[0].is_empty()
        || !segments[0]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || segments[1] != "xunduterminal"
    {
        return false;
    }
    matches!(
        segments.as_slice(),
        [_, _]
            | [_, _, "releases"]
            | [_, _, "releases", "latest"]
            | [_, _, "releases", "tag", _, ..]
    )
}

#[tauri::command]
fn open_external_url(url: String) -> Result<(), String> {
    if !is_allowed_external_url(&url) {
        return Err("此外部链接不在允许列表中".into());
    }

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler").arg(url.trim());
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg(url.trim());
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(url.trim());
        command
    };

    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("无法打开浏览器：{error}"))
}

fn diagnostics_path() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join("XunDuTerminal")
        .join("logs")
        .join("xundu-diagnostics.log")
}

fn diag_log(scope: &str, message: impl AsRef<str>) {
    let now = epoch_millis();
    let thread_id = format!("{:?}", thread::current().id());
    let path = diagnostics_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    rotate_diagnostics_if_needed(&path);
    let line = format!(
        "{} [{}] [{}] {}\n",
        now,
        thread_id,
        scope,
        redact_sensitive_text(message.as_ref())
    );
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn rotate_diagnostics_if_needed(path: &Path) {
    if fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        < DIAGNOSTICS_MAX_BYTES
    {
        return;
    }
    let backup = path.with_extension("log.1");
    let _ = fs::remove_file(&backup);
    let _ = fs::rename(path, backup);
}

fn redact_sensitive_text(message: &str) -> String {
    let private_key_markers = [
        "-----BEGIN PRIVATE KEY-----",
        "-----BEGIN OPENSSH PRIVATE KEY-----",
        "-----BEGIN RSA PRIVATE KEY-----",
    ];
    if private_key_markers
        .iter()
        .any(|marker| message.contains(marker))
    {
        return "[REDACTED PRIVATE KEY MATERIAL]".into();
    }

    let mut output = message.to_string();
    for marker in [
        "password=",
        "password:\"",
        "password: \"",
        "passphrase=",
        "token=",
        "token:\"",
        "secret=",
        "secret:\"",
        "clientsecret=",
        "/pass:",
    ] {
        redact_value_after_marker(&mut output, marker);
    }
    for marker in ["target=", "server=", "host=", "key="] {
        pseudonymize_diagnostic_field(&mut output, marker, "ENDPOINT");
    }
    for marker in ["path=", "directory=", "message=", "error="] {
        redact_diagnostic_tail(&mut output, marker);
    }
    redact_ipv4_addresses(&mut output);
    output
}

fn sanitize_diagnostic_content(contents: &str) -> String {
    let mut sanitized = contents
        .lines()
        .map(redact_sensitive_text)
        .collect::<Vec<_>>()
        .join("\n");
    if contents.ends_with('\n') {
        sanitized.push('\n');
    }
    sanitized
}

fn diagnostic_pseudonym(kind: &str, value: &str) -> String {
    let state = DIAGNOSTIC_HASH_STATE.get_or_init(RandomState::new);
    let mut hasher = state.build_hasher();
    value.hash(&mut hasher);
    format!("[{kind}:{:016x}]", hasher.finish())
}

fn find_diagnostic_field(text: &str, marker: &str, search_from: usize) -> Option<usize> {
    let lower = text.to_ascii_lowercase();
    let mut cursor = search_from;
    while cursor < lower.len() {
        let relative_position = lower[cursor..].find(marker)?;
        let position = cursor + relative_position;
        let boundary = position == 0
            || lower[..position]
                .chars()
                .next_back()
                .is_some_and(|character| character.is_whitespace());
        if boundary {
            return Some(position);
        }
        cursor = position + marker.len();
    }
    None
}

fn pseudonymize_diagnostic_field(text: &mut String, marker: &str, kind: &str) {
    let mut search_from = 0;
    loop {
        let Some(position) = find_diagnostic_field(text, marker, search_from) else {
            break;
        };
        let value_start = position + marker.len();
        if value_start >= text.len() {
            break;
        }
        if text[value_start..].starts_with('[') {
            search_from = value_start + 1;
            continue;
        }
        let value_end = text[value_start..]
            .char_indices()
            .find_map(|(offset, character)| {
                (character.is_whitespace()
                    || matches!(character, '"' | '\'' | ',' | ';' | '&' | '}' | ']'))
                .then_some(value_start + offset)
            })
            .unwrap_or(text.len());
        if value_end == value_start {
            search_from = value_start;
            continue;
        }
        let replacement = diagnostic_pseudonym(kind, &text[value_start..value_end]);
        text.replace_range(value_start..value_end, &replacement);
        search_from = value_start + replacement.len();
    }
}

fn redact_diagnostic_tail(text: &mut String, marker: &str) {
    let Some(position) = find_diagnostic_field(text, marker, 0) else {
        return;
    };
    let value_start = position + marker.len();
    if value_start >= text.len() || text[value_start..].starts_with("[REDACTED]") {
        return;
    }
    text.replace_range(value_start.., "[REDACTED]");
}

fn redact_ipv4_addresses(text: &mut String) {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if !bytes[cursor].is_ascii_digit() {
            cursor += 1;
            continue;
        }
        let start = cursor;
        while cursor < bytes.len() && (bytes[cursor].is_ascii_digit() || bytes[cursor] == b'.') {
            cursor += 1;
        }
        let candidate = &text[start..cursor];
        let segments = candidate.split('.').collect::<Vec<_>>();
        if segments.len() == 4
            && segments.iter().all(|segment| {
                !segment.is_empty() && segment.len() <= 3 && segment.parse::<u8>().is_ok()
            })
        {
            ranges.push((start, cursor));
        }
    }
    for (start, end) in ranges.into_iter().rev() {
        text.replace_range(start..end, "[IP]");
    }
}

fn redact_value_after_marker(text: &mut String, marker: &str) {
    let mut search_from = 0;
    loop {
        let lower = text.to_ascii_lowercase();
        let Some(relative_position) = lower[search_from..].find(marker) else {
            break;
        };
        let position = search_from + relative_position;
        let value_start = position + marker.len();
        if value_start >= text.len() {
            break;
        }
        if text[value_start..].starts_with("[REDACTED]") {
            search_from = value_start + "[REDACTED]".len();
            continue;
        }
        let value_end = text[value_start..]
            .char_indices()
            .find_map(|(offset, character)| {
                (character.is_whitespace()
                    || matches!(character, '"' | '\'' | ',' | ';' | '&' | '}' | ']'))
                .then_some(value_start + offset)
            })
            .unwrap_or(text.len());
        if value_end == value_start {
            search_from = value_start;
            continue;
        }
        text.replace_range(value_start..value_end, "[REDACTED]");
        search_from = value_start + "[REDACTED]".len();
    }
}

#[tauri::command]
fn diag_log_frontend(scope: String, message: String) {
    diag_log(&format!("frontend:{scope}"), message);
}

#[tauri::command]
async fn export_diagnostics() -> Result<Option<String>, String> {
    run_blocking(move || {
        let source = diagnostics_path();
        if !source.exists() {
            return Err("No diagnostics have been recorded yet".into());
        }
        let destination = rfd::FileDialog::new()
            .set_title("Export redacted diagnostics")
            .set_file_name("xundu-diagnostics.log")
            .save_file();
        let Some(destination) = destination else {
            return Ok(None);
        };
        let contents = fs::read_to_string(&source)
            .map_err(|error| format!("Failed to read diagnostics: {error}"))?;
        let sanitized = sanitize_diagnostic_content(&contents);
        fs::write(&destination, sanitized)
            .map_err(|error| format!("Failed to export diagnostics: {error}"))?;
        Ok(Some(destination.to_string_lossy().into_owned()))
    })
    .await
}

fn epoch_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

fn build_ssh_health_payload(
    session_id: &str,
    alive: &Arc<AtomicBool>,
    stats: &Arc<SshSessionStats>,
) -> SshHealthPayload {
    let now = epoch_millis();
    let last_read_ms = stats.last_read_ms.load(Ordering::Relaxed);
    let last_write_ms = stats.last_write_ms.load(Ordering::Relaxed);
    SshHealthPayload {
        session_id: session_id.to_string(),
        connected: alive.load(Ordering::SeqCst),
        idle_ms: now.saturating_sub(last_read_ms),
        write_idle_ms: now.saturating_sub(last_write_ms),
        connected_ms: now.saturating_sub(stats.connected_at_ms),
        total_read: stats.total_read.load(Ordering::Relaxed),
        total_written: stats.total_written.load(Ordering::Relaxed),
    }
}

#[derive(Serialize, Deserialize)]
struct LocalFileEntry {
    name: String,
    path: String,
    is_dir: bool,
    size: u64,
    modified: String,
    permissions: String,
    file_type: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct FileDownloadProgress {
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
struct FileDownloadResult {
    destination: String,
    copied_files: usize,
    total_files: usize,
    total_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppBackgroundSelection {
    path: String,
    name: String,
}

struct FileDownloadProgressState {
    total_bytes: u64,
    transferred_bytes: u64,
    copied_files: usize,
    total_files: usize,
    last_sample_at: Instant,
    last_sample_bytes: u64,
    bytes_per_second: u64,
    cancelled: Arc<AtomicBool>,
    on_progress: IpcChannel<FileDownloadProgress>,
}

impl FileDownloadProgressState {
    fn ensure_active(&self) -> Result<(), String> {
        if self.cancelled.load(Ordering::Relaxed) {
            Err("文件下载已取消".to_string())
        } else {
            Ok(())
        }
    }

    fn emit(&mut self, current_file: &str, force: bool, completed: bool) {
        let elapsed = self.last_sample_at.elapsed();
        if !force && elapsed < Duration::from_millis(120) {
            return;
        }
        if elapsed >= Duration::from_millis(20) {
            let bytes = self
                .transferred_bytes
                .saturating_sub(self.last_sample_bytes);
            self.bytes_per_second = (bytes as f64 / elapsed.as_secs_f64()).round() as u64;
            self.last_sample_at = Instant::now();
            self.last_sample_bytes = self.transferred_bytes;
        }
        let _ = self.on_progress.send(FileDownloadProgress {
            total_bytes: self.total_bytes,
            transferred_bytes: self.transferred_bytes,
            bytes_per_second: self.bytes_per_second,
            copied_files: self.copied_files,
            total_files: self.total_files,
            current_file: current_file.to_string(),
            completed,
        });
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct LocalSystemStats {
    user: String,
    home_dir: String,
    os: String,
    shell: String,
    process_count: usize,
    cpu_usage: f32,
    memory_used: u64,
    memory_total: u64,
    disk_used: u64,
    disk_total: u64,
    network_received: u64,
    network_transmitted: u64,
}

#[derive(Serialize, Deserialize, Clone)]
struct SystemProcessEntry {
    pid: u32,
    name: String,
    cpu_usage: f32,
    memory: u64,
    status: String,
    command: String,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
struct CliToolInfo {
    id: String,
    name: String,
    command: String,
}

const CLI_TOOL_CANDIDATES: &[(&str, &str, &[&str])] = &[
    ("claude", "Claude Code", &["claude"]),
    ("codex", "Codex", &["codex"]),
    ("gemini", "Gemini CLI", &["gemini"]),
    ("opencode", "OpenCode", &["opencode"]),
    ("kiro", "Kiro CLI", &["kiro-cli", "kiro"]),
    ("qwen", "Qwen Code", &["qwen"]),
    ("aider", "Aider", &["aider"]),
    ("copilot", "GitHub Copilot", &["copilot"]),
];

#[derive(Serialize, Deserialize)]
struct RemoteListResponse {
    path: String,
    entries: Vec<LocalFileEntry>,
}

#[tauri::command]
async fn local_shell_start(
    app: AppHandle,
    state: State<'_, LocalShellSessions>,
    session_id: String,
    cols: Option<u32>,
    rows: Option<u32>,
) -> Result<(), String> {
    let processes = state.processes.clone();
    run_blocking(move || local_shell_start_sync(app, processes, session_id, cols, rows)).await
}

fn local_shell_start_sync(
    app: AppHandle,
    processes: Arc<Mutex<HashMap<String, LocalShellProcess>>>,
    session_id: String,
    cols: Option<u32>,
    rows: Option<u32>,
) -> Result<(), String> {
    validate_ssh_part(&session_id, "session id")?;
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: rows.unwrap_or(30).clamp(8, u16::MAX as u32) as u16,
            cols: cols.unwrap_or(120).clamp(20, u16::MAX as u32) as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| format!("Failed to open local PTY: {error}"))?;
    let command = default_local_shell_command();
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| format!("Failed to start local shell: {error}"))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| format!("Failed to read local PTY: {error}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| format!("Failed to write local PTY: {error}"))?;

    let child = Arc::new(Mutex::new(child));
    let process = LocalShellProcess {
        writer: Arc::new(Mutex::new(writer)),
        master: Arc::new(Mutex::new(pair.master)),
        child: child.clone(),
    };

    if let Some(previous_process) = processes
        .lock()
        .map_err(|_| "Local shell store is poisoned".to_string())?
        .insert(session_id.clone(), process)
    {
        if let Ok(mut previous_child) = previous_process.child.lock() {
            let _ = previous_child.kill();
            let _ = previous_child.wait();
        }
    }

    spawn_local_reader(app.clone(), session_id.clone(), reader, "local:data");

    let watcher_app = app.clone();
    let watcher_session_id = session_id.clone();
    let watcher_processes = processes;
    thread::spawn(move || {
        let message = loop {
            let status = {
                let mut child = match child.lock() {
                    Ok(child) => child,
                    Err(_) => {
                        let _ = watcher_app.emit(
                            "local:error",
                            LocalStatusPayload {
                                session_id: watcher_session_id.clone(),
                                message: "Local shell lock failed".into(),
                            },
                        );
                        return;
                    }
                };
                child.try_wait()
            };

            match status {
                Ok(Some(status)) => break format!("Local shell exited: {status}"),
                Ok(None) => thread::sleep(Duration::from_millis(120)),
                Err(error) => break format!("Local shell closed: {error}"),
            }
        };

        let _ = watcher_app.emit(
            "local:closed",
            LocalStatusPayload {
                session_id: watcher_session_id.clone(),
                message,
            },
        );

        if let Ok(mut processes) = watcher_processes.lock() {
            let should_remove = processes
                .get(&watcher_session_id)
                .map(|process| Arc::ptr_eq(&process.child, &child))
                .unwrap_or(false);
            if should_remove {
                processes.remove(&watcher_session_id);
            }
        }
    });

    Ok(())
}

#[tauri::command]
fn local_shell_write(
    state: State<LocalShellSessions>,
    session_id: String,
    data: String,
) -> Result<(), String> {
    let process = state
        .processes
        .lock()
        .map_err(|_| "Local shell store is poisoned".to_string())?
        .get(&session_id)
        .cloned()
        .ok_or_else(|| "Local shell is not running".to_string())?;

    let mut writer = process
        .writer
        .lock()
        .map_err(|_| "Local shell stdin lock failed".to_string())?;
    writer
        .write_all(data.as_bytes())
        .map_err(|error| format!("Local shell write failed: {error}"))?;
    writer
        .flush()
        .map_err(|error| format!("Local shell flush failed: {error}"))?;
    Ok(())
}

#[tauri::command]
fn local_shell_resize(
    state: State<LocalShellSessions>,
    session_id: String,
    cols: u32,
    rows: u32,
) -> Result<(), String> {
    let process = state
        .processes
        .lock()
        .map_err(|_| "Local shell store is poisoned".to_string())?
        .get(&session_id)
        .cloned()
        .ok_or_else(|| "Local shell is not running".to_string())?;
    let result = process
        .master
        .lock()
        .map_err(|_| "Local PTY resize lock failed".to_string())?
        .resize(PtySize {
            rows: rows.clamp(8, u16::MAX as u32) as u16,
            cols: cols.clamp(20, u16::MAX as u32) as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| format!("Local PTY resize failed: {error}"));
    result
}

#[tauri::command]
fn local_shell_stop(state: State<LocalShellSessions>, session_id: String) -> Result<(), String> {
    let process = {
        let mut processes = state
            .processes
            .lock()
            .map_err(|_| "Local shell store is poisoned".to_string())?;
        processes.remove(&session_id)
    };

    if let Some(process) = process {
        let mut child = process
            .child
            .lock()
            .map_err(|_| "Local shell lock failed".to_string())?;
        let _ = child.kill();
        let _ = child.wait();
    }

    Ok(())
}

#[tauri::command]
fn local_home_dir() -> Result<String, String> {
    local_shell_home_dir().ok_or_else(|| "Failed to resolve user home directory".to_string())
}

#[tauri::command]
async fn local_detect_cli_tools() -> Result<Vec<CliToolInfo>, String> {
    run_blocking(|| {
        let path = local_command_search_path()
            .ok_or_else(|| "Failed to resolve the local command search path".to_string())?;
        Ok(CLI_TOOL_CANDIDATES
            .iter()
            .filter_map(|(id, name, commands)| {
                commands
                    .iter()
                    .find(|command| command_exists_in_path(command, &path))
                    .map(|command| CliToolInfo {
                        id: (*id).to_string(),
                        name: (*name).to_string(),
                        command: (*command).to_string(),
                    })
            })
            .collect())
    })
    .await
}

fn local_command_search_path() -> Option<OsString> {
    #[cfg(target_os = "windows")]
    {
        refreshed_windows_path().or_else(|| env::var_os("PATH"))
    }

    #[cfg(not(target_os = "windows"))]
    {
        env::var_os("PATH")
    }
}

fn command_exists_in_path(command: &str, path: &OsStr) -> bool {
    env::split_paths(path).any(|directory| {
        #[cfg(target_os = "windows")]
        {
            let extensions =
                env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
            extensions
                .split(';')
                .filter(|extension| !extension.trim().is_empty())
                .any(|extension| directory.join(format!("{command}{extension}")).is_file())
        }

        #[cfg(not(target_os = "windows"))]
        {
            directory.join(command).is_file()
        }
    })
}

fn local_shell_home_dir() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        if let Ok(profile) = std::env::var("USERPROFILE") {
            if !profile.trim().is_empty() {
                return Some(profile);
            }
        }

        match (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
            (Ok(drive), Ok(path)) if !drive.trim().is_empty() && !path.trim().is_empty() => {
                Some(format!("{drive}{path}"))
            }
            _ => None,
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        std::env::var("HOME")
            .ok()
            .filter(|home| !home.trim().is_empty())
    }
}

#[tauri::command]
async fn local_list_dir(path: Option<String>) -> Result<Vec<LocalFileEntry>, String> {
    run_blocking(move || local_list_dir_sync(path)).await
}

#[tauri::command]
async fn local_list_drives() -> Result<Vec<LocalFileEntry>, String> {
    run_blocking(local_list_drives_sync).await
}

fn local_list_drives_sync() -> Result<Vec<LocalFileEntry>, String> {
    let disks = Disks::new_with_refreshed_list();
    let mut seen = HashSet::new();
    let mut entries = disks
        .iter()
        .filter_map(|disk| {
            let path = disk.mount_point().to_string_lossy().to_string();
            if path.trim().is_empty() || !seen.insert(path.to_ascii_lowercase()) {
                return None;
            }
            Some(LocalFileEntry {
                name: path.clone(),
                path,
                is_dir: true,
                size: disk.total_space(),
                modified: "-".into(),
                permissions: "-".into(),
                file_type: "本地磁盘".into(),
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.path.to_lowercase().cmp(&right.path.to_lowercase()));
    Ok(entries)
}

fn local_list_dir_sync(path: Option<String>) -> Result<Vec<LocalFileEntry>, String> {
    let base_path = path
        .filter(|value| !value.trim().is_empty())
        .or_else(local_shell_home_dir)
        .ok_or_else(|| "Failed to resolve directory".to_string())?;
    let directory = PathBuf::from(base_path);
    let mut entries = Vec::with_capacity(LOCAL_DIR_ENTRY_LIMIT.min(256));
    let mut sortable_entries = Vec::with_capacity(LOCAL_DIR_ENTRY_LIMIT.min(256));

    for entry in
        fs::read_dir(&directory).map_err(|error| format!("Failed to read directory: {error}"))?
    {
        if sortable_entries.len() >= LOCAL_DIR_ENTRY_LIMIT {
            break;
        }
        let entry = entry.map_err(|error| format!("Failed to read directory entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("Failed to read file type: {error}"))?;
        let is_dir = file_type.is_dir();
        let metadata = entry.metadata().ok();
        let modified = metadata
            .as_ref()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs().to_string())
            .unwrap_or_else(|| "-".into());
        let name = entry.file_name().to_string_lossy().to_string();
        let permissions = metadata
            .as_ref()
            .map(format_file_permissions)
            .unwrap_or_else(|| "-".into());
        let file_type_label = if is_dir {
            "\u{6587}\u{4ef6}\u{5939}".to_string()
        } else if file_type.is_symlink() {
            "\u{7b26}\u{53f7}\u{94fe}\u{63a5}".to_string()
        } else {
            file_extension_label(&name)
        };
        sortable_entries.push((
            !is_dir,
            name.to_ascii_lowercase(),
            LocalFileEntry {
                name,
                path: entry.path().to_string_lossy().to_string(),
                is_dir,
                size: metadata
                    .as_ref()
                    .map(|metadata| metadata.len())
                    .unwrap_or(0),
                modified,
                permissions,
                file_type: file_type_label,
            },
        ));
    }

    sortable_entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    entries.extend(sortable_entries.into_iter().map(|(_, _, entry)| entry));
    Ok(entries)
}

#[cfg(unix)]
fn format_file_permissions(metadata: &fs::Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode();
    let mut output = String::with_capacity(9);
    for shift in [6, 3, 0] {
        output.push(if mode & (0o4 << shift) != 0 { 'r' } else { '-' });
        output.push(if mode & (0o2 << shift) != 0 { 'w' } else { '-' });
        output.push(if mode & (0o1 << shift) != 0 { 'x' } else { '-' });
    }
    output
}

#[cfg(not(unix))]
fn format_file_permissions(metadata: &fs::Metadata) -> String {
    if metadata.permissions().readonly() {
        "\u{53ea}\u{8bfb}".into()
    } else {
        "\u{8bfb}\u{5199}".into()
    }
}

fn file_extension_label(name: &str) -> String {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| !extension.is_empty())
        .unwrap_or("\u{6587}\u{4ef6}");
    extension.to_ascii_uppercase()
}

#[tauri::command]
async fn local_read_file(path: String) -> Result<String, String> {
    run_blocking(move || local_read_file_sync(path)).await
}

fn local_read_file_sync(path: String) -> Result<String, String> {
    let file_path = PathBuf::from(path);
    let metadata =
        fs::metadata(&file_path).map_err(|error| format!("Failed to read metadata: {error}"))?;
    if metadata.is_dir() {
        return Err("Cannot open a directory as a file".into());
    }
    if metadata.len() > FILE_EDITOR_MAX_BYTES {
        return Err("File is larger than the 64 MB editor limit".into());
    }

    let bytes = fs::read(&file_path).map_err(|error| format!("Failed to read file: {error}"))?;
    String::from_utf8(bytes).map_err(|_| "This file is not valid UTF-8 text".to_string())
}

#[tauri::command]
async fn local_write_file(path: String, content: String) -> Result<(), String> {
    run_blocking(move || local_write_file_sync(path, content)).await
}

fn local_write_file_sync(path: String, content: String) -> Result<(), String> {
    let file_path = PathBuf::from(path);
    if fs::metadata(&file_path)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
    {
        return Err("Cannot write text into a directory".into());
    }

    fs::write(&file_path, content).map_err(|error| format!("Failed to save file: {error}"))
}

#[tauri::command]
async fn choose_file_download_destination(
    suggested_name: String,
    is_dir: bool,
) -> Result<Option<String>, String> {
    run_blocking(move || {
        let suggested_name = safe_file_name(&suggested_name)?;
        let selected = if is_dir {
            rfd::FileDialog::new()
                .set_title("选择下载文件夹")
                .pick_folder()
                .map(|folder| unique_destination(folder.join(suggested_name)))
        } else {
            rfd::FileDialog::new()
                .set_title("保存下载文件")
                .set_file_name(&suggested_name)
                .save_file()
        };
        Ok(selected.map(|path| path.to_string_lossy().into_owned()))
    })
    .await
}

#[tauri::command]
async fn choose_file_upload_sources() -> Result<Vec<String>, String> {
    run_blocking(move || {
        Ok(rfd::FileDialog::new()
            .set_title("选择要上传的文件")
            .pick_files()
            .unwrap_or_default()
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect())
    })
    .await
}

const MAX_APP_BACKGROUND_BYTES: u64 = 64 * 1024 * 1024;

fn validated_background_extension(path: &Path) -> Result<String, String> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp" | "bmp") {
        Ok(extension)
    } else {
        Err("请选择 PNG、JPG、WEBP 或 BMP 图片".to_string())
    }
}

fn app_background_directory(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_local_data_dir()
        .map(|path| path.join("background"))
        .map_err(|error| format!("无法定位背景图片目录: {error}"))
}

#[tauri::command]
async fn choose_app_background(app: AppHandle) -> Result<Option<AppBackgroundSelection>, String> {
    run_blocking(move || {
        let Some(source) = rfd::FileDialog::new()
            .set_title("选择工作台背景图片")
            .add_filter("背景图片", &["png", "jpg", "jpeg", "webp", "bmp"])
            .pick_file()
        else {
            return Ok(None);
        };
        let extension = validated_background_extension(&source)?;
        let metadata = fs::metadata(&source)
            .map_err(|error| format!("无法读取背景图片 {}: {error}", source.display()))?;
        if !metadata.is_file() {
            return Err("请选择普通图片文件".to_string());
        }
        if metadata.len() > MAX_APP_BACKGROUND_BYTES {
            return Err("背景图片不能超过 64 MiB".to_string());
        }
        let original_name = source
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("workspace-background.{extension}"));
        let directory = app_background_directory(&app)?;
        fs::create_dir_all(&directory)
            .map_err(|error| format!("无法创建背景图片目录 {}: {error}", directory.display()))?;
        let revision = epoch_millis();
        let destination = directory.join(format!("workspace-background-{revision}.{extension}"));
        let staged = directory.join(format!(".workspace-background-{revision}.part"));
        fs::copy(&source, &staged)
            .map_err(|error| format!("无法复制背景图片 {}: {error}", source.display()))?;
        if let Err(error) = fs::rename(&staged, &destination) {
            let _ = fs::remove_file(&staged);
            return Err(format!("无法保存背景图片: {error}"));
        }
        if let Ok(entries) = fs::read_dir(&directory) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path != destination && path.is_file() {
                    let _ = fs::remove_file(path);
                }
            }
        }
        Ok(Some(AppBackgroundSelection {
            path: destination.to_string_lossy().into_owned(),
            name: original_name,
        }))
    })
    .await
}

#[tauri::command]
async fn clear_app_background(app: AppHandle) -> Result<(), String> {
    run_blocking(move || {
        let directory = app_background_directory(&app)?;
        if directory.exists() {
            fs::remove_dir_all(&directory).map_err(|error| {
                format!("无法清理背景图片目录 {}: {error}", directory.display())
            })?;
        }
        Ok(())
    })
    .await
}

#[tauri::command]
async fn choose_ssh_private_key() -> Result<Option<String>, String> {
    run_blocking(move || {
        Ok(rfd::FileDialog::new()
            .set_title("Select SSH private key")
            .pick_file()
            .map(|path| path.to_string_lossy().into_owned()))
    })
    .await
}

#[tauri::command]
async fn local_download_path(
    transfers: State<'_, FileDownloadTransfers>,
    transfer_id: String,
    source: String,
    destination: String,
    on_progress: IpcChannel<FileDownloadProgress>,
) -> Result<FileDownloadResult, String> {
    let cancelled = register_file_download(&transfers, &transfer_id)?;
    let transfer_key = transfer_id.trim().to_string();
    let transfer_store = transfers.transfers.clone();
    let result =
        run_blocking(move || local_download_path_sync(source, destination, cancelled, on_progress))
            .await;
    remove_file_download(&transfer_store, &transfer_key);
    result
}

fn local_download_path_sync(
    source: String,
    destination: String,
    cancelled: Arc<AtomicBool>,
    on_progress: IpcChannel<FileDownloadProgress>,
) -> Result<FileDownloadResult, String> {
    let source =
        fs::canonicalize(&source).map_err(|error| format!("无法读取下载源 {source}: {error}"))?;
    let destination = PathBuf::from(destination);
    let destination_matches_source = destination.exists()
        && fs::canonicalize(&destination)
            .map(|path| path == source)
            .unwrap_or(false);
    if source == destination || destination_matches_source {
        return Err("下载目标不能与源文件相同".to_string());
    }
    if source.is_dir() && path_is_within(&destination, &source) {
        return Err("不能把文件夹下载到它自身内部".to_string());
    }

    let mut total_bytes = 0u64;
    let mut total_files = 0usize;
    measure_local_download(&source, &cancelled, &mut total_bytes, &mut total_files)?;
    let mut progress = FileDownloadProgressState {
        total_bytes,
        transferred_bytes: 0,
        copied_files: 0,
        total_files,
        last_sample_at: Instant::now(),
        last_sample_bytes: 0,
        bytes_per_second: 0,
        cancelled,
        on_progress,
    };
    progress.emit("", true, false);
    copy_local_download(&source, &destination, &mut progress)?;
    progress.emit("", true, true);
    Ok(FileDownloadResult {
        destination: destination.to_string_lossy().into_owned(),
        copied_files: progress.copied_files,
        total_files,
        total_bytes,
    })
}

fn measure_local_download(
    source: &Path,
    cancelled: &AtomicBool,
    total_bytes: &mut u64,
    total_files: &mut usize,
) -> Result<(), String> {
    ensure_file_download_active(cancelled)?;
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("无法读取 {}: {error}", source.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("暂不下载符号链接: {}", source.display()));
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(source)
            .map_err(|error| format!("无法读取目录 {}: {error}", source.display()))?
        {
            let entry = entry.map_err(|error| format!("读取目录项失败: {error}"))?;
            measure_local_download(&entry.path(), cancelled, total_bytes, total_files)?;
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

fn copy_local_download(
    source: &Path,
    destination: &Path,
    progress: &mut FileDownloadProgressState,
) -> Result<(), String> {
    progress.ensure_active()?;
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("无法读取 {}: {error}", source.display()))?;
    if metadata.is_dir() {
        fs::create_dir_all(destination)
            .map_err(|error| format!("无法创建目录 {}: {error}", destination.display()))?;
        for entry in fs::read_dir(source)
            .map_err(|error| format!("无法读取目录 {}: {error}", source.display()))?
        {
            let entry = entry.map_err(|error| format!("读取目录项失败: {error}"))?;
            copy_local_download(
                &entry.path(),
                &destination.join(entry.file_name()),
                progress,
            )?;
        }
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("无法创建目录 {}: {error}", parent.display()))?;
    }
    let mut reader =
        File::open(source).map_err(|error| format!("无法打开 {}: {error}", source.display()))?;
    copy_download_reader(
        &mut reader,
        destination,
        &source.to_string_lossy(),
        progress,
    )
}

fn copy_download_reader<R: Read>(
    reader: &mut R,
    destination: &Path,
    current_file: &str,
    progress: &mut FileDownloadProgressState,
) -> Result<(), String> {
    let staged = download_staging_path(destination);
    let mut writer = File::create(&staged)
        .map_err(|error| format!("无法创建临时下载文件 {}: {error}", staged.display()))?;
    let mut buffer = vec![0u8; 512 * 1024];
    let copy_result = (|| {
        loop {
            progress.ensure_active()?;
            let read = reader
                .read(&mut buffer)
                .map_err(|error| format!("读取 {current_file} 失败: {error}"))?;
            if read == 0 {
                break;
            }
            writer
                .write_all(&buffer[..read])
                .map_err(|error| format!("写入 {} 失败: {error}", destination.display()))?;
            progress.transferred_bytes = progress.transferred_bytes.saturating_add(read as u64);
            progress.emit(current_file, false, false);
        }
        writer
            .flush()
            .map_err(|error| format!("刷新下载文件 {} 失败: {error}", destination.display()))?;
        drop(writer);
        progress.ensure_active()?;
        commit_download_file(&staged, destination)
    })();
    if copy_result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    copy_result?;
    progress.copied_files = progress.copied_files.saturating_add(1);
    progress.emit(current_file, true, false);
    Ok(())
}

fn commit_download_file(staged: &Path, destination: &Path) -> Result<(), String> {
    if destination.exists() {
        fs::remove_file(destination)
            .map_err(|error| format!("无法替换 {}: {error}", destination.display()))?;
    }
    fs::rename(staged, destination)
        .map_err(|error| format!("无法完成下载 {}: {error}", destination.display()))
}

fn download_staging_path(destination: &Path) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    destination.with_file_name(format!(".{name}.{nonce}.part"))
}

fn path_is_within(candidate: &Path, parent: &Path) -> bool {
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        env::current_dir().unwrap_or_default().join(candidate)
    };
    absolute.starts_with(parent)
}

fn safe_file_name(value: &str) -> Result<String, String> {
    let name = value
        .trim()
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim();
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err("文件名无效".to_string());
    }
    Ok(name.to_string())
}

fn unique_destination(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    for index in 1..10_000 {
        let candidate = parent.join(format!("{name} ({index})"));
        if !candidate.exists() {
            return candidate;
        }
    }
    parent.join(format!("{name}-{}", epoch_millis()))
}

fn register_file_download(
    transfers: &State<'_, FileDownloadTransfers>,
    transfer_id: &str,
) -> Result<Arc<AtomicBool>, String> {
    let transfer_id = transfer_id.trim();
    if transfer_id.is_empty() {
        return Err("下载任务标识不能为空".to_string());
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    transfers
        .transfers
        .lock()
        .map_err(|_| "下载任务状态不可用".to_string())?
        .insert(transfer_id.to_string(), cancelled.clone());
    Ok(cancelled)
}

fn remove_file_download(
    transfers: &Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    transfer_id: &str,
) {
    if let Ok(mut active) = transfers.lock() {
        active.remove(transfer_id);
    }
}

fn ensure_file_download_active(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        Err("文件下载已取消".to_string())
    } else {
        Ok(())
    }
}

#[tauri::command]
fn cancel_file_download(
    transfers: State<'_, FileDownloadTransfers>,
    transfer_id: String,
) -> Result<(), String> {
    if let Some(cancelled) = transfers
        .transfers
        .lock()
        .map_err(|_| "下载任务状态不可用".to_string())?
        .get(transfer_id.trim())
    {
        cancelled.store(true, Ordering::Relaxed);
    }
    Ok(())
}

#[tauri::command]
async fn local_rename_path(path: String, new_name: String) -> Result<String, String> {
    run_blocking(move || {
        let source = PathBuf::from(&path);
        let new_name = safe_file_name(&new_name)?;
        let destination = source.with_file_name(new_name);
        if destination.exists() {
            return Err("同名文件或文件夹已存在".to_string());
        }
        fs::rename(&source, &destination).map_err(|error| format!("重命名失败: {error}"))?;
        Ok(destination.to_string_lossy().into_owned())
    })
    .await
}

#[tauri::command]
async fn local_delete_path(path: String) -> Result<(), String> {
    run_blocking(move || {
        let target = PathBuf::from(path);
        let metadata = fs::symlink_metadata(&target)
            .map_err(|error| format!("无法读取待删除项目: {error}"))?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            fs::remove_dir_all(&target).map_err(|error| format!("删除文件夹失败: {error}"))
        } else {
            fs::remove_file(&target).map_err(|error| format!("删除文件失败: {error}"))
        }
    })
    .await
}

#[tauri::command]
async fn local_compress_paths(
    paths: Vec<String>,
    destination: Option<String>,
) -> Result<String, String> {
    run_blocking(move || local_compress_paths_sync(paths, destination)).await
}

fn local_compress_paths_sync(
    paths: Vec<String>,
    destination: Option<String>,
) -> Result<String, String> {
    if paths.is_empty() {
        return Err("Select a file or folder before compressing".into());
    }

    let source_paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
    let first = source_paths
        .first()
        .ok_or_else(|| "Select a file or folder before compressing".to_string())?;
    let parent = first
        .parent()
        .ok_or_else(|| "Selected item has no parent directory".to_string())?;
    let archive_path = destination
        .map(PathBuf::from)
        .unwrap_or_else(|| parent.join(format!("{}.zip", path_stem_for_archive(first))));

    let mut command = Command::new("tar");
    command
        .arg("-a")
        .arg("-cf")
        .arg(&archive_path)
        .current_dir(parent);
    for source in &source_paths {
        if source.parent() != Some(parent) {
            return Err("Compress currently supports items from the same directory".into());
        }
        let file_name = source
            .file_name()
            .ok_or_else(|| "Selected item has no file name".to_string())?;
        command.arg(file_name);
    }
    hide_command_window(&mut command);
    let output = command
        .output()
        .map_err(|error| format!("Failed to start compression: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Compression failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(archive_path.to_string_lossy().to_string())
}

#[tauri::command]
async fn local_extract_archive(
    path: String,
    destination: Option<String>,
) -> Result<String, String> {
    run_blocking(move || local_extract_archive_sync(path, destination)).await
}

fn local_extract_archive_sync(path: String, destination: Option<String>) -> Result<String, String> {
    let archive_path = PathBuf::from(path);
    let parent = archive_path
        .parent()
        .ok_or_else(|| "Archive has no parent directory".to_string())?;
    let output_dir = destination
        .map(PathBuf::from)
        .unwrap_or_else(|| parent.join(path_stem_for_archive(&archive_path)));
    fs::create_dir_all(&output_dir)
        .map_err(|error| format!("Failed to create output folder: {error}"))?;

    let mut command = Command::new("tar");
    command
        .arg("-xf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&output_dir);
    hide_command_window(&mut command);
    let output = command
        .output()
        .map_err(|error| format!("Failed to start extraction: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "Extraction failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(output_dir.to_string_lossy().to_string())
}

#[tauri::command]
async fn local_system_stats(cache: State<'_, LocalStatsCache>) -> Result<LocalSystemStats, String> {
    let cache = cache.last.clone();
    run_blocking(move || local_system_stats_sync(cache)).await
}

#[tauri::command]
async fn local_process_list(
    sampler: State<'_, LocalProcessSampler>,
) -> Result<Vec<SystemProcessEntry>, String> {
    let sampler = sampler.sampler.clone();
    run_blocking(move || local_process_list_sync(sampler)).await
}

fn local_process_list_sync(sampler: Arc<Mutex<System>>) -> Result<Vec<SystemProcessEntry>, String> {
    let mut system = sampler
        .lock()
        .map_err(|_| "Local process sampler is poisoned".to_string())?;
    system.refresh_processes(ProcessesToUpdate::All, true);
    thread::sleep(Duration::from_millis(500));
    system.refresh_processes(ProcessesToUpdate::All, true);
    let logical_cpu_count = system.cpus().len().max(1) as f32;
    let mut entries = system
        .processes()
        .iter()
        .map(|(pid, process)| SystemProcessEntry {
            pid: pid.as_u32(),
            name: process.name().to_string_lossy().to_string(),
            cpu_usage: (process.cpu_usage() / logical_cpu_count).clamp(0.0, 100.0),
            memory: process.memory(),
            status: format!("{:?}", process.status()),
            command: process
                .cmd()
                .iter()
                .map(|part| part.to_string_lossy())
                .collect::<Vec<_>>()
                .join(" "),
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        right
            .cpu_usage
            .total_cmp(&left.cpu_usage)
            .then_with(|| right.memory.cmp(&left.memory))
    });
    entries.truncate(600);
    Ok(entries)
}

fn local_system_stats_sync(
    cache: Arc<Mutex<Option<(Instant, LocalSystemStats)>>>,
) -> Result<LocalSystemStats, String> {
    if let Ok(guard) = cache.lock() {
        if let Some((captured_at, stats)) = guard.as_ref() {
            if captured_at.elapsed() < Duration::from_millis(2500) {
                return Ok(stats.clone());
            }
        }
    }

    let home_dir = local_shell_home_dir().unwrap_or_default();
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".into());
    let shell = std::env::var("COMSPEC")
        .or_else(|_| std::env::var("SHELL"))
        .unwrap_or_else(|_| "shell".into());
    let mut system = System::new_all();
    system.refresh_all();
    system.refresh_cpu_all();

    let disks = Disks::new_with_refreshed_list();
    let (disk_total, disk_available) =
        disks
            .iter()
            .fold((0_u64, 0_u64), |(total, available), disk| {
                (
                    total.saturating_add(disk.total_space()),
                    available.saturating_add(disk.available_space()),
                )
            });
    let networks = Networks::new_with_refreshed_list();
    let (network_received, network_transmitted) =
        networks
            .iter()
            .fold((0_u64, 0_u64), |(received, transmitted), (_, data)| {
                (
                    received.saturating_add(data.total_received()),
                    transmitted.saturating_add(data.total_transmitted()),
                )
            });

    let stats = LocalSystemStats {
        user,
        home_dir,
        os: std::env::consts::OS.into(),
        shell,
        process_count: system.processes().len(),
        cpu_usage: system.global_cpu_usage(),
        memory_used: system.used_memory(),
        memory_total: system.total_memory(),
        disk_used: disk_total.saturating_sub(disk_available),
        disk_total,
        network_received,
        network_transmitted,
    };

    if let Ok(mut guard) = cache.lock() {
        *guard = Some((Instant::now(), stats.clone()));
    }

    Ok(stats)
}

fn default_local_shell_command() -> CommandBuilder {
    #[cfg(target_os = "windows")]
    {
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        let mut command = CommandBuilder::new(shell);
        command.args([
            "/D",
            "/Q",
            "/K",
            "chcp 65001>nul & doskey cd=cd /d $* & prompt $P$G",
        ]);
        if let Some(home_dir) = local_shell_home_dir() {
            command.cwd(home_dir);
        }
        if let Some(path) = refreshed_windows_path() {
            command.env("PATH", path);
        }
        configure_terminal_environment(&mut command);
        command
    }

    #[cfg(not(target_os = "windows"))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut command = CommandBuilder::new(shell);
        command.arg("-i");
        if let Some(home_dir) = local_shell_home_dir() {
            command.cwd(home_dir);
        }
        configure_terminal_environment(&mut command);
        command
    }
}

fn configure_terminal_environment(command: &mut CommandBuilder) {
    command.env_remove("NO_COLOR");
    command.env_remove("NODE_DISABLE_COLORS");
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("CLICOLOR", "1");
    command.env("TERM_PROGRAM", "XunDuTerminal");
    command.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
}

#[cfg(target_os = "windows")]
fn refreshed_windows_path() -> Option<OsString> {
    let mut entries = Vec::<PathBuf>::new();
    let mut seen = HashSet::<String>::new();
    if let Some(path) = env::var_os("PATH") {
        append_unique_path_entries(&path, &mut entries, &mut seen);
    }

    if let Some((machine_path, user_path)) = windows_environment_snapshot() {
        if let Some(path) = machine_path {
            append_unique_path_entries(OsStr::new(&path), &mut entries, &mut seen);
        }
        if let Some(path) = user_path {
            append_unique_path_entries(OsStr::new(&path), &mut entries, &mut seen);
        }
    }

    env::join_paths(entries).ok()
}

#[cfg(target_os = "windows")]
fn windows_environment_snapshot() -> Option<(Option<String>, Option<String>)> {
    let powershell = env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    let script = concat!(
        "$ErrorActionPreference='SilentlyContinue';",
        "[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new();",
        "[Console]::WriteLine([Environment]::GetEnvironmentVariable('Path','Machine'));",
        "[Console]::WriteLine([Environment]::GetEnvironmentVariable('Path','User'));"
    );
    let mut command = Command::new(powershell);
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        script,
    ]);
    hide_command_window(&mut command);
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout
        .split('\n')
        .map(|line| line.trim_end_matches('\r').trim());
    let machine_path = lines
        .next()
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let user_path = lines
        .next()
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Some((machine_path, user_path))
}

#[cfg(target_os = "windows")]
fn append_unique_path_entries(
    value: &OsStr,
    entries: &mut Vec<PathBuf>,
    seen: &mut HashSet<String>,
) {
    for path in env::split_paths(value) {
        append_unique_path_entry(path, entries, seen);
    }
}

#[cfg(target_os = "windows")]
fn append_unique_path_entry(path: PathBuf, entries: &mut Vec<PathBuf>, seen: &mut HashSet<String>) {
    if path.as_os_str().is_empty() {
        return;
    }
    let key = path
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_lowercase();
    if seen.insert(key) {
        entries.push(path);
    }
}

fn path_stem_for_archive(path: &PathBuf) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .map(|value| value.to_string_lossy().to_string())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "archive".into())
}

fn hide_command_window(command: &mut Command) {
    #[cfg(target_os = "windows")]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

fn spawn_local_reader<R>(
    app: AppHandle,
    session_id: String,
    mut reader: R,
    event_name: &'static str,
) where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(size) => {
                    let _ = app.emit(
                        event_name,
                        LocalPayload {
                            session_id: session_id.clone(),
                            data: String::from_utf8_lossy(&buffer[..size]).into_owned(),
                        },
                    );
                }
                Err(error) => {
                    let _ = app.emit(
                        "local:error",
                        LocalStatusPayload {
                            session_id: session_id.clone(),
                            message: error.to_string(),
                        },
                    );
                    break;
                }
            }
        }
    });
}

fn terminal_output_should_flush(output: &str, last_flush: &Instant) -> bool {
    output.len() >= TERMINAL_EVENT_FLUSH_BYTES
        || output.len() <= TERMINAL_INTERACTIVE_FLUSH_BYTES
        || last_flush.elapsed() >= Duration::from_millis(TERMINAL_EVENT_FLUSH_MS)
        || terminal_output_has_prompt_tail(output)
}

fn trim_utf8_tail(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }

    let mut keep_from = value.len().saturating_sub(max_bytes);
    while keep_from < value.len() && !value.is_char_boundary(keep_from) {
        keep_from += 1;
    }
    value.drain(..keep_from);
}

fn terminal_output_has_prompt_tail(output: &str) -> bool {
    let tail = output
        .rsplit(|character| character == '\n' || character == '\r')
        .next()
        .unwrap_or(output);
    let clean_tail = strip_terminal_escape_sequences(tail);
    let trimmed = clean_tail.trim_end();
    if trimmed.is_empty() {
        return false;
    }

    matches!(trimmed.chars().last(), Some('#' | '$' | '>' | '%'))
}

fn strip_terminal_escape_sequences(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();

    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            output.push(character);
            continue;
        }

        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                while let Some(sequence_character) = chars.next() {
                    if ('@'..='~').contains(&sequence_character) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                let mut saw_escape = false;
                while let Some(sequence_character) = chars.next() {
                    if sequence_character == '\u{7}' {
                        break;
                    }
                    if saw_escape && sequence_character == '\\' {
                        break;
                    }
                    saw_escape = sequence_character == '\u{1b}';
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }

    output
}

#[tauri::command]
fn ssh_connect(
    app: AppHandle,
    state: State<SshSessions>,
    limiter: State<SshConnectLimiter>,
    session_id: String,
    host: String,
    user: String,
    password: String,
    auth_method: Option<String>,
    private_key_path: Option<String>,
    port: u16,
    cols: u32,
    rows: u32,
) -> Result<(), String> {
    validate_ssh_part(&session_id, "session id")?;
    validate_ssh_part(&host, "host")?;
    validate_ssh_part(&user, "user")?;

    register_ssh_auth_profile(SshAuthProfile {
        host: host.clone(),
        user: user.clone(),
        port,
        auth_method: auth_method.unwrap_or_else(|| "Password".into()),
        private_key_path,
        password: password.clone(),
    })?;

    let sessions = state.sessions.clone();
    let connect_in_flight = limiter.in_flight.clone();
    thread::spawn(move || {
        if let Err(message) = start_ssh_session_with_retry(
            app.clone(),
            sessions,
            connect_in_flight,
            session_id.clone(),
            host,
            user,
            password,
            port,
            cols,
            rows,
        ) {
            let _ = app.emit(
                "ssh:error",
                SshStatusPayload {
                    session_id,
                    message,
                },
            );
        }
    });

    Ok(())
}

fn start_ssh_session_with_retry(
    app: AppHandle,
    sessions: Arc<Mutex<HashMap<String, SshSessionHandle>>>,
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    session_id: String,
    host: String,
    user: String,
    password: String,
    port: u16,
    cols: u32,
    rows: u32,
) -> Result<(), String> {
    let mut last_error = String::new();

    for attempt in 0..SSH_CONNECT_RETRY_COUNT {
        match start_ssh_session(
            app.clone(),
            sessions.clone(),
            connect_in_flight.clone(),
            session_id.clone(),
            host.clone(),
            user.clone(),
            password.clone(),
            port,
            cols,
            rows,
        ) {
            Ok(()) => return Ok(()),
            Err(message) => {
                let retryable = is_retryable_ssh_connect_error(&message);
                last_error = message;
                if !retryable || attempt + 1 >= SSH_CONNECT_RETRY_COUNT {
                    break;
                }
                thread::sleep(Duration::from_millis(900 + (attempt as u64 * 900)));
            }
        }
    }

    Err(last_error)
}

fn start_ssh_session(
    app: AppHandle,
    sessions: Arc<Mutex<HashMap<String, SshSessionHandle>>>,
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    session_id: String,
    host: String,
    user: String,
    password: String,
    port: u16,
    cols: u32,
    rows: u32,
) -> Result<(), String> {
    if USE_NATIVE_SSH_CHANNEL {
        match start_native_ssh_session(
            app.clone(),
            sessions.clone(),
            connect_in_flight.clone(),
            session_id.clone(),
            host.clone(),
            user.clone(),
            password.clone(),
            port,
            cols,
            rows,
        ) {
            Ok(()) => return Ok(()),
            Err(error) => {
                diag_log(
                    "ssh-native",
                    format!(
                        "fallback_to_pty session={session_id} target={user}@{host}:{port} error={error}"
                    ),
                );
            }
        }
    }

    start_ssh_process_session(
        app,
        sessions,
        connect_in_flight,
        session_id,
        host,
        user,
        password,
        port,
        cols,
        rows,
    )
}

fn start_native_ssh_session(
    app: AppHandle,
    sessions: Arc<Mutex<HashMap<String, SshSessionHandle>>>,
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    session_id: String,
    host: String,
    user: String,
    password: String,
    port: u16,
    cols: u32,
    rows: u32,
) -> Result<(), String> {
    let started = Instant::now();
    diag_log(
        "ssh-native",
        format!("start session={session_id} target={user}@{host}:{port} size={cols}x{rows}"),
    );

    let session = {
        let _permit = SshConnectLimiter::acquire_from(connect_in_flight)?;
        connect_interactive_ssh_session(&host, &user, &password, port)?
    };
    let mut channel = session
        .channel_session()
        .map_err(|error| format!("SSH channel failed: {error}"))?;
    channel
        .request_pty(
            "xterm-256color",
            None,
            Some((cols.max(20), rows.max(8), 0, 0)),
        )
        .map_err(|error| format!("SSH PTY request failed: {error}"))?;
    channel
        .shell()
        .map_err(|error| format!("SSH shell request failed: {error}"))?;
    session.set_blocking(false);

    let now = epoch_millis();
    let (command_tx, command_rx) = mpsc::channel();
    let alive = Arc::new(AtomicBool::new(true));
    let stats = Arc::new(SshSessionStats {
        connected_at_ms: now,
        last_read_ms: AtomicU64::new(now),
        last_write_ms: AtomicU64::new(now),
        total_read: AtomicUsize::new(0),
        total_written: AtomicUsize::new(0),
    });
    let handle = SshSessionHandle {
        sender: command_tx,
        alive: alive.clone(),
        stats: stats.clone(),
    };

    if let Some(previous_handle) = sessions
        .lock()
        .map_err(|_| "SSH session store is poisoned".to_string())?
        .insert(session_id.clone(), handle.clone())
    {
        previous_handle.alive.store(false, Ordering::SeqCst);
        let _ = previous_handle.sender.send(SshWorkerCommand::Disconnect);
    }

    let _ = app.emit(
        "ssh:connected",
        SshStatusPayload {
            session_id: session_id.clone(),
            message: "SSH connected".into(),
        },
    );
    diag_log(
        "ssh-native",
        format!(
            "connected event session={session_id} elapsed_ms={}",
            started.elapsed().as_millis()
        ),
    );

    run_native_ssh_worker(
        app, sessions, session_id, session, channel, command_rx, alive, stats,
    );

    Ok(())
}

fn start_ssh_process_session(
    app: AppHandle,
    sessions: Arc<Mutex<HashMap<String, SshSessionHandle>>>,
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    session_id: String,
    host: String,
    user: String,
    password: String,
    port: u16,
    cols: u32,
    rows: u32,
) -> Result<(), String> {
    let started = Instant::now();
    diag_log(
        "ssh-pty",
        format!("start session={session_id} target={user}@{host}:{port} size={cols}x{rows}"),
    );
    let connect_permit = SshConnectLimiter::acquire_from(connect_in_flight)?;
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: rows.max(8) as u16,
            cols: cols.max(20) as u16,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| format!("Failed to open SSH PTY: {error}"))?;

    let auth = resolve_ssh_auth_profile(&host, &user, &password, port);
    let auth_method = normalized_auth_method(&auth.auth_method);
    let mut command = CommandBuilder::new("ssh.exe");
    command.arg("-tt");
    command.arg("-o");
    command.arg("StrictHostKeyChecking=accept-new");
    command.arg("-o");
    command.arg("ServerAliveInterval=30");
    command.arg("-o");
    command.arg("ServerAliveCountMax=120");
    command.arg("-o");
    command.arg("TCPKeepAlive=yes");
    command.arg("-o");
    command.arg("ConnectionAttempts=3");
    command.arg("-o");
    command.arg("IPQoS=throughput");
    command.arg("-o");
    command.arg("ConnectTimeout=10");
    command.arg("-o");
    match auth_method {
        "key" => {
            command.arg("PreferredAuthentications=publickey");
            command.arg("-o");
            command.arg("PubkeyAuthentication=yes");
            command.arg("-o");
            command.arg("IdentitiesOnly=yes");
            command.arg("-i");
            command.arg(auth.private_key_path.as_deref().unwrap_or_default());
        }
        "agent" => {
            command.arg("PreferredAuthentications=publickey");
            command.arg("-o");
            command.arg("PubkeyAuthentication=yes");
            command.arg("-o");
            command.arg("IdentitiesOnly=no");
        }
        _ => {
            command.arg("PreferredAuthentications=password,keyboard-interactive");
            command.arg("-o");
            command.arg("PubkeyAuthentication=no");
        }
    }
    command.arg("-o");
    command.arg("GSSAPIAuthentication=no");
    command.arg("-o");
    command.arg("NumberOfPasswordPrompts=1");
    command.arg("-p");
    command.arg(port.to_string());
    command.arg(format!("{user}@{host}"));

    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| format!("Failed to start OpenSSH: {error}"))?;
    diag_log(
        "ssh-pty",
        format!(
            "spawned session={session_id} target={user}@{host}:{port} elapsed_ms={}",
            started.elapsed().as_millis()
        ),
    );
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| format!("Failed to read SSH PTY: {error}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| format!("Failed to write SSH PTY: {error}"))?;

    let now = epoch_millis();
    let (command_tx, command_rx) = mpsc::channel();
    let alive = Arc::new(AtomicBool::new(true));
    let stats = Arc::new(SshSessionStats {
        connected_at_ms: now,
        last_read_ms: AtomicU64::new(now),
        last_write_ms: AtomicU64::new(now),
        total_read: AtomicUsize::new(0),
        total_written: AtomicUsize::new(0),
    });
    let handle = SshSessionHandle {
        sender: command_tx,
        alive: alive.clone(),
        stats: stats.clone(),
    };

    if let Some(previous_handle) = sessions
        .lock()
        .map_err(|_| "SSH session store is poisoned".to_string())?
        .insert(session_id.clone(), handle.clone())
    {
        previous_handle.alive.store(false, Ordering::SeqCst);
        let _ = previous_handle.sender.send(SshWorkerCommand::Disconnect);
    }

    diag_log("ssh-pty", format!("awaiting_auth session={session_id}"));
    // The limiter protects process startup only. Keeping this permit in the blocking PTY reader
    // can permanently stall every later connection when authentication exits without an EOF.
    drop(connect_permit);

    run_ssh_worker(
        app,
        sessions,
        session_id,
        pair.master,
        child,
        reader,
        writer,
        auth.password.clone(),
        auth_method == "agent" || (auth_method == "key" && auth.password.is_empty()),
        command_rx,
        alive,
        stats,
    );

    Ok(())
}

fn write_native_channel(channel: &mut Channel, data: &[u8]) -> Result<(), String> {
    let mut written = 0_usize;
    let started = Instant::now();
    while written < data.len() {
        match channel.write(&data[written..]) {
            Ok(0) => {
                if started.elapsed() > Duration::from_secs(5) {
                    return Err("SSH write timed out".into());
                }
                thread::sleep(Duration::from_millis(4));
            }
            Ok(size) => {
                written += size;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if started.elapsed() > Duration::from_secs(5) {
                    return Err("SSH write would block for too long".into());
                }
                thread::sleep(Duration::from_millis(4));
            }
            Err(error) => return Err(error.to_string()),
        }
    }

    let flush_started = Instant::now();
    loop {
        match channel.flush() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if flush_started.elapsed() > Duration::from_secs(5) {
                    return Err("SSH flush would block for too long".into());
                }
                thread::sleep(Duration::from_millis(4));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn resize_native_channel(channel: &mut Channel, cols: u32, rows: u32) -> Result<(), String> {
    let started = Instant::now();
    loop {
        match channel.request_pty_size(cols.max(20), rows.max(8), None, None) {
            Ok(()) => return Ok(()),
            Err(error) if error.code() == ssh2::ErrorCode::Session(-37) => {
                if started.elapsed() > Duration::from_secs(2) {
                    return Err("SSH resize would block for too long".into());
                }
                thread::sleep(Duration::from_millis(8));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn with_native_channel<T, F>(
    channel: &Arc<Mutex<Channel>>,
    timeout: Duration,
    action: F,
) -> Result<T, String>
where
    F: FnOnce(&mut Channel) -> Result<T, String>,
{
    let started = Instant::now();
    let mut action = Some(action);
    loop {
        match channel.try_lock() {
            Ok(mut locked_channel) => {
                return action
                    .take()
                    .ok_or_else(|| "SSH channel command was already used".to_string())?(
                    &mut locked_channel,
                );
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                if started.elapsed() >= timeout {
                    return Err("SSH channel is busy".into());
                }
                thread::sleep(Duration::from_millis(4));
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err("SSH channel lock failed".into());
            }
        }
    }
}

fn run_native_ssh_worker(
    app: AppHandle,
    sessions: Arc<Mutex<HashMap<String, SshSessionHandle>>>,
    session_id: String,
    session: Session,
    channel: Channel,
    command_rx: Receiver<SshWorkerCommand>,
    alive: Arc<AtomicBool>,
    stats: Arc<SshSessionStats>,
) {
    let mut close_message = "SSH session closed".to_string();
    let channel = Arc::new(Mutex::new(channel));
    let reader_app = app.clone();
    let reader_session_id = session_id.clone();
    let reader_alive = alive.clone();
    let reader_channel = channel.clone();
    let reader_stats = stats.clone();

    let reader_thread = thread::spawn(move || {
        diag_log(
            "ssh-native-reader",
            format!("start session={reader_session_id}"),
        );
        let mut buffer = [0_u8; 32768];
        let mut output_buffer = String::with_capacity(TERMINAL_EVENT_FLUSH_BYTES);
        let mut last_output_flush = Instant::now();
        let mut total_read = 0_usize;
        let mut flush_count = 0_usize;

        while reader_alive.load(Ordering::SeqCst) {
            let read_result = match reader_channel.try_lock() {
                Ok(mut locked_channel) => {
                    if locked_channel.eof() {
                        break;
                    }
                    locked_channel.read(&mut buffer)
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    thread::sleep(Duration::from_millis(4));
                    continue;
                }
                Err(_) => break,
            };

            match read_result {
                Ok(0) => {
                    thread::sleep(Duration::from_millis(8));
                }
                Ok(size) => {
                    total_read += size;
                    reader_stats.total_read.fetch_add(size, Ordering::Relaxed);
                    reader_stats
                        .last_read_ms
                        .store(epoch_millis(), Ordering::Relaxed);
                    output_buffer.push_str(&String::from_utf8_lossy(&buffer[..size]));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(8));
                }
                Err(error) => {
                    diag_log(
                        "ssh-native-reader",
                        format!("read_error session={reader_session_id} error={error}"),
                    );
                    break;
                }
            }

            if !output_buffer.is_empty()
                && terminal_output_should_flush(&output_buffer, &last_output_flush)
            {
                let data = std::mem::take(&mut output_buffer);
                let bytes = data.len();
                let _ = reader_app.emit(
                    "ssh:data",
                    SshPayload {
                        session_id: reader_session_id.clone(),
                        data,
                    },
                );
                flush_count += 1;
                if flush_count <= 8 || flush_count % 20 == 0 || bytes > 64 * 1024 {
                    diag_log(
                        "ssh-native-reader",
                        format!(
                            "emit session={reader_session_id} bytes={bytes} total_read={total_read} flush_count={flush_count}"
                        ),
                    );
                }
                last_output_flush = Instant::now();
            }
        }

        if !output_buffer.is_empty() {
            let bytes = output_buffer.len();
            let _ = reader_app.emit(
                "ssh:data",
                SshPayload {
                    session_id: reader_session_id.clone(),
                    data: output_buffer,
                },
            );
            diag_log(
                "ssh-native-reader",
                format!(
                    "final_emit session={reader_session_id} bytes={bytes} total_read={total_read}"
                ),
            );
        }
        diag_log(
            "ssh-native-reader",
            format!(
                "exit session={reader_session_id} total_read={total_read} flush_count={flush_count}"
            ),
        );
    });

    let mut last_health_emit = Instant::now();
    let mut last_keepalive = Instant::now();
    loop {
        if !alive.load(Ordering::SeqCst) {
            break;
        }

        match channel.try_lock() {
            Ok(locked_channel) => {
                if locked_channel.eof() {
                    close_message = "SSH channel reached EOF".into();
                    break;
                }
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
            Err(_) => {
                close_message = "SSH channel lock failed".into();
                alive.store(false, Ordering::SeqCst);
                break;
            }
        }

        match command_rx.recv_timeout(Duration::from_millis(80)) {
            Ok(SshWorkerCommand::Write(data)) => {
                if !data.is_empty() {
                    if data.len() > 1024 {
                        diag_log(
                            "ssh-native",
                            format!("write session={session_id} bytes={}", data.len()),
                        );
                    }
                    match with_native_channel(
                        &channel,
                        Duration::from_millis(750),
                        |locked_channel| {
                            write_native_channel(locked_channel, &data)?;
                            let _ = locked_channel.flush();
                            Ok(())
                        },
                    ) {
                        Ok(()) => {
                            stats.total_written.fetch_add(data.len(), Ordering::Relaxed);
                            stats.last_write_ms.store(epoch_millis(), Ordering::Relaxed);
                        }
                        Err(error) => {
                            let _ = app.emit(
                                "ssh:error",
                                SshStatusPayload {
                                    session_id: session_id.clone(),
                                    message: format!("SSH write failed: {error}"),
                                },
                            );
                            alive.store(false, Ordering::SeqCst);
                        }
                    }
                }
            }
            Ok(SshWorkerCommand::Resize(cols, rows)) => {
                diag_log(
                    "ssh-native",
                    format!("resize session={session_id} size={cols}x{rows}"),
                );
                match with_native_channel(&channel, Duration::from_millis(500), |locked_channel| {
                    resize_native_channel(locked_channel, cols, rows)
                }) {
                    Ok(()) => {}
                    Err(error) => {
                        let _ = app.emit(
                            "ssh:error",
                            SshStatusPayload {
                                session_id: session_id.clone(),
                                message: format!("SSH resize failed: {error}"),
                            },
                        );
                    }
                }
            }
            Ok(SshWorkerCommand::Disconnect) => {
                close_message = "SSH session disconnected".into();
                alive.store(false, Ordering::SeqCst);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                alive.store(false, Ordering::SeqCst);
                break;
            }
        }

        if last_keepalive.elapsed() >= Duration::from_secs(10) {
            if let Err(error) = session.keepalive_send() {
                diag_log(
                    "ssh-native",
                    format!("keepalive_error session={session_id} error={error}"),
                );
            }
            last_keepalive = Instant::now();
        }

        if last_health_emit.elapsed() >= Duration::from_secs(2) {
            let payload = build_ssh_health_payload(&session_id, &alive, &stats);
            if payload.idle_ms > 20_000 {
                diag_log(
                    "ssh-health",
                    format!(
                        "session={} alive={} idle_ms={} connected_ms={} read={} written={}",
                        session_id,
                        payload.connected,
                        payload.idle_ms,
                        payload.connected_ms,
                        payload.total_read,
                        payload.total_written
                    ),
                );
            }
            let _ = app.emit("ssh:health", payload);
            last_health_emit = Instant::now();
        }
    }

    let should_emit_closed = alive.swap(false, Ordering::SeqCst);
    if let Ok(mut locked_channel) = channel.lock() {
        let _ = locked_channel.close();
        let _ = locked_channel.wait_close();
    }
    let _ = reader_thread.join();
    drop(session);
    diag_log(
        "ssh-native",
        format!(
            "closed session={session_id} message={close_message} read={} written={}",
            stats.total_read.load(Ordering::Relaxed),
            stats.total_written.load(Ordering::Relaxed)
        ),
    );

    if should_emit_closed {
        let _ = app.emit(
            "ssh:closed",
            SshStatusPayload {
                session_id: session_id.clone(),
                message: close_message.clone(),
            },
        );
    }

    if let Ok(mut sessions) = sessions.lock() {
        let should_remove = sessions
            .get(&session_id)
            .map(|handle| Arc::ptr_eq(&handle.alive, &alive))
            .unwrap_or(false);
        if should_remove {
            sessions.remove(&session_id);
        }
    }
}

fn run_ssh_worker(
    app: AppHandle,
    sessions: Arc<Mutex<HashMap<String, SshSessionHandle>>>,
    session_id: String,
    master: Box<dyn MasterPty + Send>,
    mut child: Box<dyn PtyChild + Send + Sync>,
    mut reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    password: String,
    credential_already_available: bool,
    command_rx: Receiver<SshWorkerCommand>,
    alive: Arc<AtomicBool>,
    stats: Arc<SshSessionStats>,
) {
    let mut close_message = "SSH session closed".to_string();
    let auth_started = Instant::now();
    let authenticated = Arc::new(AtomicBool::new(false));
    let writer = Arc::new(Mutex::new(writer));
    let reader_app = app.clone();
    let reader_session_id = session_id.clone();
    let reader_alive = alive.clone();
    let reader_writer = writer.clone();
    let reader_stats = stats.clone();
    let reader_authenticated = authenticated.clone();

    let reader_thread = thread::spawn(move || {
        diag_log(
            "ssh-pty-reader",
            format!("start session={reader_session_id}"),
        );
        let mut buffer = [0_u8; 16384];
        let mut output_buffer = String::with_capacity(TERMINAL_EVENT_FLUSH_BYTES);
        let mut last_output_flush = Instant::now();
        let mut auth_probe = String::new();
        let mut password_sent = credential_already_available;
        let mut connected_emitted = false;
        let mut total_read = 0_usize;
        let mut flush_count = 0_usize;
        let mut last_reported_slow_write_ms = 0_u64;

        while reader_alive.load(Ordering::SeqCst) {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    diag_log("ssh-pty-reader", format!("eof session={reader_session_id}"));
                    let _ = reader_app.emit(
                        "ssh:closed",
                        SshStatusPayload {
                            session_id: reader_session_id.clone(),
                            message: "SSH PTY stream reached EOF".to_string(),
                        },
                    );
                    reader_alive.store(false, Ordering::SeqCst);
                    break;
                }
                Ok(size) => {
                    let observed_at_ms = epoch_millis();
                    let last_write_ms = reader_stats.last_write_ms.load(Ordering::Relaxed);
                    let echo_candidate_ms = observed_at_ms.saturating_sub(last_write_ms);
                    if size <= TERMINAL_INTERACTIVE_FLUSH_BYTES
                        && reader_stats.total_written.load(Ordering::Relaxed) > 0
                        && last_write_ms > last_reported_slow_write_ms
                        && echo_candidate_ms >= 120
                    {
                        diag_log(
                            "ssh-latency",
                            format!(
                                "slow_echo_candidate session={reader_session_id} bytes={size} elapsed_ms={echo_candidate_ms}"
                            ),
                        );
                        last_reported_slow_write_ms = last_write_ms;
                    }
                    total_read += size;
                    reader_stats.total_read.fetch_add(size, Ordering::Relaxed);
                    reader_stats
                        .last_read_ms
                        .store(epoch_millis(), Ordering::Relaxed);
                    let chunk = String::from_utf8_lossy(&buffer[..size]).to_string();
                    if !password_sent {
                        auth_probe.push_str(&chunk.to_ascii_lowercase());
                        if auth_probe.len() > 4096 {
                            trim_utf8_tail(&mut auth_probe, 2048);
                        }

                        if auth_probe.contains("are you sure you want to continue connecting") {
                            diag_log(
                                "ssh-pty-reader",
                                format!("hostkey prompt session={reader_session_id}"),
                            );
                            if let Ok(mut locked_writer) = reader_writer.lock() {
                                let _ = locked_writer.write_all(b"yes\r");
                                let _ = locked_writer.flush();
                            }
                            auth_probe.clear();
                        } else if auth_probe.contains("password:")
                            || auth_probe.contains("passphrase for key")
                        {
                            diag_log(
                                "ssh-pty-reader",
                                format!("password prompt session={reader_session_id}"),
                            );
                            if let Ok(mut locked_writer) = reader_writer.lock() {
                                let _ = locked_writer.write_all(password.as_bytes());
                                let _ = locked_writer.write_all(b"\r");
                                let _ = locked_writer.flush();
                            }
                            password_sent = true;
                            auth_probe.clear();
                            continue;
                        }
                    }

                    if password_sent
                        && !connected_emitted
                        && ssh_output_reports_authentication_failure(&chunk)
                    {
                        let message =
                            "SSH authentication failed: the server rejected the configured credentials"
                                .to_string();
                        diag_log(
                            "ssh-pty-reader",
                            format!("authentication_failed session={reader_session_id}"),
                        );
                        let _ = reader_app.emit(
                            "ssh:error",
                            SshStatusPayload {
                                session_id: reader_session_id.clone(),
                                message,
                            },
                        );
                        output_buffer.push_str(&chunk);
                        reader_alive.store(false, Ordering::SeqCst);
                        continue;
                    }

                    if ssh_output_confirms_authentication(password_sent, connected_emitted, &chunk)
                    {
                        connected_emitted = true;
                        reader_authenticated.store(true, Ordering::SeqCst);
                        let _ = reader_app.emit(
                            "ssh:connected",
                            SshStatusPayload {
                                session_id: reader_session_id.clone(),
                                message: "SSH connected".into(),
                            },
                        );
                        diag_log(
                            "ssh-pty-reader",
                            format!("authenticated session={reader_session_id}"),
                        );
                    }

                    output_buffer.push_str(&chunk);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(8));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let message = format!("SSH PTY read failed: {error}");
                    diag_log(
                        "ssh-pty-reader",
                        format!("read_error session={reader_session_id} error={error}"),
                    );
                    let _ = reader_app.emit(
                        "ssh:error",
                        SshStatusPayload {
                            session_id: reader_session_id.clone(),
                            message,
                        },
                    );
                    reader_alive.store(false, Ordering::SeqCst);
                    break;
                }
            }

            if !output_buffer.is_empty()
                && terminal_output_should_flush(&output_buffer, &last_output_flush)
            {
                let data = std::mem::take(&mut output_buffer);
                let bytes = data.len();
                let _ = reader_app.emit(
                    "ssh:data",
                    SshPayload {
                        session_id: reader_session_id.clone(),
                        data,
                    },
                );
                flush_count += 1;
                if flush_count <= 8 || flush_count % 20 == 0 || bytes > 64 * 1024 {
                    diag_log(
                        "ssh-pty-reader",
                        format!(
                            "emit session={reader_session_id} bytes={bytes} total_read={total_read} flush_count={flush_count}"
                        ),
                    );
                }
                last_output_flush = Instant::now();
            }
        }

        if !output_buffer.is_empty() {
            let bytes = output_buffer.len();
            let _ = reader_app.emit(
                "ssh:data",
                SshPayload {
                    session_id: reader_session_id.clone(),
                    data: output_buffer,
                },
            );
            diag_log(
                "ssh-pty-reader",
                format!(
                    "final_emit session={reader_session_id} bytes={bytes} total_read={total_read}"
                ),
            );
        }
        diag_log(
            "ssh-pty-reader",
            format!("exit session={reader_session_id} total_read={total_read} flush_count={flush_count}"),
        );
    });

    let mut last_health_emit = Instant::now();
    loop {
        if !alive.load(Ordering::SeqCst) {
            break;
        }

        if let Ok(Some(status)) = child.try_wait() {
            close_message = format!("SSH process exited: {status}");
            break;
        }

        if !authenticated.load(Ordering::SeqCst) && auth_started.elapsed() >= SSH_PTY_AUTH_TIMEOUT {
            close_message = "SSH authentication timed out".to_string();
            let _ = app.emit(
                "ssh:error",
                SshStatusPayload {
                    session_id: session_id.clone(),
                    message: close_message.clone(),
                },
            );
            alive.store(false, Ordering::SeqCst);
            break;
        }

        match command_rx.recv_timeout(Duration::from_millis(80)) {
            Ok(SshWorkerCommand::Write(data)) => {
                if !data.is_empty() {
                    if data.len() > 1024 {
                        diag_log(
                            "ssh-pty",
                            format!("write session={session_id} bytes={}", data.len()),
                        );
                    }
                    match writer.lock() {
                        Ok(mut locked_writer) => {
                            if let Err(error) = locked_writer.write_all(&data) {
                                let _ = app.emit(
                                    "ssh:error",
                                    SshStatusPayload {
                                        session_id: session_id.clone(),
                                        message: format!("SSH write failed: {error}"),
                                    },
                                );
                                alive.store(false, Ordering::SeqCst);
                            } else {
                                let _ = locked_writer.flush();
                                stats.total_written.fetch_add(data.len(), Ordering::Relaxed);
                                stats.last_write_ms.store(epoch_millis(), Ordering::Relaxed);
                            }
                        }
                        Err(_) => {
                            alive.store(false, Ordering::SeqCst);
                        }
                    }
                }
            }
            Ok(SshWorkerCommand::Resize(cols, rows)) => {
                diag_log(
                    "ssh-pty",
                    format!("resize session={session_id} size={cols}x{rows}"),
                );
                if let Err(error) = master.resize(PtySize {
                    rows: rows.max(8) as u16,
                    cols: cols.max(20) as u16,
                    pixel_width: 0,
                    pixel_height: 0,
                }) {
                    let _ = app.emit(
                        "ssh:error",
                        SshStatusPayload {
                            session_id: session_id.clone(),
                            message: format!("SSH resize failed: {error}"),
                        },
                    );
                }
            }
            Ok(SshWorkerCommand::Disconnect) => {
                close_message = "SSH session disconnected".into();
                alive.store(false, Ordering::SeqCst);
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                alive.store(false, Ordering::SeqCst);
                break;
            }
        }

        if last_health_emit.elapsed() >= Duration::from_secs(2) {
            let payload = build_ssh_health_payload(&session_id, &alive, &stats);
            if payload.idle_ms > 20_000 {
                diag_log(
                    "ssh-health",
                    format!(
                        "session={} alive={} idle_ms={} connected_ms={} read={} written={}",
                        session_id,
                        payload.connected,
                        payload.idle_ms,
                        payload.connected_ms,
                        payload.total_read,
                        payload.total_written
                    ),
                );
            }
            let _ = app.emit("ssh:health", payload);
            last_health_emit = Instant::now();
        }
    }

    let should_emit_closed = alive.swap(false, Ordering::SeqCst);
    let _ = child.kill();
    let _ = child.wait();
    if should_emit_closed {
        let _ = app.emit(
            "ssh:closed",
            SshStatusPayload {
                session_id: session_id.clone(),
                message: close_message.clone(),
            },
        );
    }
    drop(writer);
    drop(master);
    let _ = reader_thread.join();
    diag_log(
        "ssh-pty",
        format!(
            "closed session={session_id} message={close_message} read={} written={}",
            stats.total_read.load(Ordering::Relaxed),
            stats.total_written.load(Ordering::Relaxed)
        ),
    );

    if let Ok(mut sessions) = sessions.lock() {
        let should_remove = sessions
            .get(&session_id)
            .map(|handle| Arc::ptr_eq(&handle.alive, &alive))
            .unwrap_or(false);
        if should_remove {
            sessions.remove(&session_id);
        }
    }
}

#[tauri::command]
async fn remote_list_dir(
    aux_sessions: State<'_, RemoteAuxSessions>,
    connect_limiter: State<'_, SshConnectLimiter>,
    limiter: State<'_, RemoteAuxLimiter>,
    host: String,
    user: String,
    password: String,
    port: u16,
    path: Option<String>,
) -> Result<RemoteListResponse, String> {
    let aux_sessions = aux_sessions.sessions.clone();
    let connect_in_flight = connect_limiter.in_flight.clone();
    let in_flight = limiter.in_flight.clone();
    let limit = limiter.limit.clone();
    run_blocking(move || {
        let _permit = RemoteAuxLimiter::from_parts(in_flight, limit).acquire()?;
        remote_list_dir_sync(
            aux_sessions,
            connect_in_flight,
            host,
            user,
            password,
            port,
            path,
        )
    })
    .await
}

fn remote_list_dir_sync(
    aux_sessions: Arc<Mutex<HashMap<String, RemoteAuxSessionHandle>>>,
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    host: String,
    user: String,
    password: String,
    port: u16,
    path: Option<String>,
) -> Result<RemoteListResponse, String> {
    let aux_result = remote_list_dir_via_aux(
        aux_sessions,
        connect_in_flight.clone(),
        host.clone(),
        user.clone(),
        password.clone(),
        port,
        path.clone(),
    );
    match aux_result {
        Ok(response) => return Ok(response),
        Err(error) => {
            diag_log(
                "remote-list-broker",
                format!("fallback_to_sftp target={user}@{host}:{port} error={error}"),
            );
        }
    }

    remote_list_dir_via_sftp(connect_in_flight, host, user, password, port, path)
}

fn remote_list_dir_via_aux(
    aux_sessions: Arc<Mutex<HashMap<String, RemoteAuxSessionHandle>>>,
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    host: String,
    user: String,
    password: String,
    port: u16,
    path: Option<String>,
) -> Result<RemoteListResponse, String> {
    let started = Instant::now();
    let requested_path = path.unwrap_or_else(|| "~".to_string());
    diag_log(
        "remote-list-broker",
        format!("start target={user}@{host}:{port} path={requested_path}"),
    );
    let command = format!(
        "python3 -c {} -- {}",
        shell_single_quote(REMOTE_LIST_DIR_PY),
        shell_single_quote(&requested_path),
    );
    let output = remote_aux_exec_output(
        aux_sessions,
        connect_in_flight,
        host.clone(),
        user.clone(),
        password,
        port,
        command,
        None,
    )?;
    let response = serde_json::from_str::<RemoteListResponse>(&output)
        .map_err(|error| format!("Failed to parse remote directory JSON: {error}"))?;
    diag_log(
        "remote-list-broker",
        format!(
            "done target={user}@{host}:{port} path={} entries={} elapsed_ms={}",
            response.path,
            response.entries.len(),
            started.elapsed().as_millis()
        ),
    );
    Ok(response)
}

fn remote_list_dir_via_sftp(
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    host: String,
    user: String,
    password: String,
    port: u16,
    path: Option<String>,
) -> Result<RemoteListResponse, String> {
    let started = Instant::now();
    diag_log(
        "sftp-list",
        format!(
            "start target={user}@{host}:{port} path={}",
            path.as_deref().unwrap_or("~")
        ),
    );
    let session = connect_limited_ssh_session(connect_in_flight, &host, &user, &password, port)?;
    session.set_blocking(true);
    let sftp = session
        .sftp()
        .map_err(|error| format!("Failed to open SFTP subsystem: {error}"))?;
    let directory = resolve_sftp_path(&sftp, path.as_deref())?;
    let mut entries = Vec::new();

    for (entry_path, stat) in sftp
        .readdir(Path::new(&directory))
        .map_err(|error| format!("Failed to read remote directory: {error}"))?
        .into_iter()
        .take(LOCAL_DIR_ENTRY_LIMIT)
    {
        let name = entry_path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }

        let mode = stat.perm.unwrap_or(0);
        let is_dir = sftp_mode_is_dir(mode);
        let is_link = sftp_mode_is_symlink(mode);
        let extension = Path::new(&name)
            .extension()
            .map(|value| value.to_string_lossy().to_ascii_uppercase())
            .unwrap_or_default();
        let file_type = if is_dir {
            "文件夹".to_string()
        } else if is_link {
            "符号链接".to_string()
        } else if extension.is_empty() {
            "文件".to_string()
        } else {
            extension
        };
        let full_path = join_remote_path(&directory, &name);
        entries.push(LocalFileEntry {
            name,
            path: full_path,
            is_dir,
            size: stat.size.unwrap_or(0),
            modified: stat
                .mtime
                .map(|value| value.to_string())
                .unwrap_or_default(),
            permissions: format_unix_permissions(mode),
            file_type,
        });
    }

    entries.sort_by(|left, right| match right.is_dir.cmp(&left.is_dir) {
        std::cmp::Ordering::Equal => left.name.to_lowercase().cmp(&right.name.to_lowercase()),
        ordering => ordering,
    });

    diag_log(
        "sftp-list",
        format!(
            "done target={user}@{host}:{port} path={directory} entries={} elapsed_ms={}",
            entries.len(),
            started.elapsed().as_millis()
        ),
    );

    Ok(RemoteListResponse {
        path: directory,
        entries,
    })
}

const REMOTE_LIST_DIR_PY: &str = r#"
import json
import os
import stat
import sys

limit = 800
raw_path = sys.argv[-1] if len(sys.argv) > 1 else "~"
directory = os.path.realpath(os.path.expanduser(raw_path or "~"))
entries = []

for name in os.listdir(directory):
    if name in (".", ".."):
        continue
    full_path = os.path.join(directory, name)
    try:
        info = os.lstat(full_path)
    except OSError:
        continue
    mode = info.st_mode
    is_dir = stat.S_ISDIR(mode)
    is_link = stat.S_ISLNK(mode)
    extension = os.path.splitext(name)[1].lstrip(".").upper()
    if is_dir:
        file_type = "文件夹"
    elif is_link:
        file_type = "符号链接"
    elif extension:
        file_type = extension
    else:
        file_type = "文件"
    entries.append({
        "name": name,
        "path": full_path,
        "is_dir": is_dir,
        "size": int(info.st_size),
        "modified": str(int(info.st_mtime)),
        "permissions": stat.filemode(mode),
        "file_type": file_type,
    })

entries.sort(key=lambda item: (not item["is_dir"], item["name"].lower()))
print(json.dumps({"path": directory, "entries": entries[:limit]}, ensure_ascii=False, separators=(",", ":")))
"#;

#[tauri::command]
async fn remote_read_file(
    aux_sessions: State<'_, RemoteAuxSessions>,
    connect_limiter: State<'_, SshConnectLimiter>,
    limiter: State<'_, RemoteAuxLimiter>,
    host: String,
    user: String,
    password: String,
    port: u16,
    path: String,
) -> Result<String, String> {
    let aux_sessions = aux_sessions.sessions.clone();
    let connect_in_flight = connect_limiter.in_flight.clone();
    let in_flight = limiter.in_flight.clone();
    let limit = limiter.limit.clone();
    run_blocking(move || {
        let _permit = RemoteAuxLimiter::from_parts(in_flight, limit).acquire()?;
        remote_read_file_sync(
            aux_sessions,
            connect_in_flight,
            host,
            user,
            password,
            port,
            path,
        )
    })
    .await
}

fn remote_read_file_sync(
    _aux_sessions: Arc<Mutex<HashMap<String, RemoteAuxSessionHandle>>>,
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    host: String,
    user: String,
    password: String,
    port: u16,
    path: String,
) -> Result<String, String> {
    let started = Instant::now();
    diag_log(
        "sftp-read",
        format!("start target={user}@{host}:{port} path={path}"),
    );
    let session = connect_limited_ssh_session(connect_in_flight, &host, &user, &password, port)?;
    session.set_blocking(true);
    let sftp = session
        .sftp()
        .map_err(|error| format!("Failed to open SFTP subsystem: {error}"))?;
    let stat = sftp
        .stat(Path::new(&path))
        .map_err(|error| format!("Failed to stat remote file: {error}"))?;
    if stat.perm.map(sftp_mode_is_dir).unwrap_or(false) {
        return Err("Cannot open a directory as a file".into());
    }
    if stat.size.unwrap_or(0) > FILE_EDITOR_MAX_BYTES {
        return Err("File is larger than the 64 MB editor limit".into());
    }
    let mut file = sftp
        .open(Path::new(&path))
        .map_err(|error| format!("Failed to open remote file: {error}"))?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|error| format!("Failed to read remote file: {error}"))?;
    diag_log(
        "sftp-read",
        format!(
            "done target={user}@{host}:{port} path={path} bytes={} elapsed_ms={}",
            content.len(),
            started.elapsed().as_millis()
        ),
    );
    Ok(content)
}

#[tauri::command]
async fn remote_write_file(
    aux_sessions: State<'_, RemoteAuxSessions>,
    connect_limiter: State<'_, SshConnectLimiter>,
    limiter: State<'_, RemoteAuxLimiter>,
    host: String,
    user: String,
    password: String,
    port: u16,
    path: String,
    content: String,
) -> Result<(), String> {
    let aux_sessions = aux_sessions.sessions.clone();
    let connect_in_flight = connect_limiter.in_flight.clone();
    let in_flight = limiter.in_flight.clone();
    let limit = limiter.limit.clone();
    run_blocking(move || {
        let _permit = RemoteAuxLimiter::from_parts(in_flight, limit).acquire()?;
        remote_write_file_sync(
            aux_sessions,
            connect_in_flight,
            host,
            user,
            password,
            port,
            path,
            content,
        )
    })
    .await
}

fn remote_write_file_sync(
    _aux_sessions: Arc<Mutex<HashMap<String, RemoteAuxSessionHandle>>>,
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    host: String,
    user: String,
    password: String,
    port: u16,
    path: String,
    content: String,
) -> Result<(), String> {
    let started = Instant::now();
    diag_log(
        "sftp-write",
        format!(
            "start target={user}@{host}:{port} path={path} bytes={}",
            content.len()
        ),
    );
    let session = connect_limited_ssh_session(connect_in_flight, &host, &user, &password, port)?;
    session.set_blocking(true);
    let sftp = session
        .sftp()
        .map_err(|error| format!("Failed to open SFTP subsystem: {error}"))?;
    if sftp
        .stat(Path::new(&path))
        .ok()
        .and_then(|stat| stat.perm)
        .map(sftp_mode_is_dir)
        .unwrap_or(false)
    {
        return Err("Cannot write text into a directory".into());
    }
    let mut file = sftp
        .create(Path::new(&path))
        .map_err(|error| format!("Failed to open remote file for writing: {error}"))?;
    file.write_all(content.as_bytes())
        .map_err(|error| format!("Failed to write remote file: {error}"))?;
    diag_log(
        "sftp-write",
        format!(
            "done target={user}@{host}:{port} path={path} elapsed_ms={}",
            started.elapsed().as_millis()
        ),
    );
    Ok(())
}

#[tauri::command]
async fn remote_download_path(
    transfers: State<'_, FileDownloadTransfers>,
    connect_limiter: State<'_, SshConnectLimiter>,
    limiter: State<'_, RemoteAuxLimiter>,
    transfer_id: String,
    host: String,
    user: String,
    password: String,
    port: u16,
    remote_path: String,
    destination: String,
    on_progress: IpcChannel<FileDownloadProgress>,
) -> Result<FileDownloadResult, String> {
    let cancelled = register_file_download(&transfers, &transfer_id)?;
    let transfer_key = transfer_id.trim().to_string();
    let transfer_store = transfers.transfers.clone();
    let connect_in_flight = connect_limiter.in_flight.clone();
    let in_flight = limiter.in_flight.clone();
    let limit = limiter.limit.clone();
    let result = run_blocking(move || {
        let _permit = RemoteAuxLimiter::from_parts(in_flight, limit).acquire()?;
        remote_download_path_sync(
            connect_in_flight,
            host,
            user,
            password,
            port,
            remote_path,
            destination,
            cancelled,
            on_progress,
        )
    })
    .await;
    remove_file_download(&transfer_store, &transfer_key);
    result
}

#[allow(clippy::too_many_arguments)]
fn remote_download_path_sync(
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    host: String,
    user: String,
    password: String,
    port: u16,
    remote_path: String,
    destination: String,
    cancelled: Arc<AtomicBool>,
    on_progress: IpcChannel<FileDownloadProgress>,
) -> Result<FileDownloadResult, String> {
    let started = Instant::now();
    diag_log(
        "sftp-download",
        format!("start target={user}@{host}:{port} path={remote_path}"),
    );
    let session = connect_limited_ssh_session(connect_in_flight, &host, &user, &password, port)?;
    session.set_blocking(true);
    let sftp = session
        .sftp()
        .map_err(|error| format!("无法打开 SFTP 子系统: {error}"))?;
    let remote_path = sftp
        .realpath(Path::new(&remote_path))
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .map_err(|error| format!("无法解析远程路径: {error}"))?;
    let destination = PathBuf::from(destination);
    let mut total_bytes = 0u64;
    let mut total_files = 0usize;
    measure_remote_download(
        &sftp,
        &remote_path,
        &cancelled,
        &mut total_bytes,
        &mut total_files,
    )?;
    let mut progress = FileDownloadProgressState {
        total_bytes,
        transferred_bytes: 0,
        copied_files: 0,
        total_files,
        last_sample_at: Instant::now(),
        last_sample_bytes: 0,
        bytes_per_second: 0,
        cancelled,
        on_progress,
    };
    progress.emit("", true, false);
    copy_remote_download(&sftp, &remote_path, &destination, &mut progress)?;
    progress.emit("", true, true);
    diag_log(
        "sftp-download",
        format!(
            "done target={user}@{host}:{port} path={remote_path} bytes={total_bytes} elapsed_ms={}",
            started.elapsed().as_millis()
        ),
    );
    Ok(FileDownloadResult {
        destination: destination.to_string_lossy().into_owned(),
        copied_files: progress.copied_files,
        total_files,
        total_bytes,
    })
}

fn measure_remote_download(
    sftp: &ssh2::Sftp,
    remote_path: &str,
    cancelled: &AtomicBool,
    total_bytes: &mut u64,
    total_files: &mut usize,
) -> Result<(), String> {
    ensure_file_download_active(cancelled)?;
    let stat = sftp
        .lstat(Path::new(remote_path))
        .map_err(|error| format!("无法读取远程项目 {remote_path}: {error}"))?;
    let mode = stat.perm.unwrap_or(0);
    if sftp_mode_is_symlink(mode) {
        return Err(format!("暂不下载符号链接: {remote_path}"));
    }
    if sftp_mode_is_dir(mode) {
        for (entry_path, entry_stat) in sftp
            .readdir(Path::new(remote_path))
            .map_err(|error| format!("无法读取远程目录 {remote_path}: {error}"))?
        {
            let name = entry_path
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_default();
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }
            if entry_stat.perm.map(sftp_mode_is_symlink).unwrap_or(false) {
                return Err(format!(
                    "暂不下载符号链接: {}",
                    join_remote_path(remote_path, &name)
                ));
            }
            measure_remote_download(
                sftp,
                &join_remote_path(remote_path, &name),
                cancelled,
                total_bytes,
                total_files,
            )?;
        }
        return Ok(());
    }
    *total_bytes = total_bytes.saturating_add(stat.size.unwrap_or(0));
    *total_files = total_files.saturating_add(1);
    Ok(())
}

fn copy_remote_download(
    sftp: &ssh2::Sftp,
    remote_path: &str,
    destination: &Path,
    progress: &mut FileDownloadProgressState,
) -> Result<(), String> {
    progress.ensure_active()?;
    let stat = sftp
        .lstat(Path::new(remote_path))
        .map_err(|error| format!("无法读取远程项目 {remote_path}: {error}"))?;
    let mode = stat.perm.unwrap_or(0);
    if sftp_mode_is_dir(mode) {
        fs::create_dir_all(destination)
            .map_err(|error| format!("无法创建目录 {}: {error}", destination.display()))?;
        for (entry_path, entry_stat) in sftp
            .readdir(Path::new(remote_path))
            .map_err(|error| format!("无法读取远程目录 {remote_path}: {error}"))?
        {
            let name = entry_path
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_default();
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }
            if entry_stat.perm.map(sftp_mode_is_symlink).unwrap_or(false) {
                return Err(format!(
                    "暂不下载符号链接: {}",
                    join_remote_path(remote_path, &name)
                ));
            }
            copy_remote_download(
                sftp,
                &join_remote_path(remote_path, &name),
                &destination.join(&name),
                progress,
            )?;
        }
        return Ok(());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("无法创建目录 {}: {error}", parent.display()))?;
    }
    let mut reader = sftp
        .open(Path::new(remote_path))
        .map_err(|error| format!("无法打开远程文件 {remote_path}: {error}"))?;
    copy_remote_download_reader(
        &mut reader,
        destination,
        remote_path,
        stat.size.unwrap_or(0),
        stat.mtime.unwrap_or(0),
        progress,
    )
}

fn copy_remote_download_reader(
    reader: &mut ssh2::File,
    destination: &Path,
    remote_path: &str,
    remote_size: u64,
    remote_modified: u64,
    progress: &mut FileDownloadProgressState,
) -> Result<(), String> {
    let staged = remote_download_staging_path(destination);
    let metadata_path = staged.with_extension("resume.json");
    let expected_metadata = RemoteDownloadResumeMetadata {
        remote_path: remote_path.to_string(),
        size: remote_size,
        modified: remote_modified,
    };
    let metadata_matches = fs::read_to_string(&metadata_path)
        .ok()
        .and_then(|content| serde_json::from_str::<RemoteDownloadResumeMetadata>(&content).ok())
        .map(|metadata| metadata == expected_metadata)
        .unwrap_or(false);
    if !metadata_matches {
        let _ = fs::remove_file(&staged);
        let metadata = serde_json::to_vec(&expected_metadata)
            .map_err(|error| format!("Failed to serialize download resume metadata: {error}"))?;
        atomic_write_file(&metadata_path, &metadata)?;
    }

    let staged_bytes = fs::metadata(&staged)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    if staged_bytes > remote_size {
        fs::remove_file(&staged)
            .map_err(|error| format!("Failed to reset invalid partial download: {error}"))?;
    }
    let resumed_bytes = if staged_bytes <= remote_size {
        staged_bytes
    } else {
        0
    };
    if resumed_bytes > 0 {
        reader
            .seek(SeekFrom::Start(resumed_bytes))
            .map_err(|error| format!("Failed to resume remote file {remote_path}: {error}"))?;
        progress.transferred_bytes = progress.transferred_bytes.saturating_add(resumed_bytes);
        progress.emit(remote_path, false, false);
    }

    let mut writer = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&staged)
        .map_err(|error| {
            format!(
                "Failed to open resumable download {}: {error}",
                staged.display()
            )
        })?;
    let mut buffer = vec![0u8; 512 * 1024];
    loop {
        progress.ensure_active()?;
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("Failed to read {remote_path}: {error}"))?;
        if read == 0 {
            break;
        }
        writer
            .write_all(&buffer[..read])
            .map_err(|error| format!("Failed to write {}: {error}", destination.display()))?;
        progress.transferred_bytes = progress.transferred_bytes.saturating_add(read as u64);
        progress.emit(remote_path, false, false);
    }
    writer
        .flush()
        .map_err(|error| format!("Failed to flush {}: {error}", destination.display()))?;
    drop(writer);
    progress.ensure_active()?;
    if fs::metadata(&staged)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
        != remote_size
    {
        return Err(format!(
            "Downloaded size does not match remote file {remote_path}"
        ));
    }
    commit_download_file(&staged, destination)?;
    let _ = fs::remove_file(metadata_path);
    progress.copied_files = progress.copied_files.saturating_add(1);
    progress.emit(remote_path, true, false);
    Ok(())
}

fn remote_download_staging_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("download");
    destination.with_file_name(format!(".{name}.xundu.part"))
}

#[tauri::command]
async fn remote_upload_paths(
    transfers: State<'_, FileDownloadTransfers>,
    connect_limiter: State<'_, SshConnectLimiter>,
    limiter: State<'_, RemoteAuxLimiter>,
    transfer_id: String,
    host: String,
    user: String,
    password: String,
    port: u16,
    sources: Vec<String>,
    remote_directory: String,
    on_progress: IpcChannel<FileDownloadProgress>,
) -> Result<FileDownloadResult, String> {
    let cancelled = register_file_download(&transfers, &transfer_id)?;
    let transfer_key = transfer_id.trim().to_string();
    let transfer_store = transfers.transfers.clone();
    let connect_in_flight = connect_limiter.in_flight.clone();
    let in_flight = limiter.in_flight.clone();
    let limit = limiter.limit.clone();
    let result = run_blocking(move || {
        let _permit = RemoteAuxLimiter::from_parts(in_flight, limit).acquire()?;
        remote_upload_paths_sync(
            connect_in_flight,
            host,
            user,
            password,
            port,
            sources,
            remote_directory,
            cancelled,
            on_progress,
        )
    })
    .await;
    remove_file_download(&transfer_store, &transfer_key);
    result
}

#[allow(clippy::too_many_arguments)]
fn remote_upload_paths_sync(
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    host: String,
    user: String,
    password: String,
    port: u16,
    sources: Vec<String>,
    remote_directory: String,
    cancelled: Arc<AtomicBool>,
    on_progress: IpcChannel<FileDownloadProgress>,
) -> Result<FileDownloadResult, String> {
    if sources.is_empty() {
        return Err("请选择要上传的文件".to_string());
    }
    let started = Instant::now();
    diag_log(
        "sftp-upload",
        format!(
            "start target={user}@{host}:{port} directory={remote_directory} files={}",
            sources.len()
        ),
    );
    let mut local_sources = Vec::with_capacity(sources.len());
    let mut source_names = HashSet::with_capacity(sources.len());
    let mut total_bytes = 0u64;
    let mut total_files = 0usize;
    for source in sources {
        ensure_file_download_active(&cancelled)?;
        let source_path = PathBuf::from(&source);
        let source_metadata = fs::symlink_metadata(&source_path)
            .map_err(|error| format!("无法读取上传文件 {source}: {error}"))?;
        if source_metadata.file_type().is_symlink() {
            return Err(format!("暂不上传符号链接: {}", source_path.display()));
        }
        let path = fs::canonicalize(&source_path)
            .map_err(|error| format!("无法读取上传文件 {source}: {error}"))?;
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| format!("无法解析上传文件名: {}", path.display()))?;
        let name = safe_file_name(name)?;
        if !source_names.insert(name.clone()) {
            return Err(format!("上传项目包含同名文件或文件夹: {name}"));
        }
        measure_local_upload(&path, &cancelled, &mut total_bytes, &mut total_files)?;
        local_sources.push(path);
    }

    let session = connect_limited_ssh_session(connect_in_flight, &host, &user, &password, port)?;
    session.set_blocking(true);
    let sftp = session
        .sftp()
        .map_err(|error| format!("无法打开 SFTP 子系统: {error}"))?;
    let remote_directory = resolve_sftp_path(&sftp, Some(&remote_directory))?;
    for source in &local_sources {
        let name = local_upload_name(source)?;
        let destination = join_remote_path(&remote_directory, &name);
        if sftp.lstat(Path::new(&destination)).is_ok() {
            return Err(format!("远程目录已存在同名文件或文件夹: {destination}"));
        }
    }
    let mut progress = FileDownloadProgressState {
        total_bytes,
        transferred_bytes: 0,
        copied_files: 0,
        total_files,
        last_sample_at: Instant::now(),
        last_sample_bytes: 0,
        bytes_per_second: 0,
        cancelled,
        on_progress,
    };
    progress.emit("", true, false);

    for source in &local_sources {
        upload_local_path(&sftp, source, &remote_directory, &mut progress)?;
    }
    progress.emit("", true, true);
    diag_log(
        "sftp-upload",
        format!(
            "done target={user}@{host}:{port} directory={remote_directory} files={} bytes={total_bytes} elapsed_ms={}",
            progress.copied_files,
            started.elapsed().as_millis()
        ),
    );
    Ok(FileDownloadResult {
        destination: remote_directory,
        copied_files: progress.copied_files,
        total_files,
        total_bytes,
    })
}

fn local_upload_name(source: &Path) -> Result<String, String> {
    let name = source
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("无法解析上传文件名: {}", source.display()))?;
    safe_file_name(name)
}

fn measure_local_upload(
    source: &Path,
    cancelled: &AtomicBool,
    total_bytes: &mut u64,
    total_files: &mut usize,
) -> Result<(), String> {
    ensure_file_download_active(cancelled)?;
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("无法读取上传项目 {}: {error}", source.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("暂不上传符号链接: {}", source.display()));
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(source)
            .map_err(|error| format!("无法读取上传目录 {}: {error}", source.display()))?
        {
            let entry = entry.map_err(|error| format!("读取上传目录项失败: {error}"))?;
            local_upload_name(&entry.path())?;
            measure_local_upload(&entry.path(), cancelled, total_bytes, total_files)?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        return Err(format!("不支持的上传文件类型: {}", source.display()));
    }
    *total_bytes = total_bytes.saturating_add(metadata.len());
    *total_files = total_files.saturating_add(1);
    Ok(())
}

fn upload_local_path(
    sftp: &ssh2::Sftp,
    source: &Path,
    remote_directory: &str,
    progress: &mut FileDownloadProgressState,
) -> Result<(), String> {
    progress.ensure_active()?;
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| format!("无法读取上传项目 {}: {error}", source.display()))?;
    if metadata.is_dir() {
        upload_local_directory(sftp, source, remote_directory, progress)
    } else if metadata.is_file() {
        upload_local_file(sftp, source, remote_directory, progress)
    } else {
        Err(format!("不支持的上传文件类型: {}", source.display()))
    }
}

fn upload_local_directory(
    sftp: &ssh2::Sftp,
    source: &Path,
    remote_directory: &str,
    progress: &mut FileDownloadProgressState,
) -> Result<(), String> {
    let name = local_upload_name(source)?;
    let destination = join_remote_path(remote_directory, &name);
    if sftp.lstat(Path::new(&destination)).is_ok() {
        return Err(format!("远程目录已存在同名文件或文件夹: {destination}"));
    }
    let staged = join_remote_path(
        remote_directory,
        &format!(".{name}.xundu-upload-{}.part", epoch_millis()),
    );
    sftp.mkdir(Path::new(&staged), 0o755)
        .map_err(|error| format!("无法创建远程临时目录 {staged}: {error}"))?;
    let upload_result = (|| {
        upload_local_directory_contents(sftp, source, &staged, progress)?;
        progress.ensure_active()?;
        if sftp.lstat(Path::new(&destination)).is_ok() {
            return Err(format!("远程目录已存在同名文件或文件夹: {destination}"));
        }
        sftp.rename(Path::new(&staged), Path::new(&destination), None)
            .map_err(|error| format!("无法完成远程文件夹上传 {destination}: {error}"))
    })();
    if upload_result.is_err() {
        let _ = delete_remote_path_recursive(sftp, &staged);
    }
    upload_result
}

fn upload_local_directory_contents(
    sftp: &ssh2::Sftp,
    source: &Path,
    remote_directory: &str,
    progress: &mut FileDownloadProgressState,
) -> Result<(), String> {
    progress.ensure_active()?;
    for entry in fs::read_dir(source)
        .map_err(|error| format!("无法读取上传目录 {}: {error}", source.display()))?
    {
        progress.ensure_active()?;
        let entry = entry.map_err(|error| format!("读取上传目录项失败: {error}"))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("无法读取上传项目 {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("暂不上传符号链接: {}", path.display()));
        }
        if metadata.is_dir() {
            let name = local_upload_name(&path)?;
            let destination = join_remote_path(remote_directory, &name);
            sftp.mkdir(Path::new(&destination), 0o755)
                .map_err(|error| format!("无法创建远程目录 {destination}: {error}"))?;
            upload_local_directory_contents(sftp, &path, &destination, progress)?;
        } else if metadata.is_file() {
            upload_local_file(sftp, &path, remote_directory, progress)?;
        } else {
            return Err(format!("不支持的上传文件类型: {}", path.display()));
        }
    }
    Ok(())
}

fn upload_local_file(
    sftp: &ssh2::Sftp,
    source: &Path,
    remote_directory: &str,
    progress: &mut FileDownloadProgressState,
) -> Result<(), String> {
    progress.ensure_active()?;
    let name = source
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("无法解析上传文件名: {}", source.display()))?;
    let destination = join_remote_path(remote_directory, name);
    if sftp.lstat(Path::new(&destination)).is_ok() {
        return Err(format!("远程目录已存在同名文件: {destination}"));
    }
    let staged = join_remote_path(
        remote_directory,
        &format!(".{name}.xundu-upload-{}.part", epoch_millis()),
    );
    let mut reader = File::open(source)
        .map_err(|error| format!("无法打开上传文件 {}: {error}", source.display()))?;
    let mut writer = sftp
        .create(Path::new(&staged))
        .map_err(|error| format!("无法创建远程临时文件 {staged}: {error}"))?;
    let mut buffer = vec![0u8; 512 * 1024];
    let upload_result = (|| {
        loop {
            progress.ensure_active()?;
            let read = reader
                .read(&mut buffer)
                .map_err(|error| format!("读取上传文件 {} 失败: {error}", source.display()))?;
            if read == 0 {
                break;
            }
            writer
                .write_all(&buffer[..read])
                .map_err(|error| format!("写入远程文件 {destination} 失败: {error}"))?;
            progress.transferred_bytes = progress.transferred_bytes.saturating_add(read as u64);
            progress.emit(name, false, false);
        }
        writer
            .flush()
            .map_err(|error| format!("刷新远程文件 {destination} 失败: {error}"))?;
        drop(writer);
        progress.ensure_active()?;
        if sftp.lstat(Path::new(&destination)).is_ok() {
            return Err(format!("远程目录已存在同名文件: {destination}"));
        }
        sftp.rename(Path::new(&staged), Path::new(&destination), None)
            .map_err(|error| format!("无法完成远程上传 {destination}: {error}"))
    })();
    if upload_result.is_err() {
        let _ = sftp.unlink(Path::new(&staged));
    }
    upload_result?;
    progress.copied_files = progress.copied_files.saturating_add(1);
    progress.emit(name, true, false);
    Ok(())
}

#[tauri::command]
async fn remote_rename_path(
    connect_limiter: State<'_, SshConnectLimiter>,
    limiter: State<'_, RemoteAuxLimiter>,
    host: String,
    user: String,
    password: String,
    port: u16,
    path: String,
    new_name: String,
) -> Result<String, String> {
    let connect_in_flight = connect_limiter.in_flight.clone();
    let in_flight = limiter.in_flight.clone();
    let limit = limiter.limit.clone();
    run_blocking(move || {
        let _permit = RemoteAuxLimiter::from_parts(in_flight, limit).acquire()?;
        let new_name = safe_file_name(&new_name)?;
        let parent = path
            .trim_end_matches('/')
            .rsplit_once('/')
            .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
            .ok_or_else(|| "无法解析远程父目录".to_string())?;
        let destination = join_remote_path(parent, &new_name);
        let session =
            connect_limited_ssh_session(connect_in_flight, &host, &user, &password, port)?;
        session.set_blocking(true);
        let sftp = session
            .sftp()
            .map_err(|error| format!("无法打开 SFTP 子系统: {error}"))?;
        if sftp.stat(Path::new(&destination)).is_ok() {
            return Err("同名文件或文件夹已存在".to_string());
        }
        sftp.rename(Path::new(&path), Path::new(&destination), None)
            .map_err(|error| format!("远程重命名失败: {error}"))?;
        Ok(destination)
    })
    .await
}

#[tauri::command]
async fn remote_delete_path(
    connect_limiter: State<'_, SshConnectLimiter>,
    limiter: State<'_, RemoteAuxLimiter>,
    host: String,
    user: String,
    password: String,
    port: u16,
    path: String,
) -> Result<(), String> {
    let connect_in_flight = connect_limiter.in_flight.clone();
    let in_flight = limiter.in_flight.clone();
    let limit = limiter.limit.clone();
    run_blocking(move || {
        let _permit = RemoteAuxLimiter::from_parts(in_flight, limit).acquire()?;
        let session =
            connect_limited_ssh_session(connect_in_flight, &host, &user, &password, port)?;
        session.set_blocking(true);
        let sftp = session
            .sftp()
            .map_err(|error| format!("无法打开 SFTP 子系统: {error}"))?;
        delete_remote_path_recursive(&sftp, &path)
    })
    .await
}

fn delete_remote_path_recursive(sftp: &ssh2::Sftp, path: &str) -> Result<(), String> {
    let stat = sftp
        .lstat(Path::new(path))
        .map_err(|error| format!("无法读取远程项目 {path}: {error}"))?;
    let mode = stat.perm.unwrap_or(0);
    if sftp_mode_is_dir(mode) && !sftp_mode_is_symlink(mode) {
        for (entry_path, _) in sftp
            .readdir(Path::new(path))
            .map_err(|error| format!("无法读取远程目录 {path}: {error}"))?
        {
            let name = entry_path
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_default();
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }
            delete_remote_path_recursive(sftp, &join_remote_path(path, &name))?;
        }
        sftp.rmdir(Path::new(path))
            .map_err(|error| format!("删除远程文件夹失败: {error}"))
    } else {
        sftp.unlink(Path::new(path))
            .map_err(|error| format!("删除远程文件失败: {error}"))
    }
}

#[tauri::command]
async fn remote_system_stats(
    cache: State<'_, RemoteStatsCache>,
    aux_sessions: State<'_, RemoteAuxSessions>,
    connect_limiter: State<'_, SshConnectLimiter>,
    limiter: State<'_, RemoteAuxLimiter>,
    host: String,
    user: String,
    password: String,
    port: u16,
) -> Result<LocalSystemStats, String> {
    let cache = cache.stats.clone();
    let aux_sessions = aux_sessions.sessions.clone();
    let connect_in_flight = connect_limiter.in_flight.clone();
    let in_flight = limiter.in_flight.clone();
    let limit = limiter.limit.clone();
    run_blocking(move || {
        remote_system_stats_cached(
            cache,
            aux_sessions,
            connect_in_flight,
            in_flight,
            limit,
            host,
            user,
            password,
            port,
        )
    })
    .await
}

#[tauri::command]
async fn remote_process_list(
    aux_sessions: State<'_, RemoteAuxSessions>,
    connect_limiter: State<'_, SshConnectLimiter>,
    limiter: State<'_, RemoteAuxLimiter>,
    host: String,
    user: String,
    password: String,
    port: u16,
) -> Result<Vec<SystemProcessEntry>, String> {
    let aux_sessions = aux_sessions.sessions.clone();
    let connect_in_flight = connect_limiter.in_flight.clone();
    let in_flight = limiter.in_flight.clone();
    let limit = limiter.limit.clone();
    run_blocking(move || {
        let _permit = RemoteAuxLimiter::from_parts(in_flight, limit).acquire()?;
        let command = r#"sh -lc 'cores=$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || echo 1); case "$cores" in ""|*[!0-9]*|0) cores=1;; esac; ps -eo pid=,pcpu=,rss=,stat=,comm=,args= --sort=-pcpu 2>/dev/null | head -n 600 | awk -v cores="$cores" '\''{ pid=$1; cpu=$2/cores; if (cpu > 100) cpu=100; rss=$3; stat=$4; name=$5; $1=$2=$3=$4=$5=""; sub(/^[[:space:]]+/, "", $0); printf "%s\t%.2f\t%s\t%s\t%s\t%s\n", pid, cpu, rss, stat, name, $0 }'\'''"#.to_string();
        let output = remote_aux_exec_output(
            aux_sessions,
            connect_in_flight,
            host,
            user,
            password,
            port,
            command,
            None,
        )?;
        Ok(parse_remote_process_list(&output))
    })
    .await
}

#[tauri::command]
async fn remote_detect_cli_tools(
    aux_sessions: State<'_, RemoteAuxSessions>,
    connect_limiter: State<'_, SshConnectLimiter>,
    limiter: State<'_, RemoteAuxLimiter>,
    host: String,
    user: String,
    password: String,
    port: u16,
) -> Result<Vec<CliToolInfo>, String> {
    let aux_sessions = aux_sessions.sessions.clone();
    let connect_in_flight = connect_limiter.in_flight.clone();
    let in_flight = limiter.in_flight.clone();
    let limit = limiter.limit.clone();
    run_blocking(move || {
        let _permit = RemoteAuxLimiter::from_parts(in_flight, limit).acquire()?;
        let command = r#"sh -lc 'for candidate in claude codex gemini opencode kiro-cli kiro qwen aider copilot; do if command -v "$candidate" >/dev/null 2>&1; then printf "%s\n" "$candidate"; fi; done'"#.to_string();
        let output = remote_aux_exec_output(
            aux_sessions,
            connect_in_flight,
            host,
            user,
            password,
            port,
            command,
            None,
        )?;
        Ok(parse_detected_cli_tools(&output))
    })
    .await
}

fn parse_detected_cli_tools(output: &str) -> Vec<CliToolInfo> {
    let detected: HashSet<&str> = output.lines().map(str::trim).collect();
    CLI_TOOL_CANDIDATES
        .iter()
        .filter_map(|(id, name, commands)| {
            commands
                .iter()
                .find(|command| detected.contains(**command))
                .map(|command| CliToolInfo {
                    id: (*id).to_string(),
                    name: (*name).to_string(),
                    command: (*command).to_string(),
                })
        })
        .collect()
}

fn parse_remote_process_list(output: &str) -> Vec<SystemProcessEntry> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(6, '\t');
            let pid = fields.next()?.trim().parse::<u32>().ok()?;
            let cpu_usage = fields.next()?.trim().parse::<f32>().unwrap_or_default();
            let memory = fields
                .next()?
                .trim()
                .parse::<u64>()
                .unwrap_or_default()
                .saturating_mul(1024);
            let status = fields.next()?.trim().to_string();
            let name = fields.next()?.trim().to_string();
            let command = fields.next().unwrap_or_default().trim().to_string();
            Some(SystemProcessEntry {
                pid,
                name,
                cpu_usage,
                memory,
                status,
                command,
            })
        })
        .collect()
}

fn remote_system_stats_cached(
    cache: Arc<Mutex<HashMap<String, (Instant, LocalSystemStats)>>>,
    aux_sessions: Arc<Mutex<HashMap<String, RemoteAuxSessionHandle>>>,
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    in_flight: Arc<(Mutex<usize>, Condvar)>,
    limit: Arc<Mutex<usize>>,
    host: String,
    user: String,
    password: String,
    port: u16,
) -> Result<LocalSystemStats, String> {
    let cache_key = format!("{user}@{host}:{port}");
    if let Ok(guard) = cache.lock() {
        if let Some((captured_at, stats)) = guard.get(&cache_key) {
            if captured_at.elapsed() < Duration::from_millis(10_000) {
                return Ok(stats.clone());
            }
        }
    }

    let _permit = RemoteAuxLimiter::from_parts(in_flight, limit).acquire()?;
    if let Ok(guard) = cache.lock() {
        if let Some((captured_at, stats)) = guard.get(&cache_key) {
            if captured_at.elapsed() < Duration::from_millis(10_000) {
                return Ok(stats.clone());
            }
        }
    }

    let stats =
        remote_system_stats_sync(aux_sessions, connect_in_flight, host, user, password, port)?;
    if let Ok(mut guard) = cache.lock() {
        guard.insert(cache_key, (Instant::now(), stats.clone()));
    }
    Ok(stats)
}

fn remote_system_stats_sync(
    aux_sessions: Arc<Mutex<HashMap<String, RemoteAuxSessionHandle>>>,
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    host: String,
    user: String,
    password: String,
    port: u16,
) -> Result<LocalSystemStats, String> {
    let started = Instant::now();
    diag_log("remote-stats", format!("start target={user}@{host}:{port}"));
    let command = r#"sh -lc '
printf "__XUNDU_USER__\n%s\n" "${USER:-$(id -un 2>/dev/null)}"
printf "__XUNDU_HOME__\n%s\n" "${HOME:-}"
printf "__XUNDU_SHELL__\n%s\n" "${SHELL:-}"
printf "__XUNDU_OS__\n"; uname -srvmo 2>/dev/null || uname -a 2>/dev/null || true
printf "__XUNDU_CORES__\n"; getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || echo 1
printf "__XUNDU_LOADAVG__\n"; cat /proc/loadavg 2>/dev/null || true
printf "__XUNDU_MEMINFO__\n"; cat /proc/meminfo 2>/dev/null || true
printf "__XUNDU_DF__\n"; df -B1 / 2>/dev/null | tail -n 1 || true
printf "__XUNDU_NETDEV__\n"; cat /proc/net/dev 2>/dev/null || true
printf "__XUNDU_PROCS__\n"; find /proc -maxdepth 1 -type d -name "[0-9]*" 2>/dev/null | wc -l
'"#
    .to_string();
    let output = remote_aux_exec_output(
        aux_sessions,
        connect_in_flight,
        host,
        user,
        password,
        port,
        command,
        None,
    )?;
    let stats = parse_remote_stats_snapshot(&output)?;
    diag_log(
        "remote-stats",
        format!(
            "done target={}:{} output_bytes={} elapsed_ms={}",
            stats.user,
            port,
            output.len(),
            started.elapsed().as_millis()
        ),
    );
    Ok(stats)
}

fn parse_remote_stats_snapshot(output: &str) -> Result<LocalSystemStats, String> {
    let user = first_section_line(output, "__XUNDU_USER__").unwrap_or_default();
    let home_dir = first_section_line(output, "__XUNDU_HOME__").unwrap_or_default();
    let shell = first_section_line(output, "__XUNDU_SHELL__").unwrap_or_default();
    let os = first_section_line(output, "__XUNDU_OS__").unwrap_or_default();
    let cores = first_section_line(output, "__XUNDU_CORES__")
        .and_then(|value| value.trim().parse::<f32>().ok())
        .filter(|value| *value > 0.0)
        .unwrap_or(1.0);
    let load = first_section_line(output, "__XUNDU_LOADAVG__")
        .and_then(|value| value.split_whitespace().next()?.parse::<f32>().ok())
        .unwrap_or(0.0);
    let meminfo = section(output, "__XUNDU_MEMINFO__");
    let memory_total = parse_meminfo_value(meminfo, "MemTotal").unwrap_or(0);
    let memory_available = parse_meminfo_value(meminfo, "MemAvailable").unwrap_or(0);
    let (disk_used, disk_total) = parse_df_line(
        first_section_line(output, "__XUNDU_DF__")
            .as_deref()
            .unwrap_or_default(),
    );
    let (network_received, network_transmitted) = parse_netdev(section(output, "__XUNDU_NETDEV__"));
    let process_count = first_section_line(output, "__XUNDU_PROCS__")
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);

    if memory_total == 0 && disk_total == 0 && process_count == 0 {
        return Err("Failed to parse remote stats snapshot".into());
    }

    Ok(LocalSystemStats {
        user,
        home_dir,
        os,
        shell,
        process_count,
        cpu_usage: (load / cores * 100.0).clamp(0.0, 100.0),
        memory_used: memory_total.saturating_sub(memory_available),
        memory_total,
        disk_used,
        disk_total,
        network_received,
        network_transmitted,
    })
}

fn section<'a>(output: &'a str, marker: &str) -> &'a str {
    let Some(start_index) = output.find(marker) else {
        return "";
    };
    let start = start_index + marker.len();
    let rest = output[start..].trim_start_matches(['\r', '\n']);
    let end = rest.find("\n__XUNDU_").unwrap_or(rest.len());
    rest[..end].trim()
}

fn first_section_line(output: &str, marker: &str) -> Option<String> {
    section(output, marker)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToString::to_string)
}

fn parse_meminfo_value(meminfo: &str, key: &str) -> Option<u64> {
    meminfo.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name != key {
            return None;
        }
        value
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
            .map(|kb| kb * 1024)
    })
}

fn parse_df_line(line: &str) -> (u64, u64) {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 4 {
        return (0, 0);
    }
    let total = parts
        .get(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let used = parts
        .get(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    (used, total)
}

fn parse_netdev(netdev: &str) -> (u64, u64) {
    netdev
        .lines()
        .filter_map(|line| {
            let (name, values) = line.split_once(':')?;
            if name.trim() == "lo" {
                return None;
            }
            let parts: Vec<&str> = values.split_whitespace().collect();
            Some((
                parts.first()?.parse::<u64>().ok()?,
                parts.get(8)?.parse::<u64>().ok()?,
            ))
        })
        .fold((0, 0), |total, value| {
            (total.0 + value.0, total.1 + value.1)
        })
}

#[tauri::command]
fn ssh_write(state: State<SshSessions>, session_id: String, data: String) -> Result<(), String> {
    let handle = get_session_handle(&state.sessions, &session_id)?;
    handle
        .sender
        .send(SshWorkerCommand::Write(data.into_bytes()))
        .map_err(|_| "SSH worker is not running".to_string())?;
    Ok(())
}

#[tauri::command]
fn ssh_session_health(
    state: State<SshSessions>,
    session_id: String,
) -> Result<SshHealthPayload, String> {
    let handle = get_session_handle(&state.sessions, &session_id)?;
    Ok(build_ssh_health_payload(
        &session_id,
        &handle.alive,
        &handle.stats,
    ))
}

#[tauri::command]
fn ssh_resize(
    state: State<SshSessions>,
    session_id: String,
    cols: u32,
    rows: u32,
) -> Result<(), String> {
    let handle = get_session_handle(&state.sessions, &session_id)?;
    handle
        .sender
        .send(SshWorkerCommand::Resize(cols, rows))
        .map_err(|_| "SSH worker is not running".to_string())?;
    Ok(())
}

#[tauri::command]
fn ssh_disconnect(state: State<SshSessions>, session_id: String) -> Result<(), String> {
    let handle = {
        let mut sessions = state
            .sessions
            .lock()
            .map_err(|_| "SSH session store is poisoned".to_string())?;
        sessions.remove(&session_id)
    };

    if let Some(handle) = handle {
        handle.alive.store(false, Ordering::SeqCst);
        let _ = handle.sender.send(SshWorkerCommand::Disconnect);
    }

    Ok(())
}

fn get_session_handle(
    sessions: &Arc<Mutex<HashMap<String, SshSessionHandle>>>,
    session_id: &str,
) -> Result<SshSessionHandle, String> {
    sessions
        .lock()
        .map_err(|_| "SSH session store is poisoned".to_string())?
        .get(session_id)
        .filter(|handle| handle.alive.load(Ordering::SeqCst))
        .cloned()
        .ok_or_else(|| "SSH session is not connected".to_string())
}

fn is_retryable_ssh_connect_error(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase();
    normalized.contains("failed getting banner")
        || normalized.contains("connection reset")
        || normalized.contains("connection refused")
        || normalized.contains("connection timed out")
        || normalized.contains("tcp connect failed")
        || normalized.contains("handshake failed")
}

fn ssh_output_confirms_authentication(
    password_sent: bool,
    connected_emitted: bool,
    output: &str,
) -> bool {
    if !password_sent || connected_emitted || output.trim().is_empty() {
        return false;
    }

    let normalized = output.to_ascii_lowercase();
    !ssh_output_reports_authentication_failure(&normalized)
        && !normalized.contains("connection closed")
        && !normalized.contains("connection reset")
        && !normalized.contains("connection timed out")
}

fn ssh_output_reports_authentication_failure(output: &str) -> bool {
    let normalized = output.to_ascii_lowercase();
    normalized.contains("permission denied")
        || normalized.contains("authentication failed")
        || normalized.contains("too many authentication failures")
}

fn connect_interactive_ssh_session(
    host: &str,
    user: &str,
    password: &str,
    port: u16,
) -> Result<Session, String> {
    let session = open_ssh_session(host, port)?;
    verify_or_store_known_host(&session, host, port)?;

    let auth = resolve_ssh_auth_profile(host, user, password, port);
    match normalized_auth_method(&auth.auth_method) {
        "key" => {
            let path = auth
                .private_key_path
                .as_deref()
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .ok_or_else(|| "SSH private key path is missing".to_string())?;
            session
                .userauth_pubkey_file(
                    user,
                    None,
                    Path::new(path),
                    (!auth.password.is_empty()).then_some(auth.password.as_str()),
                )
                .map_err(|error| format!("SSH private key authentication failed: {error}"))?;
        }
        "agent" => session
            .userauth_agent(user)
            .map_err(|error| format!("SSH Agent authentication failed: {error}"))?,
        _ => session
            .userauth_password(user, &auth.password)
            .map_err(|error| format!("SSH password authentication failed: {error}"))?,
    }

    if !session.authenticated() {
        return Err("SSH authentication was rejected".into());
    }
    session.set_keepalive(true, 10);

    Ok(session)
}

fn resolve_ssh_socket_addresses(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    let socket_host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    let addresses = (socket_host, port)
        .to_socket_addrs()
        .map_err(|error| format!("Failed to resolve SSH host: {error}"))?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        Err("Failed to resolve SSH host".to_string())
    } else {
        Ok(addresses)
    }
}

fn open_ssh_session_with_host_key_preference(
    host: &str,
    port: u16,
    host_key_preference: Option<&str>,
) -> Result<Session, String> {
    let addresses = resolve_ssh_socket_addresses(host, port)?;
    let mut last_error = String::new();

    for socket_address in addresses {
        let tcp = match TcpStream::connect_timeout(&socket_address, Duration::from_secs(10)) {
            Ok(tcp) => tcp,
            Err(error) => {
                last_error = format!("TCP connect failed for {socket_address}: {error}");
                continue;
            }
        };
        tcp.set_nodelay(true)
            .map_err(|error| format!("Failed to configure TCP nodelay: {error}"))?;
        tcp.set_read_timeout(Some(Duration::from_millis(SSH_IO_TIMEOUT_MS.into())))
            .map_err(|error| format!("Failed to configure TCP read timeout: {error}"))?;
        tcp.set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|error| format!("Failed to configure TCP write timeout: {error}"))?;

        let mut session =
            ssh2::Session::new().map_err(|error| format!("SSH session failed: {error}"))?;
        session.set_timeout(SSH_IO_TIMEOUT_MS);
        if let Some(preference) = host_key_preference {
            session
                .method_pref(MethodType::HostKey, preference)
                .map_err(|error| format!("Failed to configure SSH host key algorithms: {error}"))?;
        }
        session.set_tcp_stream(tcp);
        match session.handshake() {
            Ok(()) => return Ok(session),
            Err(error) => {
                last_error = format!("SSH handshake failed for {socket_address}: {error}");
            }
        }
    }

    Err(if last_error.is_empty() {
        "SSH connection failed".to_string()
    } else {
        last_error
    })
}

fn open_ssh_session(host: &str, port: u16) -> Result<Session, String> {
    match open_ssh_session_with_host_key_preference(host, port, Some(SSH_HOST_KEY_PREFERENCE)) {
        Ok(session) => Ok(session),
        Err(preferred_error)
            if preferred_error
                .to_ascii_lowercase()
                .contains("exchange encryption keys") =>
        {
            diag_log(
                "ssh-connect",
                format!("compatibility_retry target={host}:{port} error={preferred_error}"),
            );
            open_ssh_session_with_host_key_preference(host, port, None).map_err(|fallback_error| {
                format!("SSH handshake failed after compatibility retry: {fallback_error}")
            })
        }
        Err(error) => Err(error),
    }
}

fn verify_or_store_known_host(session: &Session, host: &str, port: u16) -> Result<(), String> {
    let _known_hosts_guard = known_hosts_file_lock()
        .lock()
        .map_err(|_| "known_hosts lock is poisoned".to_string())?;
    let (host_key, host_key_type) = session
        .host_key()
        .ok_or_else(|| "SSH server did not provide a host key".to_string())?;
    let home = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| "Cannot locate the user profile for known_hosts".to_string())?;
    let ssh_dir = home.join(".ssh");
    let known_hosts_path = ssh_dir.join("known_hosts");
    let mut known_hosts = session
        .known_hosts()
        .map_err(|error| format!("Failed to initialize known_hosts: {error}"))?;
    if known_hosts_path.exists() {
        known_hosts
            .read_file(&known_hosts_path, KnownHostFileKind::OpenSSH)
            .map_err(|error| format!("Failed to read known_hosts: {error}"))?;
    }

    match known_hosts.check_port(host, port, host_key) {
        CheckResult::Match => Ok(()),
        CheckResult::Mismatch => Err(format!(
            "SSH host key mismatch for {host}:{port}; the connection was blocked ({})",
            ssh_host_fingerprint(session)
        )),
        CheckResult::Failure => Err(format!("Failed to verify SSH host key for {host}:{port}")),
        CheckResult::NotFound => {
            fs::create_dir_all(&ssh_dir).map_err(|error| {
                format!("Failed to create SSH configuration directory: {error}")
            })?;
            let known_host_name = if port == 22 {
                host.to_string()
            } else {
                format!("[{host}]:{port}")
            };
            known_hosts
                .add(
                    &known_host_name,
                    host_key,
                    "XunDuTerminal accept-new",
                    host_key_type.into(),
                )
                .map_err(|error| format!("Failed to trust the SSH host key: {error}"))?;
            known_hosts
                .write_file(&known_hosts_path, KnownHostFileKind::OpenSSH)
                .map_err(|error| format!("Failed to update known_hosts: {error}"))?;
            Ok(())
        }
    }
}

fn known_hosts_file_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[tauri::command]
async fn ssh_replace_known_host(
    connect_limiter: State<'_, SshConnectLimiter>,
    host: String,
    port: u16,
    expected_fingerprint: String,
) -> Result<String, String> {
    validate_ssh_part(&host, "host")?;
    let connect_in_flight = connect_limiter.in_flight.clone();
    run_blocking(move || {
        let _permit = SshConnectLimiter::acquire_from(connect_in_flight)?;
        let session = open_ssh_session(&host, port)?;
        let fingerprint = ssh_host_fingerprint(&session);
        if fingerprint == "fingerprint unavailable"
            || !fingerprint.eq_ignore_ascii_case(expected_fingerprint.trim())
        {
            return Err(format!(
                "SSH host key changed again during confirmation for {host}:{port}; no record was updated"
            ));
        }
        replace_known_host_record(&session, &host, port)?;
        diag_log(
            "known-hosts",
            format!("replaced target={host}:{port} fingerprint={fingerprint}"),
        );
        Ok(fingerprint)
    })
    .await
}

fn replace_known_host_record(session: &Session, host: &str, port: u16) -> Result<(), String> {
    let _known_hosts_guard = known_hosts_file_lock()
        .lock()
        .map_err(|_| "known_hosts lock is poisoned".to_string())?;
    let (host_key, host_key_type) = session
        .host_key()
        .ok_or_else(|| "SSH server did not provide a host key".to_string())?;
    let home = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| "Cannot locate the user profile for known_hosts".to_string())?;
    let ssh_dir = home.join(".ssh");
    let known_hosts_path = ssh_dir.join("known_hosts");
    let mut known_hosts = session
        .known_hosts()
        .map_err(|error| format!("Failed to initialize known_hosts: {error}"))?;
    if known_hosts_path.exists() {
        known_hosts
            .read_file(&known_hosts_path, KnownHostFileKind::OpenSSH)
            .map_err(|error| format!("Failed to read known_hosts: {error}"))?;
    }

    let known_host_name = if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    };
    for entry in known_hosts
        .hosts()
        .map_err(|error| format!("Failed to inspect known_hosts: {error}"))?
    {
        let should_remove = entry
            .name()
            .is_some_and(|name| known_host_entry_matches_target(name, host, port));
        if should_remove {
            known_hosts
                .remove(&entry)
                .map_err(|error| format!("Failed to remove the old SSH host key: {error}"))?;
        }
    }

    fs::create_dir_all(&ssh_dir)
        .map_err(|error| format!("Failed to create SSH configuration directory: {error}"))?;
    known_hosts
        .add(
            &known_host_name,
            host_key,
            "XunDuTerminal confirmed replacement",
            host_key_type.into(),
        )
        .map_err(|error| format!("Failed to store the new SSH host key: {error}"))?;
    known_hosts
        .write_file(&known_hosts_path, KnownHostFileKind::OpenSSH)
        .map_err(|error| format!("Failed to update known_hosts: {error}"))
}

fn known_host_entry_matches_target(entry_name: &str, host: &str, port: u16) -> bool {
    if port == 22 {
        entry_name == host || entry_name == format!("[{host}]:22")
    } else {
        entry_name == format!("[{host}]:{port}")
    }
}

fn ssh_host_fingerprint(session: &Session) -> String {
    session
        .host_key_hash(ssh2::HashType::Sha256)
        .map(|bytes| {
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(":")
        })
        .unwrap_or_else(|| "fingerprint unavailable".into())
}

fn connect_limited_ssh_session(
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    host: &str,
    user: &str,
    password: &str,
    port: u16,
) -> Result<Session, String> {
    let mut last_error = String::new();

    for attempt in 0..SSH_CONNECT_RETRY_COUNT {
        let started = Instant::now();
        diag_log(
            "ssh-connect",
            format!("start target={user}@{host}:{port} attempt={}", attempt + 1),
        );
        let result = {
            let _permit = SshConnectLimiter::acquire_from(connect_in_flight.clone())?;
            connect_interactive_ssh_session(host, user, password, port)
        };

        match result {
            Ok(session) => {
                diag_log(
                    "ssh-connect",
                    format!(
                        "done target={user}@{host}:{port} attempt={} elapsed_ms={}",
                        attempt + 1,
                        started.elapsed().as_millis()
                    ),
                );
                return Ok(session);
            }
            Err(message) => {
                diag_log(
                    "ssh-connect",
                    format!(
                        "error target={user}@{host}:{port} attempt={} elapsed_ms={} error={message}",
                        attempt + 1,
                        started.elapsed().as_millis()
                    ),
                );
                let retryable = is_retryable_ssh_connect_error(&message);
                last_error = message;
                if !retryable || attempt + 1 >= SSH_CONNECT_RETRY_COUNT {
                    break;
                }
                thread::sleep(Duration::from_millis(500 + (attempt as u64 * 500)));
            }
        }
    }

    Err(last_error)
}

fn resolve_sftp_path(sftp: &ssh2::Sftp, raw_path: Option<&str>) -> Result<String, String> {
    let home = sftp
        .realpath(Path::new("."))
        .map_err(|error| format!("Failed to resolve remote home: {error}"))?
        .to_string_lossy()
        .replace('\\', "/");
    let candidate = raw_path.unwrap_or("").trim();
    if candidate.is_empty() || candidate == "~" {
        return Ok(home);
    }
    let expanded = if let Some(rest) = candidate.strip_prefix("~/") {
        join_remote_path(&home, rest)
    } else {
        candidate.to_string()
    };
    sftp.realpath(Path::new(&expanded))
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .map_err(|error| format!("Failed to resolve remote path: {error}"))
}

fn join_remote_path(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

fn sftp_mode_is_dir(mode: u32) -> bool {
    mode & 0o170000 == 0o040000
}

fn sftp_mode_is_symlink(mode: u32) -> bool {
    mode & 0o170000 == 0o120000
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn format_unix_permissions(mode: u32) -> String {
    let mut output = String::with_capacity(9);
    for shift in [6, 3, 0] {
        let value = (mode >> shift) & 0o7;
        output.push(if value & 0o4 != 0 { 'r' } else { '-' });
        output.push(if value & 0o2 != 0 { 'w' } else { '-' });
        output.push(if value & 0o1 != 0 { 'x' } else { '-' });
    }
    output
}

fn remote_aux_exec_output(
    sessions: Arc<Mutex<HashMap<String, RemoteAuxSessionHandle>>>,
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    host: String,
    user: String,
    password: String,
    port: u16,
    command: String,
    input: Option<String>,
) -> Result<String, String> {
    validate_ssh_part(&host, "host")?;
    validate_ssh_part(&user, "user")?;
    if password.is_empty() {
        return Err("Password is required for SSH password auth".into());
    }

    let key = remote_aux_session_key(&host, &user, port);
    let mut last_error = None;

    for _ in 0..REMOTE_AUX_EXEC_ATTEMPTS {
        let started = Instant::now();
        diag_log(
            "remote-aux",
            format!("dispatch key={key} command_bytes={}", command.len()),
        );
        let handle = get_or_start_remote_aux_session(
            sessions.clone(),
            connect_in_flight.clone(),
            key.clone(),
            host.clone(),
            user.clone(),
            password.clone(),
            port,
        )?;
        let (response_tx, response_rx) = mpsc::channel();
        let command_packet = RemoteAuxCommand::Exec {
            command: command.clone(),
            input: input.clone(),
            response: response_tx,
        };

        if handle.sender.send(command_packet).is_err() {
            handle.alive.store(false, Ordering::SeqCst);
            remove_remote_aux_session(&sessions, &key, &handle.alive);
            last_error = Some("Remote helper session is not running".to_string());
            continue;
        }

        match response_rx.recv_timeout(REMOTE_AUX_EXEC_TIMEOUT) {
            Ok(result) => {
                match &result {
                    Ok(output) => diag_log(
                        "remote-aux",
                        format!(
                            "response key={key} ok bytes={} elapsed_ms={}",
                            output.len(),
                            started.elapsed().as_millis()
                        ),
                    ),
                    Err(error) => diag_log(
                        "remote-aux",
                        format!(
                            "response key={key} error elapsed_ms={} error={error}",
                            started.elapsed().as_millis()
                        ),
                    ),
                }
                return result;
            }
            Err(error) => {
                diag_log(
                    "remote-aux",
                    format!(
                        "timeout key={key} elapsed_ms={} error={error}",
                        started.elapsed().as_millis()
                    ),
                );
                handle.alive.store(false, Ordering::SeqCst);
                remove_remote_aux_session(&sessions, &key, &handle.alive);
                last_error = Some(format!("Remote helper timed out: {error}"));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| "Remote helper failed".to_string()))
}

fn get_or_start_remote_aux_session(
    sessions: Arc<Mutex<HashMap<String, RemoteAuxSessionHandle>>>,
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    key: String,
    host: String,
    user: String,
    password: String,
    port: u16,
) -> Result<RemoteAuxSessionHandle, String> {
    if let Some(handle) = sessions
        .lock()
        .map_err(|_| "Remote helper session store is poisoned".to_string())?
        .get(&key)
        .filter(|handle| handle.alive.load(Ordering::SeqCst))
        .cloned()
    {
        return Ok(handle);
    }

    let (command_tx, command_rx) = mpsc::channel();
    let alive = Arc::new(AtomicBool::new(true));
    let handle = RemoteAuxSessionHandle {
        sender: command_tx,
        alive: alive.clone(),
    };

    {
        let mut guard = sessions
            .lock()
            .map_err(|_| "Remote helper session store is poisoned".to_string())?;
        if let Some(existing) = guard
            .get(&key)
            .filter(|handle| handle.alive.load(Ordering::SeqCst))
        {
            return Ok(existing.clone());
        }
        guard.insert(key.clone(), handle.clone());
    }

    thread::spawn(move || {
        run_remote_aux_worker(
            sessions,
            connect_in_flight,
            key,
            host,
            user,
            password,
            port,
            command_rx,
            alive,
        );
    });

    Ok(handle)
}

fn run_remote_aux_worker(
    sessions: Arc<Mutex<HashMap<String, RemoteAuxSessionHandle>>>,
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    key: String,
    host: String,
    user: String,
    password: String,
    port: u16,
    command_rx: Receiver<RemoteAuxCommand>,
    alive: Arc<AtomicBool>,
) {
    diag_log(
        "remote-aux-worker",
        format!("start key={key} target={user}@{host}:{port}"),
    );
    let mut session =
        match connect_aux_ssh_session(connect_in_flight, &host, &user, &password, port) {
            Ok(session) => session,
            Err(message) => {
                diag_log(
                    "remote-aux-worker",
                    format!("connect_error key={key} error={message}"),
                );
                alive.store(false, Ordering::SeqCst);
                while let Ok(RemoteAuxCommand::Exec { response, .. }) =
                    command_rx.recv_timeout(Duration::from_millis(250))
                {
                    let _ = response.send(Err(message.clone()));
                }
                remove_remote_aux_session(&sessions, &key, &alive);
                return;
            }
        };

    loop {
        if !alive.load(Ordering::SeqCst) {
            break;
        }

        let packet = match command_rx.recv_timeout(REMOTE_AUX_IDLE_TIMEOUT) {
            Ok(packet) => packet,
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };

        match packet {
            RemoteAuxCommand::Exec {
                command,
                input,
                response,
            } => {
                let started = Instant::now();
                diag_log(
                    "remote-aux-worker",
                    format!(
                        "exec_start key={key} command_bytes={} has_input={}",
                        command.len(),
                        input.is_some()
                    ),
                );
                let result = exec_on_existing_session(&mut session, &command, input);
                let failed = result.is_err();
                match &result {
                    Ok(output) => diag_log(
                        "remote-aux-worker",
                        format!(
                            "exec_done key={key} bytes={} elapsed_ms={}",
                            output.len(),
                            started.elapsed().as_millis()
                        ),
                    ),
                    Err(error) => diag_log(
                        "remote-aux-worker",
                        format!(
                            "exec_error key={key} elapsed_ms={} error={error}",
                            started.elapsed().as_millis()
                        ),
                    ),
                }
                let _ = response.send(result);
                if failed {
                    alive.store(false, Ordering::SeqCst);
                    break;
                }
            }
        }
    }

    alive.store(false, Ordering::SeqCst);
    remove_remote_aux_session(&sessions, &key, &alive);
    diag_log("remote-aux-worker", format!("exit key={key}"));
}

fn connect_aux_ssh_session(
    connect_in_flight: Arc<(Mutex<usize>, Condvar)>,
    host: &str,
    user: &str,
    password: &str,
    port: u16,
) -> Result<Session, String> {
    let mut last_error = String::new();

    for attempt in 0..REMOTE_AUX_CONNECT_RETRY_COUNT {
        let result = {
            let _permit = SshConnectLimiter::acquire_from(connect_in_flight.clone())?;
            connect_interactive_ssh_session(host, user, password, port)
        };

        match result {
            Ok(session) => {
                session.set_keepalive(true, 30);
                session.set_timeout(SSH_IO_TIMEOUT_MS);
                session.set_blocking(true);
                return Ok(session);
            }
            Err(message) => {
                let retryable = is_retryable_ssh_connect_error(&message);
                last_error = message;
                if !retryable || attempt + 1 >= REMOTE_AUX_CONNECT_RETRY_COUNT {
                    break;
                }
                thread::sleep(Duration::from_millis(900 + (attempt as u64 * 900)));
            }
        }
    }

    Err(last_error)
}

fn exec_on_existing_session(
    session: &mut Session,
    command: &str,
    input: Option<String>,
) -> Result<String, String> {
    let mut channel = session
        .channel_session()
        .map_err(|error| format!("Failed to open SSH exec channel: {error}"))?;
    channel
        .exec(command)
        .map_err(|error| format!("Failed to run remote command: {error}"))?;
    read_exec_channel_output(&mut channel, input)
}

fn read_exec_channel_output(
    channel: &mut Channel,
    input: Option<String>,
) -> Result<String, String> {
    if let Some(input) = input {
        channel
            .write_all(input.as_bytes())
            .map_err(|error| format!("Failed to write remote stdin: {error}"))?;
        channel
            .send_eof()
            .map_err(|error| format!("Failed to close remote stdin: {error}"))?;
    }

    let mut stdout = String::new();
    channel
        .read_to_string(&mut stdout)
        .map_err(|error| format!("Failed to read remote output: {error}"))?;
    let mut stderr = String::new();
    channel
        .stderr()
        .read_to_string(&mut stderr)
        .map_err(|error| format!("Failed to read remote error output: {error}"))?;
    channel
        .wait_close()
        .map_err(|error| format!("Failed to close remote command: {error}"))?;
    let status = channel
        .exit_status()
        .map_err(|error| format!("Failed to read remote exit status: {error}"))?;
    if status != 0 {
        let message = if stderr.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        return Err(if message.is_empty() {
            format!("Remote command failed with status {status}")
        } else {
            message
        });
    }
    Ok(stdout)
}

fn remove_remote_aux_session(
    sessions: &Arc<Mutex<HashMap<String, RemoteAuxSessionHandle>>>,
    key: &str,
    alive: &Arc<AtomicBool>,
) {
    if let Ok(mut guard) = sessions.lock() {
        let should_remove = guard
            .get(key)
            .map(|handle| Arc::ptr_eq(&handle.alive, alive))
            .unwrap_or(false);
        if should_remove {
            guard.remove(key);
        }
    }
}

fn remote_aux_session_key(host: &str, user: &str, port: u16) -> String {
    format!("{user}@{host}:{port}")
}

fn validate_ssh_part(value: &str, label: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{label} is required"));
    }

    let safe = value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '@' | ':'));

    if safe {
        Ok(())
    } else {
        Err(format!("{label} contains unsupported characters"))
    }
}

const MAX_TEXT_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_BATCH_COMMAND_BYTES: usize = 16 * 1024;
const MAX_BATCH_OUTPUT_BYTES: usize = 1024 * 1024;

fn atomic_write_file(path: &Path, content: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension("tmp");
    let backup = path.with_extension("bak");
    fs::write(&temporary, content)
        .map_err(|error| format!("Failed to write temporary state file: {error}"))?;
    if backup.exists() {
        let _ = fs::remove_file(&backup);
    }
    if path.exists() {
        fs::rename(path, &backup)
            .map_err(|error| format!("Failed to stage previous state file: {error}"))?;
    }
    if let Err(error) = fs::rename(&temporary, path) {
        if backup.exists() {
            let _ = fs::rename(&backup, path);
        }
        let _ = fs::remove_file(&temporary);
        return Err(format!("Failed to replace state file: {error}"));
    }
    if backup.exists() {
        let _ = fs::remove_file(backup);
    }
    Ok(())
}

#[tauri::command]
fn save_text_export(suggested_name: String, content: String) -> Result<Option<String>, String> {
    if content.len() > MAX_TEXT_FILE_BYTES {
        return Err("Export content exceeds 8 MiB".into());
    }
    let safe_name = Path::new(&suggested_name)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "XunDuTerminal-export.json".into());
    let Some(path) = rfd::FileDialog::new()
        .add_filter("JSON", &["json"])
        .set_file_name(&safe_name)
        .save_file()
    else {
        return Ok(None);
    };
    atomic_write_file(&path, content.as_bytes())?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

#[tauri::command]
fn open_text_import() -> Result<Option<String>, String> {
    let Some(path) = rfd::FileDialog::new()
        .add_filter("JSON", &["json"])
        .pick_file()
    else {
        return Ok(None);
    };
    let metadata =
        fs::metadata(&path).map_err(|error| format!("Failed to inspect import file: {error}"))?;
    if metadata.len() > MAX_TEXT_FILE_BYTES as u64 {
        return Err("Import file exceeds 8 MiB".into());
    }
    fs::read_to_string(path)
        .map(Some)
        .map_err(|error| format!("Failed to read import file: {error}"))
}

#[tauri::command]
async fn ssh_execute_command(
    connect_limiter: State<'_, SshConnectLimiter>,
    limiter: State<'_, RemoteAuxLimiter>,
    host: String,
    user: String,
    password: String,
    port: u16,
    command: String,
    timeout_seconds: u64,
) -> Result<SshCommandResult, String> {
    validate_ssh_part(&host, "host")?;
    validate_ssh_part(&user, "user")?;
    if port == 0 {
        return Err("SSH port must be between 1 and 65535".into());
    }
    if command.trim().is_empty() {
        return Err("Command cannot be empty".into());
    }
    if command.len() > MAX_BATCH_COMMAND_BYTES || command.contains('\0') {
        return Err("Command exceeds 16 KiB or contains a null byte".into());
    }
    let timeout_seconds = timeout_seconds.clamp(5, 600);
    let connect_in_flight = connect_limiter.in_flight.clone();
    let in_flight = limiter.in_flight.clone();
    let limit = limiter.limit.clone();
    run_blocking(move || {
        let started = Instant::now();
        let _permit = RemoteAuxLimiter::from_parts(in_flight, limit).acquire()?;
        let _connect_permit = SshConnectLimiter::acquire_from(connect_in_flight)?;
        let mut session = connect_interactive_ssh_session(&host, &user, &password, port)?;
        let io_timeout_ms = ((timeout_seconds + 15) * 1_000).min(u32::MAX as u64) as u32;
        session.set_timeout(io_timeout_ms);
        let marker = "__XUNDU_EXIT_CODE__=";
        let wrapped = format!(
            "timeout {timeout_seconds}s sh -lc {} 2>&1; code=$?; printf '\\n{marker}%s\\n' \"$code\"; exit 0",
            shell_single_quote(&command),
        );
        let output = exec_on_existing_session(&mut session, &wrapped, None)?;
        parse_ssh_command_result(
            output,
            started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        )
    })
    .await
}

fn parse_ssh_command_result(
    mut output: String,
    duration_ms: u64,
) -> Result<SshCommandResult, String> {
    let marker = "__XUNDU_EXIT_CODE__=";
    let marker_index = output
        .rfind(marker)
        .ok_or_else(|| "Remote command did not return an exit status".to_string())?;
    let exit_code = output[marker_index + marker.len()..]
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .parse::<i32>()
        .map_err(|_| "Remote command returned an invalid exit status".to_string())?;
    output.truncate(marker_index);
    while output.ends_with('\r') || output.ends_with('\n') {
        output.pop();
    }
    trim_utf8_tail(&mut output, MAX_BATCH_OUTPUT_BYTES);
    Ok(SshCommandResult {
        output,
        exit_code,
        duration_ms,
        timed_out: exit_code == 124 || exit_code == 137,
    })
}

fn tunnel_forwarding_argument(
    mode: &str,
    bind_address: &str,
    listen_port: u16,
    target_host: &str,
    target_port: u16,
) -> Result<(&'static str, String), String> {
    match mode {
        "local" => Ok((
            "-L",
            format!("{bind_address}:{listen_port}:{target_host}:{target_port}"),
        )),
        "remote" => Ok((
            "-R",
            format!("{bind_address}:{listen_port}:{target_host}:{target_port}"),
        )),
        "dynamic" => Ok(("-D", format!("{bind_address}:{listen_port}"))),
        _ => Err("Tunnel mode must be local, remote, or dynamic".into()),
    }
}

fn tunnel_status(id: &str, process: &SshTunnelProcess) -> SshTunnelStatus {
    SshTunnelStatus {
        id: id.to_string(),
        pid: process.child.id(),
        started_at: process.started_at,
        mode: process.mode.clone(),
        listen_port: process.listen_port,
    }
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn ssh_tunnel_start(
    state: State<'_, SshTunnelProcesses>,
    tunnel_id: String,
    host: String,
    user: String,
    port: u16,
    mode: String,
    bind_address: String,
    listen_port: u16,
    target_host: String,
    target_port: u16,
) -> Result<SshTunnelStatus, String> {
    let processes = state.processes.clone();
    run_blocking(move || {
        ssh_tunnel_start_sync(
            processes,
            tunnel_id,
            host,
            user,
            port,
            mode,
            bind_address,
            listen_port,
            target_host,
            target_port,
        )
    })
    .await
}

#[allow(clippy::too_many_arguments)]
fn ssh_tunnel_start_sync(
    process_store: Arc<Mutex<HashMap<String, SshTunnelProcess>>>,
    tunnel_id: String,
    host: String,
    user: String,
    port: u16,
    mode: String,
    bind_address: String,
    listen_port: u16,
    target_host: String,
    target_port: u16,
) -> Result<SshTunnelStatus, String> {
    validate_ssh_part(&tunnel_id, "tunnel id")?;
    validate_ssh_part(&host, "host")?;
    validate_ssh_part(&user, "user")?;
    validate_ssh_part(&bind_address, "bind address")?;
    if port == 0 || listen_port == 0 {
        return Err("SSH and listening ports must be between 1 and 65535".into());
    }
    let mode = mode.trim().to_ascii_lowercase();
    if !matches!(mode.as_str(), "local" | "remote" | "dynamic") {
        return Err("Tunnel mode must be local, remote, or dynamic".into());
    }
    if mode != "dynamic" {
        validate_ssh_part(&target_host, "target host")?;
        if target_port == 0 {
            return Err("Target port must be between 1 and 65535".into());
        }
    }
    if mode == "local" || mode == "dynamic" {
        let listener =
            TcpListener::bind((bind_address.as_str(), listen_port)).map_err(|error| {
                format!("Local port {bind_address}:{listen_port} is unavailable: {error}")
            })?;
        drop(listener);
    }

    let auth = resolve_ssh_auth_profile(&host, &user, "", port);
    let auth_method = normalized_auth_method(&auth.auth_method);
    if auth_method == "password" {
        return Err("Password tunnels are disabled because credentials must not be exposed to ssh.exe; use a private key or SSH Agent".into());
    }

    let mut processes = process_store
        .lock()
        .map_err(|_| "SSH tunnel store is unavailable".to_string())?;
    if let Some(existing) = processes.get_mut(&tunnel_id) {
        if existing
            .child
            .try_wait()
            .map_err(|error| format!("Failed to inspect existing tunnel: {error}"))?
            .is_none()
        {
            return Ok(tunnel_status(&tunnel_id, existing));
        }
        processes.remove(&tunnel_id);
    }

    let mut command = Command::new("ssh.exe");
    command
        .arg("-N")
        .arg("-T")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=3")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-p")
        .arg(port.to_string());
    if auth_method == "key" {
        let key_path = auth
            .private_key_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
            .ok_or_else(|| "SSH private key path is missing".to_string())?;
        command
            .arg("-o")
            .arg("PreferredAuthentications=publickey")
            .arg("-o")
            .arg("IdentitiesOnly=yes")
            .arg("-i")
            .arg(key_path);
    } else {
        command
            .arg("-o")
            .arg("PreferredAuthentications=publickey")
            .arg("-o")
            .arg("IdentitiesOnly=no");
    }
    let (forwarding_flag, forwarding) =
        tunnel_forwarding_argument(&mode, &bind_address, listen_port, &target_host, target_port)?;
    command.arg(forwarding_flag);
    command
        .arg(forwarding)
        .arg(format!("{user}@{host}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    hide_command_window(&mut command);
    let mut child = command
        .spawn()
        .map_err(|error| format!("Failed to start ssh.exe: {error}"))?;
    thread::sleep(Duration::from_millis(450));
    if let Some(status) = child
        .try_wait()
        .map_err(|error| format!("Failed to inspect SSH tunnel startup: {error}"))?
    {
        let mut detail = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut detail);
        }
        return Err(if detail.trim().is_empty() {
            format!("ssh.exe exited during tunnel startup with status {status}")
        } else {
            detail.trim().to_string()
        });
    }
    if let Some(mut stderr) = child.stderr.take() {
        thread::spawn(move || {
            let _ = std::io::copy(&mut stderr, &mut std::io::sink());
        });
    }
    let process = SshTunnelProcess {
        child,
        started_at: epoch_millis(),
        mode,
        listen_port,
    };
    let status = tunnel_status(&tunnel_id, &process);
    processes.insert(tunnel_id, process);
    Ok(status)
}

#[tauri::command]
async fn ssh_tunnel_stop(
    state: State<'_, SshTunnelProcesses>,
    tunnel_id: String,
) -> Result<(), String> {
    let processes = state.processes.clone();
    run_blocking(move || ssh_tunnel_stop_sync(processes, tunnel_id)).await
}

fn ssh_tunnel_stop_sync(
    process_store: Arc<Mutex<HashMap<String, SshTunnelProcess>>>,
    tunnel_id: String,
) -> Result<(), String> {
    let mut process = process_store
        .lock()
        .map_err(|_| "SSH tunnel store is unavailable".to_string())?
        .remove(&tunnel_id);
    if let Some(process) = process.as_mut() {
        if process
            .child
            .try_wait()
            .map_err(|error| format!("Failed to inspect SSH tunnel: {error}"))?
            .is_none()
        {
            process
                .child
                .kill()
                .map_err(|error| format!("Failed to stop SSH tunnel: {error}"))?;
        }
        let _ = process.child.wait();
    }
    Ok(())
}

#[tauri::command]
fn ssh_tunnel_list(state: State<SshTunnelProcesses>) -> Result<Vec<SshTunnelStatus>, String> {
    let mut processes = state
        .processes
        .lock()
        .map_err(|_| "SSH tunnel store is unavailable".to_string())?;
    let mut stopped = Vec::new();
    let mut statuses = Vec::new();
    for (id, process) in processes.iter_mut() {
        match process.child.try_wait() {
            Ok(None) => statuses.push(tunnel_status(id, process)),
            Ok(Some(_)) => stopped.push(id.clone()),
            Err(error) => return Err(format!("Failed to inspect SSH tunnel {id}: {error}")),
        }
    }
    for id in stopped {
        processes.remove(&id);
    }
    statuses.sort_by_key(|status| std::cmp::Reverse(status.started_at));
    Ok(statuses)
}

#[tauri::command]
fn set_remote_aux_limit(limiter: State<RemoteAuxLimiter>, limit: usize) -> Result<usize, String> {
    limiter.set_limit(limit)
}

#[tauri::command]
fn get_remote_aux_limit(limiter: State<RemoteAuxLimiter>) -> Result<usize, String> {
    limiter.current_limit()
}

#[cfg(test)]
mod tests {
    use super::{
        atomic_write_file, default_local_shell_command, is_allowed_external_url,
        is_allowed_source_repository_url, known_host_entry_matches_target, local_process_list_sync,
        local_read_file_sync, measure_local_upload, normalized_auth_method,
        parse_detected_cli_tools, parse_ssh_command_result, parse_update_version,
        redact_sensitive_text, remote_download_staging_path, resolve_ssh_socket_addresses,
        safe_file_name, sanitize_diagnostic_content, ssh_output_confirms_authentication,
        ssh_output_reports_authentication_failure, terminal_output_should_flush, trim_utf8_tail,
        tunnel_forwarding_argument, unique_destination, validated_background_extension,
        CliToolInfo, LocalProcessSampler, QQ_GROUP_ONE_URL, QQ_GROUP_TWO_URL,
        SSH_HOST_KEY_PREFERENCE,
    };
    use ssh2::{MethodType, Session};
    use std::{
        env, fs, process,
        sync::atomic::AtomicBool,
        time::{Duration, Instant},
    };

    #[cfg(target_os = "windows")]
    use super::{refreshed_windows_path, windows_environment_snapshot};
    #[cfg(target_os = "windows")]
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    #[cfg(target_os = "windows")]
    use std::{
        ffi::OsStr,
        io::{Read, Write},
        sync::mpsc,
        thread,
    };

    #[test]
    fn trims_terminal_probe_on_utf8_boundary() {
        let mut probe = "a".repeat(4095);
        probe.push_str("中文输出");

        trim_utf8_tail(&mut probe, 2048);

        assert!(probe.len() <= 2048);
        assert!(probe.ends_with("中文输出"));
    }

    #[test]
    fn local_process_cpu_is_normalized_to_machine_capacity() {
        let sampler = LocalProcessSampler::default();
        let entries = local_process_list_sync(sampler.sampler).expect("process sample");

        assert!(!entries.is_empty());
        assert!(entries
            .iter()
            .all(|entry| (0.0..=100.0).contains(&entry.cpu_usage)));
    }

    #[test]
    fn cli_detection_preserves_known_order_and_prefers_primary_command() {
        let tools = parse_detected_cli_tools("kiro\nunknown\ncodex\nkiro-cli\nclaude\n");
        assert_eq!(
            tools,
            vec![
                CliToolInfo {
                    id: "claude".into(),
                    name: "Claude Code".into(),
                    command: "claude".into(),
                },
                CliToolInfo {
                    id: "codex".into(),
                    name: "Codex".into(),
                    command: "codex".into(),
                },
                CliToolInfo {
                    id: "kiro".into(),
                    name: "Kiro CLI".into(),
                    command: "kiro-cli".into(),
                },
            ]
        );
    }

    #[test]
    fn ssh_process_does_not_report_connected_before_authentication() {
        assert!(!ssh_output_confirms_authentication(
            false,
            false,
            "SSH process started"
        ));
        assert!(!ssh_output_confirms_authentication(
            true,
            false,
            "Permission denied, please try again."
        ));
        assert!(ssh_output_reports_authentication_failure(
            "Permission denied, please try again."
        ));
        assert!(ssh_output_reports_authentication_failure(
            "Too many authentication failures"
        ));
    }

    #[test]
    fn ssh_helper_prefers_modern_host_keys_before_legacy_rsa() {
        assert!(SSH_HOST_KEY_PREFERENCE.starts_with("ssh-ed25519,ecdsa-sha2-nistp256"));
        assert!(SSH_HOST_KEY_PREFERENCE.contains("ecdsa-sha2-nistp384"));
        assert!(SSH_HOST_KEY_PREFERENCE.contains("ecdsa-sha2-nistp521"));
        assert!(SSH_HOST_KEY_PREFERENCE.contains("ssh-ed25519-cert-v01@openssh.com"));
        let session = Session::new().expect("SSH session");
        session
            .method_pref(MethodType::HostKey, SSH_HOST_KEY_PREFERENCE)
            .expect("supported host key preference");
    }

    #[test]
    fn ssh_socket_resolution_accepts_bare_and_bracketed_ipv6_hosts() {
        for host in ["::1", "[::1]"] {
            let addresses = resolve_ssh_socket_addresses(host, 22).expect("IPv6 address");
            assert!(addresses
                .iter()
                .any(|address| address.is_ipv6() && address.port() == 22));
        }
    }

    #[test]
    fn small_terminal_echo_flushes_immediately() {
        assert!(terminal_output_should_flush("echo", &Instant::now()));
    }

    #[test]
    fn known_host_replacement_only_matches_the_exact_endpoint() {
        assert!(known_host_entry_matches_target(
            "server.example",
            "server.example",
            22
        ));
        assert!(known_host_entry_matches_target(
            "[server.example]:22",
            "server.example",
            22
        ));
        assert!(known_host_entry_matches_target(
            "[server.example]:2222",
            "server.example",
            2222
        ));
        assert!(!known_host_entry_matches_target(
            "server.example",
            "server.example",
            2222
        ));
        assert!(!known_host_entry_matches_target(
            "[server.example]:2200",
            "server.example",
            2222
        ));
        assert!(!known_host_entry_matches_target(
            "other.example",
            "server.example",
            22
        ));
    }

    #[test]
    fn ssh_process_reports_connected_after_authenticated_output() {
        assert!(ssh_output_confirms_authentication(
            true,
            false,
            "Last login: Sun Jul 12 19:10:00 2026"
        ));
        assert!(!ssh_output_confirms_authentication(
            true,
            true,
            "root@server:~#"
        ));
    }

    #[test]
    fn diagnostics_redact_credentials_and_private_keys() {
        let redacted = redact_sensitive_text(
            "password=hunter2 token=abc123 clientSecret=xyz /pass:rdp-secret",
        );
        assert!(!redacted.contains("hunter2"));
        assert!(!redacted.contains("abc123"));
        assert!(!redacted.contains("rdp-secret"));
        assert_eq!(redacted.matches("[REDACTED]").count(), 4);
        assert_eq!(
            redact_sensitive_text("-----BEGIN OPENSSH PRIVATE KEY----- secret material"),
            "[REDACTED PRIVATE KEY MATERIAL]"
        );
    }

    #[test]
    fn diagnostics_anonymize_infrastructure_and_paths() {
        let endpoint = redact_sensitive_text("start target=root@203.0.113.247:22 size=120x40");
        assert!(!endpoint.contains("root"));
        assert!(!endpoint.contains("203.0.113.247"));
        assert!(endpoint.contains("target=[ENDPOINT:"));
        assert!(endpoint.contains("size=120x40"));

        let server =
            redact_sensitive_text("slow dispatch server=prod.example.com bytes=512 elapsed_ms=140");
        assert!(!server.contains("prod.example.com"));
        assert!(server.contains("server=[ENDPOINT:"));
        assert!(server.contains("bytes=512 elapsed_ms=140"));

        let path = redact_sensitive_text(
            "done target=operator@host.example:22 path=/var/www/Customer Site bytes=128",
        );
        assert!(!path.contains("operator"));
        assert!(!path.contains("host.example"));
        assert!(!path.contains("/var/www"));
        assert!(!path.contains("Customer Site"));
        assert!(path.contains("path=[REDACTED]"));

        let error = redact_sensitive_text(
            r"error host=10.2.3.4 message=failed to open C:\Users\operator\secret.txt",
        );
        assert!(!error.contains("10.2.3.4"));
        assert!(!error.contains("operator"));
        assert!(!error.contains("secret.txt"));
        assert!(error.contains("message=[REDACTED]"));

        let network = redact_sensitive_text("peer 203.0.113.9 disconnected");
        assert_eq!(network, "peer [IP] disconnected");

        let exported = sanitize_diagnostic_content(
            "1 [ssh] start target=root@203.0.113.10:22\n2 [file] path=/srv/customer data\n",
        );
        assert!(!exported.contains("root"));
        assert!(!exported.contains("203.0.113.10"));
        assert!(!exported.contains("/srv/customer"));
        assert_eq!(exported.lines().count(), 2);
        assert!(exported.ends_with('\n'));
    }

    #[test]
    fn ssh_auth_method_accepts_legacy_and_agent_names() {
        assert_eq!(normalized_auth_method("Password"), "password");
        assert_eq!(normalized_auth_method("Key"), "key");
        assert_eq!(normalized_auth_method("private-key"), "key");
        assert_eq!(normalized_auth_method("SSH-Agent"), "agent");
    }

    #[test]
    fn editor_accepts_text_files_larger_than_five_megabytes() {
        let path = env::temp_dir().join(format!("xundu-editor-limit-{}.txt", process::id()));
        let bytes = vec![b'a'; 6 * 1024 * 1024];
        fs::write(&path, &bytes).expect("write editor fixture");

        let result = local_read_file_sync(path.to_string_lossy().into_owned());
        let _ = fs::remove_file(&path);

        assert_eq!(result.expect("read editor fixture").len(), bytes.len());
    }

    #[test]
    fn download_names_cannot_escape_the_selected_folder() {
        assert_eq!(safe_file_name("/var/log/app.log").unwrap(), "app.log");
        assert!(safe_file_name("../").is_err());
        assert!(safe_file_name(".").is_err());
    }

    #[test]
    fn folder_download_uses_a_unique_destination() {
        let root = env::temp_dir().join(format!("xundu-download-destination-{}", process::id()));
        let existing = root.join("logs");
        fs::create_dir_all(&existing).expect("create existing download folder");

        let destination = unique_destination(existing);
        let _ = fs::remove_dir_all(&root);

        assert_eq!(destination.file_name().unwrap(), "logs (1)");
    }

    #[test]
    fn folder_upload_measurement_counts_nested_files_and_bytes() {
        let root = env::temp_dir().join(format!("xundu-upload-tree-{}", process::id()));
        let nested = root.join("nested");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(nested.join("empty")).expect("create upload fixture");
        fs::write(root.join("one.txt"), b"abc").expect("write root upload fixture");
        fs::write(nested.join("two.bin"), b"12345").expect("write nested upload fixture");

        let mut total_bytes = 0;
        let mut total_files = 0;
        measure_local_upload(
            &root,
            &AtomicBool::new(false),
            &mut total_bytes,
            &mut total_files,
        )
        .expect("measure recursive upload");
        let _ = fs::remove_dir_all(&root);

        assert_eq!(total_files, 2);
        assert_eq!(total_bytes, 8);
    }

    #[test]
    fn background_picker_accepts_only_supported_image_extensions() {
        assert_eq!(
            validated_background_extension(std::path::Path::new("aurora.WEBP")).unwrap(),
            "webp"
        );
        assert!(validated_background_extension(std::path::Path::new("notes.svg")).is_err());
        assert!(validated_background_extension(std::path::Path::new("no-extension")).is_err());
    }

    #[test]
    fn atomic_write_replaces_content_without_temp_files() {
        let root = env::temp_dir().join(format!("xundu-atomic-state-{}", process::id()));
        let path = root.join("state.json");
        fs::create_dir_all(&root).expect("create phase2 state directory");

        atomic_write_file(&path, br#"{"version":1}"#).expect("write first state");
        atomic_write_file(&path, br#"{"version":2}"#).expect("replace state");

        assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"version":2}"#);
        assert!(!path.with_extension("tmp").exists());
        assert!(!path.with_extension("bak").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn remote_download_resume_path_is_stable_and_hidden() {
        let destination = env::temp_dir().join("release.zip");
        let first = remote_download_staging_path(&destination);
        let second = remote_download_staging_path(&destination);

        assert_eq!(first, second);
        assert_eq!(first.file_name().unwrap(), ".release.zip.xundu.part");
        assert_ne!(first, destination);
    }

    #[test]
    fn batch_command_result_parses_exit_codes_and_timeout() {
        let success = parse_ssh_command_result("done\n__XUNDU_EXIT_CODE__=0\n".into(), 42)
            .expect("parse successful command");
        assert_eq!(success.output, "done");
        assert_eq!(success.exit_code, 0);
        assert_eq!(success.duration_ms, 42);
        assert!(!success.timed_out);

        let timeout = parse_ssh_command_result("partial\n__XUNDU_EXIT_CODE__=124\n".into(), 5000)
            .expect("parse timed out command");
        assert_eq!(timeout.exit_code, 124);
        assert!(timeout.timed_out);
        assert!(parse_ssh_command_result("missing marker".into(), 1).is_err());
    }

    #[test]
    fn tunnel_modes_generate_expected_openssh_arguments() {
        assert_eq!(
            tunnel_forwarding_argument("local", "127.0.0.1", 8080, "db", 5432).unwrap(),
            ("-L", "127.0.0.1:8080:db:5432".into())
        );
        assert_eq!(
            tunnel_forwarding_argument("remote", "0.0.0.0", 9000, "127.0.0.1", 3000).unwrap(),
            ("-R", "0.0.0.0:9000:127.0.0.1:3000".into())
        );
        assert_eq!(
            tunnel_forwarding_argument("dynamic", "127.0.0.1", 1080, "", 0).unwrap(),
            ("-D", "127.0.0.1:1080".into())
        );
        assert!(tunnel_forwarding_argument("invalid", "127.0.0.1", 1, "", 0).is_err());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn local_shell_uses_a_controlling_pty_and_refreshed_path() {
        let command = default_local_shell_command();
        assert!(command.get_controlling_tty());
        assert!(command.get_env("NO_COLOR").is_none());
        assert!(command.get_env("NODE_DISABLE_COLORS").is_none());
        assert_eq!(command.get_env("TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(command.get_env("COLORTERM"), Some(OsStr::new("truecolor")));
        assert_eq!(command.get_env("CLICOLOR"), Some(OsStr::new("1")));

        let path = command
            .get_env("PATH")
            .expect("local shell PATH")
            .to_string_lossy()
            .to_lowercase();
        let refreshed = refreshed_windows_path()
            .expect("refreshed PATH")
            .to_string_lossy()
            .to_lowercase();
        assert_eq!(path, refreshed);

        if let Some((_, Some(user_path))) = windows_environment_snapshot() {
            for entry in env::split_paths(OsStr::new(&user_path)) {
                let entry = entry.to_string_lossy().to_lowercase();
                assert!(path.contains(&entry), "missing user PATH entry: {entry}");
            }
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn conpty_reports_interactive_node_stdio() {
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open ConPTY");
        let mut command = CommandBuilder::new("node.exe");
        command.args([
            "-e",
            "console.log(Boolean(process.stdin.isTTY),Boolean(process.stdout.isTTY))",
        ]);
        let mut reader = pair.master.try_clone_reader().expect("clone PTY reader");
        let mut child = pair
            .slave
            .spawn_command(command)
            .expect("spawn node in ConPTY");
        drop(pair.slave);

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut output = String::new();
            let _ = reader.read_to_string(&mut output);
            let _ = tx.send(output);
        });
        child.wait().expect("wait for node");
        drop(pair.master);
        let output = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("read ConPTY probe output")
            .to_lowercase();
        assert!(
            output.contains("true true"),
            "unexpected ConPTY probe: {output:?}"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn local_cmd_cd_switches_drive_without_explicit_d_flag() {
        let target = env::current_dir().expect("current test directory");
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: 24,
                cols: 120,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("open local shell ConPTY");
        let command = default_local_shell_command();
        let mut reader = pair
            .master
            .try_clone_reader()
            .expect("clone local PTY reader");
        let mut writer = pair.master.take_writer().expect("take local PTY writer");
        let mut child = pair
            .slave
            .spawn_command(command)
            .expect("spawn local cmd shell");
        drop(pair.slave);

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut output = String::new();
            let _ = reader.read_to_string(&mut output);
            let _ = tx.send(output);
        });
        thread::sleep(Duration::from_millis(350));
        write!(writer, "cd \"{}\"\r", target.display()).expect("write local cd probe");
        writer.flush().expect("flush local cd probe");
        thread::sleep(Duration::from_millis(180));
        writer
            .write_all(b"echo __XUNDU_CWD__%CD%\r")
            .expect("write local cwd probe");
        writer.flush().expect("flush local cwd probe");
        thread::sleep(Duration::from_millis(180));
        writer
            .write_all(
                b"node -e \"console.log('__XUNDU_COLOR__',process.env.NO_COLOR||'unset',process.env.TERM,process.env.COLORTERM,process.stdout.getColorDepth?.())\"\r",
            )
            .expect("write local color probe");
        writer.flush().expect("flush local color probe");
        thread::sleep(Duration::from_millis(300));
        writer.write_all(b"exit\r").expect("write local shell exit");
        writer.flush().expect("flush local shell exit");
        drop(writer);
        child.wait().expect("wait for local cmd shell");
        drop(pair.master);

        let output = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("read local shell probe output")
            .replace('/', "\\")
            .to_lowercase();
        let expected = format!("__xundu_cwd__{}", target.display())
            .replace('/', "\\")
            .to_lowercase();
        assert!(
            output.contains(&expected),
            "local cd did not switch directory: {output:?}"
        );
        assert!(
            output.contains("__xundu_color__ unset xterm-256color truecolor")
                && output.contains("24\r\n"),
            "local shell did not expose truecolor capabilities: {output:?}"
        );
    }

    #[test]
    fn update_versions_and_external_urls_are_validated() {
        assert!(parse_update_version("v0.2.0").unwrap() > parse_update_version("0.1.9").unwrap());
        assert!(is_allowed_external_url("https://xunduyun.com/"));
        assert!(is_allowed_external_url(
            "https://www.xunduyun.com/xunduterminal/download"
        ));
        assert!(is_allowed_external_url(
            "https://github.com/example/XunDuTerminal/releases/latest"
        ));
        assert!(is_allowed_source_repository_url(
            "https://github.com/example/XunDuTerminal/releases/tag/v0.2.0"
        ));
        assert!(is_allowed_external_url(QQ_GROUP_ONE_URL));
        assert!(is_allowed_external_url(QQ_GROUP_TWO_URL));
        assert!(!is_allowed_external_url("http://xunduyun.com/"));
        assert!(!is_allowed_external_url(
            "https://xunduyun.com.example.com/"
        ));
        assert!(!is_allowed_source_repository_url(
            "https://github.com/example/not-xunduterminal/releases/latest"
        ));
        assert!(!is_allowed_external_url(
            "mqqapi://card/show_pslcard?src_type=internal&version=1&uin=123456&card_type=group&source=qrcode"
        ));
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(SshSessions::default())
        .manage(SshConnectLimiter::default())
        .manage(RemoteAuxSessions::default())
        .manage(LocalShellSessions::default())
        .manage(LocalStatsCache::default())
        .manage(RemoteStatsCache::default())
        .manage(LocalProcessSampler::default())
        .manage(RemoteAuxLimiter::default())
        .manage(FileDownloadTransfers::default())
        .manage(SshTunnelProcesses::default())
        .manage(IronRdpSessions::default())
        .manage(RdpFileTransfers::default())
        .setup(|app| {
            diag_log("app", "========== startup ==========");
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            local_shell_start,
            local_shell_write,
            local_shell_resize,
            local_shell_stop,
            local_home_dir,
            local_list_drives,
            local_list_dir,
            local_read_file,
            local_write_file,
            choose_file_download_destination,
            choose_file_upload_sources,
            choose_ssh_private_key,
            choose_app_background,
            clear_app_background,
            local_download_path,
            cancel_file_download,
            local_rename_path,
            local_delete_path,
            local_compress_paths,
            local_extract_archive,
            local_system_stats,
            local_process_list,
            remote_list_dir,
            remote_read_file,
            remote_write_file,
            remote_download_path,
            remote_upload_paths,
            remote_rename_path,
            remote_delete_path,
            remote_system_stats,
            remote_process_list,
            local_detect_cli_tools,
            remote_detect_cli_tools,
            credential_vault_status,
            credential_store_many,
            credential_get_many,
            credential_delete_many,
            ssh_register_auth_profiles,
            ssh_import_config,
            diag_log_frontend,
            export_diagnostics,
            check_app_update,
            open_external_url,
            save_text_export,
            open_text_import,
            set_remote_aux_limit,
            get_remote_aux_limit,
            ssh_replace_known_host,
            rdp_connect,
            rdp_input,
            rdp_disconnect,
            rdp_clipboard_file_paths,
            rdp_clipboard_sequence_number,
            rdp_offer_clipboard_files,
            rdp_file_clipboard_progress,
            rdp_upload_files,
            rdp_cancel_file_transfer,
            ssh_connect,
            ssh_write,
            ssh_session_health,
            ssh_resize,
            ssh_disconnect,
            ssh_execute_command,
            ssh_tunnel_start,
            ssh_tunnel_stop,
            ssh_tunnel_list
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
