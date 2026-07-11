use std::error::Error;
use std::ffi::{CString, OsStr};
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

const PRIVATE_FILE_MODE: u32 = 0o600;

pub(super) fn anchor_state_path(path: &Path) -> Result<PathBuf, DaemonStateLockError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .map_err(|_| DaemonStateLockError::Unavailable)?
            .join(path))
    }
}

/// Keeps an advisory lock on a stable state-directory entry until the daemon exits.
pub(super) struct DaemonStateLock {
    _file: File,
}

impl DaemonStateLock {
    pub(super) fn acquire(state_path: &Path) -> Result<Self, DaemonStateLockError> {
        let sqlite_path = state_path.with_extension("sqlite");
        let parent = open_private_parent(&sqlite_path)?;
        let lock_name = lock_file_name(&sqlite_path)?;
        let file = open_or_create_lock_file(&parent, &lock_name)?;
        secure_lock_file(&file)?;

        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result != 0 {
            let error = io::Error::last_os_error();
            return if matches!(error.raw_os_error(), Some(libc::EWOULDBLOCK)) {
                Err(DaemonStateLockError::AlreadyHeld)
            } else {
                Err(DaemonStateLockError::Unavailable)
            };
        }

        Ok(Self { _file: file })
    }
}

#[derive(Debug)]
pub(super) enum DaemonStateLockError {
    AlreadyHeld,
    UnsafePath,
    Unavailable,
}

impl fmt::Display for DaemonStateLockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyHeld => {
                formatter.write_str("daemon state lock is already held by another daemon")
            }
            Self::UnsafePath => formatter.write_str("daemon state lock path is unsafe"),
            Self::Unavailable => formatter.write_str("daemon state lock is unavailable"),
        }
    }
}

impl Error for DaemonStateLockError {}

fn open_private_parent(path: &Path) -> Result<File, DaemonStateLockError> {
    let parent = path.parent().ok_or(DaemonStateLockError::UnsafePath)?;
    let mut current = if parent.is_absolute() {
        open_directory(Path::new("/"))?
    } else {
        open_directory(Path::new("."))?
    };
    validate_ancestor(&current)?;

    for component in parent.components() {
        let Component::Normal(name) = component else {
            if matches!(component, Component::ParentDir | Component::Prefix(_)) {
                return Err(DaemonStateLockError::UnsafePath);
            }
            continue;
        };
        let next = open_directory_at(&current, name)?;
        validate_ancestor(&next)?;
        current = next;
    }

    let metadata = current
        .metadata()
        .map_err(|_| DaemonStateLockError::Unavailable)?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
        return Err(DaemonStateLockError::UnsafePath);
    }
    Ok(current)
}

fn open_directory(path: &Path) -> Result<File, DaemonStateLockError> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(classify_path_error)
}

fn open_directory_at(parent: &File, name: &OsStr) -> Result<File, DaemonStateLockError> {
    open_at(
        parent,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
}

fn lock_file_name(sqlite_path: &Path) -> Result<std::ffi::OsString, DaemonStateLockError> {
    let sqlite_name = sqlite_path
        .file_name()
        .ok_or(DaemonStateLockError::UnsafePath)?;
    let mut lock_name = std::ffi::OsString::from(".");
    lock_name.push(sqlite_name);
    lock_name.push(".lock");
    Ok(lock_name)
}

fn open_or_create_lock_file(parent: &File, name: &OsStr) -> Result<File, DaemonStateLockError> {
    match open_at_result(
        parent,
        name,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        PRIVATE_FILE_MODE,
    ) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => open_at(
            parent,
            name,
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        ),
        Err(error) => Err(classify_path_error(error)),
    }
}

fn open_at(
    parent: &File,
    name: &OsStr,
    flags: i32,
    mode: u32,
) -> Result<File, DaemonStateLockError> {
    open_at_result(parent, name, flags, mode).map_err(classify_path_error)
}

fn open_at_result(parent: &File, name: &OsStr, flags: i32, mode: u32) -> io::Result<File> {
    let name = CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags,
            mode as libc::mode_t,
        )
    };
    if fd >= 0 {
        Ok(unsafe { File::from_raw_fd(fd) })
    } else {
        Err(io::Error::last_os_error())
    }
}

fn validate_ancestor(directory: &File) -> Result<(), DaemonStateLockError> {
    let metadata = directory
        .metadata()
        .map_err(|_| DaemonStateLockError::Unavailable)?;
    let trusted_owner = metadata.uid() == unsafe { libc::geteuid() } || metadata.uid() == 0;
    let replaceable = metadata.mode() & 0o022 != 0 && metadata.mode() & libc::S_ISVTX == 0;
    if !metadata.is_dir() || !trusted_owner || replaceable {
        Err(DaemonStateLockError::UnsafePath)
    } else {
        Ok(())
    }
}

fn secure_lock_file(file: &File) -> Result<(), DaemonStateLockError> {
    let metadata = file
        .metadata()
        .map_err(|_| DaemonStateLockError::Unavailable)?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(DaemonStateLockError::UnsafePath);
    }
    if unsafe { libc::fchmod(file.as_raw_fd(), PRIVATE_FILE_MODE as libc::mode_t) } != 0 {
        return Err(DaemonStateLockError::Unavailable);
    }
    let metadata = file
        .metadata()
        .map_err(|_| DaemonStateLockError::Unavailable)?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != PRIVATE_FILE_MODE
    {
        return Err(DaemonStateLockError::UnsafePath);
    }
    Ok(())
}

fn classify_path_error(error: io::Error) -> DaemonStateLockError {
    if matches!(
        error.raw_os_error(),
        Some(libc::ELOOP) | Some(libc::ENOTDIR)
    ) {
        DaemonStateLockError::UnsafePath
    } else {
        DaemonStateLockError::Unavailable
    }
}
