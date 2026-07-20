use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, FileContentsFlags, FileContentsRequest, FileContentsResponse, FileDescriptor,
};

const MAX_FILE_COUNT: usize = 100_000;
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
struct LocalClipboardFile {
    path: PathBuf,
    directory: bool,
    size: u64,
    display_name: String,
}

#[derive(Debug, Default)]
struct FileClipboardState {
    files: Vec<LocalClipboardFile>,
    total_bytes: u64,
    served_bytes: u64,
    current_file: String,
    accepted: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct FileClipboardProgress {
    pub total_bytes: u64,
    pub served_bytes: u64,
    pub total_files: usize,
    pub current_file: String,
    pub accepted: Option<bool>,
}

fn state() -> &'static Mutex<FileClipboardState> {
    static STATE: OnceLock<Mutex<FileClipboardState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(FileClipboardState::default()))
}

fn ready_count() -> &'static AtomicUsize {
    static READY_COUNT: AtomicUsize = AtomicUsize::new(0);
    &READY_COUNT
}

pub fn file_clipboard_ready() -> bool {
    ready_count().load(Ordering::Acquire) > 0
}

pub(crate) fn register_file_clipboard_ready() {
    ready_count().fetch_add(1, Ordering::AcqRel);
}

pub(crate) fn unregister_file_clipboard_ready() {
    let _ = ready_count().fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| count.checked_sub(1));
}

pub fn set_file_clipboard_paths(paths: Vec<String>) -> Result<Vec<FileDescriptor>, String> {
    if paths.is_empty() {
        return Err("no files were provided".to_owned());
    }

    let mut files = Vec::new();
    let mut descriptors = Vec::new();
    for path in paths {
        let path = fs::canonicalize(&path).map_err(|error| format!("unable to read {path}: {error}"))?;
        collect_path(&path, None, &mut files, &mut descriptors)?;
        if files.len() > MAX_FILE_COUNT {
            return Err(format!("file selection exceeds {MAX_FILE_COUNT} entries"));
        }
    }

    let total_bytes = files
        .iter()
        .filter(|file| !file.directory)
        .fold(0u64, |total, file| total.saturating_add(file.size));
    *state()
        .lock()
        .map_err(|_| "file clipboard state is unavailable".to_owned())? = FileClipboardState {
        files,
        total_bytes,
        served_bytes: 0,
        current_file: String::new(),
        accepted: None,
    };
    Ok(descriptors)
}

pub fn clear_file_clipboard() {
    if let Ok(mut clipboard) = state().lock() {
        *clipboard = FileClipboardState::default();
    }
}

pub fn file_clipboard_progress() -> FileClipboardProgress {
    state()
        .lock()
        .map(|clipboard| FileClipboardProgress {
            total_bytes: clipboard.total_bytes,
            served_bytes: clipboard.served_bytes.min(clipboard.total_bytes),
            total_files: clipboard.files.iter().filter(|file| !file.directory).count(),
            current_file: clipboard.current_file.clone(),
            accepted: clipboard.accepted,
        })
        .unwrap_or_default()
}

pub(crate) fn mark_file_clipboard_accepted(accepted: bool) {
    if let Ok(mut clipboard) = state().lock() {
        clipboard.accepted = Some(accepted);
    }
}

pub(crate) fn file_contents_response(request: FileContentsRequest) -> FileContentsResponse<'static> {
    let index = match usize::try_from(request.index) {
        Ok(index) => index,
        Err(_) => return FileContentsResponse::new_error(request.stream_id),
    };
    let file = match state()
        .lock()
        .ok()
        .and_then(|clipboard| clipboard.files.get(index).cloned())
    {
        Some(file) => file,
        None => return FileContentsResponse::new_error(request.stream_id),
    };

    if request.flags.contains(FileContentsFlags::SIZE) {
        return FileContentsResponse::new_size_response(request.stream_id, file.size);
    }
    if file.directory || !request.flags.contains(FileContentsFlags::RANGE) {
        return FileContentsResponse::new_error(request.stream_id);
    }

    let response = read_file_range(&file.path, request.position, request.requested_size);
    match response {
        Ok(data) => {
            if let Ok(mut clipboard) = state().lock() {
                clipboard.served_bytes = clipboard
                    .served_bytes
                    .saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX))
                    .min(clipboard.total_bytes);
                clipboard.current_file = file.display_name;
            }
            FileContentsResponse::new_data_response(request.stream_id, data)
        }
        Err(_) => FileContentsResponse::new_error(request.stream_id),
    }
}

fn collect_path(
    path: &Path,
    relative_parent: Option<&str>,
    files: &mut Vec<LocalClipboardFile>,
    descriptors: &mut Vec<FileDescriptor>,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("unable to read {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("symbolic links are not supported: {}", path.display()));
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("invalid file name: {}", path.display()))?
        .to_owned();
    validate_wire_name(relative_parent, &name)?;

    let directory = metadata.is_dir();
    let size = if directory { 0 } else { metadata.len() };
    let attributes = if directory {
        ClipboardFileAttributes::DIRECTORY
    } else {
        ClipboardFileAttributes::NORMAL
    };
    let mut descriptor = FileDescriptor::new(name.clone()).with_attributes(attributes);
    if !directory {
        descriptor = descriptor.with_file_size(size);
    }
    if let Some(parent) = relative_parent {
        descriptor = descriptor.with_relative_path(parent.to_owned());
    }
    descriptors.push(descriptor);
    files.push(LocalClipboardFile {
        path: path.to_owned(),
        directory,
        size,
        display_name: name.clone(),
    });

    if directory {
        let child_parent = match relative_parent {
            Some(parent) => format!("{parent}\\{name}"),
            None => name,
        };
        let mut children = fs::read_dir(path)
            .map_err(|error| format!("unable to read directory {}: {error}", path.display()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("unable to read directory entry: {error}"))?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            collect_path(&child.path(), Some(&child_parent), files, descriptors)?;
        }
    } else if !metadata.is_file() {
        return Err(format!("unsupported file type: {}", path.display()));
    }
    Ok(())
}

fn validate_wire_name(relative_parent: Option<&str>, name: &str) -> Result<(), String> {
    let length = relative_parent.map_or(0, |parent| parent.encode_utf16().count() + 1)
        + name.encode_utf16().count()
        + 1;
    if length > 260 {
        Err(format!("remote clipboard path is longer than 260 UTF-16 characters: {name}"))
    } else {
        Ok(())
    }
}

fn read_file_range(path: &Path, position: u64, requested_size: u32) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(position))?;
    let capacity = usize::try_from(requested_size)
        .unwrap_or(MAX_RESPONSE_BYTES)
        .min(MAX_RESPONSE_BYTES);
    let mut data = vec![0u8; capacity];
    let read = file.read(&mut data)?;
    data.truncate(read);
    Ok(data)
}
