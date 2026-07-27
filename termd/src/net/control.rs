use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};
use std::path::{Component, Path, PathBuf};

use axum::Router;
use axum::body::Body;
use axum_core::extract::Request;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::{server::conn::auto::Builder, service::TowerToHyperService};
use thiserror::Error;
use tokio::net::UnixListener;
use tower::ServiceExt as _;
use tracing::warn;

pub const DEFAULT_CONTROL_SOCKET_PATH: &str = "/run/termd/termd.sock";
const PRIVATE_MODE: u32 = 0o600;
const STAGING_DIRECTORY_MODE: u32 = 0o700;

#[derive(Debug, Error)]
pub enum ControlSocketError {
    #[error("daemon control socket path is unsafe")]
    UnsafePath,
    #[error("daemon control socket lock is already held")]
    LockHeld,
    #[error("daemon control socket path is already active")]
    Active,
    #[error("daemon control socket path cannot be safely replaced")]
    Collision,
    #[error("failed to bind daemon control socket")]
    Bind(#[source] io::Error),
    #[error("daemon control socket server failed")]
    Serve(#[source] io::Error),
}

pub struct ControlSocketServer {
    listener: UnixListener,
    router: Router,
    _ownership: SocketOwnership,
}

impl ControlSocketServer {
    pub fn bind(path: impl AsRef<Path>, router: Router) -> Result<Self, ControlSocketError> {
        let path = validated_socket_path(path.as_ref())?;
        let lock = acquire_path_lock(&path)?;
        remove_verified_stale_socket(&path)?;

        let mut publication = SocketPublication::create(&path)?;
        let listener = publication.bind()?;
        listener
            .set_nonblocking(true)
            .map_err(ControlSocketError::Bind)?;
        let listener = UnixListener::from_std(listener).map_err(ControlSocketError::Bind)?;
        let identity = publication.publish()?;
        let ownership = SocketOwnership {
            path,
            dev: identity.dev,
            ino: identity.ino,
            _lock: lock,
        };
        Ok(Self {
            listener,
            router,
            _ownership: ownership,
        })
    }

    pub async fn serve(self) -> Result<(), ControlSocketError> {
        loop {
            let (stream, _) = self
                .listener
                .accept()
                .await
                .map_err(ControlSocketError::Serve)?;
            let service = self
                .router
                .clone()
                .map_request(|request: Request<Incoming>| request.map(Body::new));
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let service = TowerToHyperService::new(service);
                if let Err(error) = Builder::new(TokioExecutor::new())
                    .serve_connection(io, service)
                    .await
                {
                    warn!(%error, "daemon control HTTP connection failed");
                }
            });
        }
    }
}

#[derive(Clone, Copy)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

impl FileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        }
    }

    fn matches(self, metadata: &fs::Metadata) -> bool {
        metadata.dev() == self.dev && metadata.ino() == self.ino
    }
}

struct SocketPublication {
    final_path: PathBuf,
    staging_directory_path: PathBuf,
    staging_socket_path: PathBuf,
    staging_directory: File,
    staging_directory_identity: FileIdentity,
    socket_identity: Option<FileIdentity>,
    published: bool,
    committed: bool,
}

impl SocketPublication {
    fn create(final_path: &Path) -> Result<Self, ControlSocketError> {
        let parent = final_path.parent().ok_or(ControlSocketError::UnsafePath)?;
        for _ in 0..16 {
            let suffix = uuid::Uuid::new_v4().as_u128() as u32;
            let staging_directory_path = parent.join(format!(".t{suffix:08x}"));
            let mut builder = fs::DirBuilder::new();
            builder.mode(STAGING_DIRECTORY_MODE);
            match builder.create(&staging_directory_path) {
                Ok(()) => {
                    return Self::from_created_directory(
                        final_path.to_path_buf(),
                        staging_directory_path,
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(ControlSocketError::Bind(error)),
            }
        }
        Err(ControlSocketError::Bind(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate control socket staging directory",
        )))
    }

    fn from_created_directory(
        final_path: PathBuf,
        staging_directory_path: PathBuf,
    ) -> Result<Self, ControlSocketError> {
        let staging_directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&staging_directory_path)
            .map_err(ControlSocketError::Bind)?;
        let metadata = staging_directory
            .metadata()
            .map_err(ControlSocketError::Bind)?;
        if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(ControlSocketError::UnsafePath);
        }
        let staging_directory_identity = FileIdentity::from_metadata(&metadata);
        let publication = Self {
            staging_socket_path: staging_directory_path.join("s"),
            final_path,
            staging_directory_path,
            staging_directory,
            staging_directory_identity,
            socket_identity: None,
            published: false,
            committed: false,
        };
        publication.secure_staging_directory()?;
        Ok(publication)
    }

    fn secure_staging_directory(&self) -> Result<(), ControlSocketError> {
        if unsafe {
            libc::fchmod(
                self.staging_directory.as_raw_fd(),
                STAGING_DIRECTORY_MODE as libc::mode_t,
            )
        } != 0
        {
            return Err(ControlSocketError::Bind(io::Error::last_os_error()));
        }
        self.verify_staging_directory()
    }

    fn verify_staging_directory(&self) -> Result<(), ControlSocketError> {
        let opened = self
            .staging_directory
            .metadata()
            .map_err(ControlSocketError::Bind)?;
        let visible =
            fs::symlink_metadata(&self.staging_directory_path).map_err(ControlSocketError::Bind)?;
        if !opened.is_dir()
            || !visible.is_dir()
            || !self.staging_directory_identity.matches(&opened)
            || !self.staging_directory_identity.matches(&visible)
            || opened.uid() != unsafe { libc::geteuid() }
            || visible.uid() != unsafe { libc::geteuid() }
            || opened.mode() & 0o777 != STAGING_DIRECTORY_MODE
            || visible.mode() & 0o777 != STAGING_DIRECTORY_MODE
        {
            return Err(ControlSocketError::UnsafePath);
        }
        Ok(())
    }

    fn bind(&mut self) -> Result<StdUnixListener, ControlSocketError> {
        self.verify_staging_directory()?;
        let listener =
            StdUnixListener::bind(&self.staging_socket_path).map_err(ControlSocketError::Bind)?;
        let metadata =
            fs::symlink_metadata(&self.staging_socket_path).map_err(ControlSocketError::Bind)?;
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(ControlSocketError::UnsafePath);
        }
        let identity = FileIdentity::from_metadata(&metadata);
        self.socket_identity = Some(identity);
        fs::set_permissions(
            &self.staging_socket_path,
            fs::Permissions::from_mode(PRIVATE_MODE),
        )
        .map_err(ControlSocketError::Bind)?;
        self.verify_staged_socket(identity)?;
        self.verify_staging_directory()?;
        Ok(listener)
    }

    fn verify_staged_socket(&self, identity: FileIdentity) -> Result<(), ControlSocketError> {
        let metadata =
            fs::symlink_metadata(&self.staging_socket_path).map_err(ControlSocketError::Bind)?;
        if !metadata.file_type().is_socket()
            || !identity.matches(&metadata)
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != PRIVATE_MODE
        {
            return Err(ControlSocketError::UnsafePath);
        }
        Ok(())
    }

    fn publish(&mut self) -> Result<FileIdentity, ControlSocketError> {
        let identity = self.socket_identity.ok_or(ControlSocketError::UnsafePath)?;
        self.verify_staging_directory()?;
        self.verify_staged_socket(identity)?;
        match rename_noreplace(&self.staging_socket_path, &self.final_path) {
            Ok(()) => self.published = true,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(ControlSocketError::Collision);
            }
            Err(error) => return Err(ControlSocketError::Bind(error)),
        }
        let metadata = fs::symlink_metadata(&self.final_path).map_err(ControlSocketError::Bind)?;
        if !metadata.file_type().is_socket()
            || !identity.matches(&metadata)
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o777 != PRIVATE_MODE
        {
            return Err(ControlSocketError::UnsafePath);
        }
        self.remove_staging_directory()?;
        self.committed = true;
        Ok(identity)
    }

    fn remove_staging_directory(&self) -> Result<(), ControlSocketError> {
        let metadata = match fs::symlink_metadata(&self.staging_directory_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(ControlSocketError::Bind(error)),
        };
        if !metadata.is_dir()
            || !self.staging_directory_identity.matches(&metadata)
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(ControlSocketError::UnsafePath);
        }
        fs::remove_dir(&self.staging_directory_path).map_err(ControlSocketError::Bind)
    }
}

impl Drop for SocketPublication {
    fn drop(&mut self) {
        if !self.committed
            && let Some(identity) = self.socket_identity
        {
            let path = if self.published {
                &self.final_path
            } else {
                &self.staging_socket_path
            };
            remove_owned_socket(path, identity);
        }
        remove_owned_directory(
            &self.staging_directory_path,
            self.staging_directory_identity,
        );
    }
}

fn remove_owned_socket(path: &Path, identity: FileIdentity) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_socket()
        && identity.matches(&metadata)
        && metadata.uid() == unsafe { libc::geteuid() }
    {
        let _ = fs::remove_file(path);
    }
}

fn remove_owned_directory(path: &Path, identity: FileIdentity) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir()
        && identity.matches(&metadata)
        && metadata.uid() == unsafe { libc::geteuid() }
    {
        let _ = fs::remove_dir(path);
    }
}

#[cfg(target_os = "linux")]
fn rename_noreplace(source: &Path, target: &Path) -> io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let target = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "linux"))]
fn rename_noreplace(source: &Path, target: &Path) -> io::Result<()> {
    match fs::symlink_metadata(target) {
        Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::rename(source, target)
}

struct SocketOwnership {
    path: PathBuf,
    dev: u64,
    ino: u64,
    _lock: File,
}

impl Drop for SocketOwnership {
    fn drop(&mut self) {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_socket()
            && metadata.dev() == self.dev
            && metadata.ino() == self.ino
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn validated_socket_path(path: &Path) -> Result<PathBuf, ControlSocketError> {
    if !path.is_absolute()
        || path.file_name().is_none()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::CurDir | Component::Prefix(_)
            )
        })
    {
        return Err(ControlSocketError::UnsafePath);
    }
    let parent = path.parent().ok_or(ControlSocketError::UnsafePath)?;
    let mut current = open_directory(Path::new("/"))?;
    validate_ancestor(&current)?;
    for component in parent.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                current = open_directory_at(&current, name)?;
                validate_ancestor(&current)?;
            }
            _ => return Err(ControlSocketError::UnsafePath),
        }
    }
    let opened = current
        .metadata()
        .map_err(|_| ControlSocketError::UnsafePath)?;
    let visible = fs::symlink_metadata(parent).map_err(|_| ControlSocketError::UnsafePath)?;
    if !visible.is_dir()
        || visible.dev() != opened.dev()
        || visible.ino() != opened.ino()
        || opened.uid() != unsafe { libc::geteuid() }
        || opened.mode() & 0o022 != 0
    {
        return Err(ControlSocketError::UnsafePath);
    }
    Ok(path.to_path_buf())
}

fn open_directory(path: &Path) -> Result<File, ControlSocketError> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| ControlSocketError::UnsafePath)
}

fn open_directory_at(parent: &File, name: &OsStr) -> Result<File, ControlSocketError> {
    let name = CString::new(name.as_bytes()).map_err(|_| ControlSocketError::UnsafePath)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        Err(ControlSocketError::UnsafePath)
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn validate_ancestor(directory: &File) -> Result<(), ControlSocketError> {
    let metadata = directory
        .metadata()
        .map_err(|_| ControlSocketError::UnsafePath)?;
    let trusted_owner = metadata.uid() == unsafe { libc::geteuid() } || metadata.uid() == 0;
    let replaceable = metadata.mode() & 0o022 != 0 && metadata.mode() & libc::S_ISVTX == 0;
    if !metadata.is_dir() || !trusted_owner || replaceable {
        Err(ControlSocketError::UnsafePath)
    } else {
        Ok(())
    }
}

fn acquire_path_lock(path: &Path) -> Result<File, ControlSocketError> {
    let file_name = path.file_name().ok_or(ControlSocketError::UnsafePath)?;
    let mut lock_name = std::ffi::OsString::from(".");
    lock_name.push(file_name);
    lock_name.push(".lock");
    let lock_path = path.with_file_name(lock_name);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(PRIVATE_MODE)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(lock_path)
        .map_err(|_| ControlSocketError::UnsafePath)?;
    let metadata = file
        .metadata()
        .map_err(|_| ControlSocketError::UnsafePath)?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o777 != PRIVATE_MODE
        || metadata.nlink() != 1
    {
        return Err(ControlSocketError::UnsafePath);
    }
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(file);
    }
    let error = io::Error::last_os_error();
    if error
        .raw_os_error()
        .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
    {
        Err(ControlSocketError::LockHeld)
    } else {
        Err(ControlSocketError::UnsafePath)
    }
}

fn remove_verified_stale_socket(path: &Path) -> Result<(), ControlSocketError> {
    let inspected = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(ControlSocketError::Collision),
    };
    if !inspected.file_type().is_socket()
        || inspected.uid() != unsafe { libc::geteuid() }
        || inspected.mode() & 0o022 != 0
    {
        return Err(ControlSocketError::Collision);
    }
    match StdUnixStream::connect(path) {
        Ok(_) => return Err(ControlSocketError::Active),
        Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {}
        Err(_) => return Err(ControlSocketError::Collision),
    }
    let current = fs::symlink_metadata(path).map_err(|_| ControlSocketError::Collision)?;
    if !current.file_type().is_socket()
        || current.dev() != inspected.dev()
        || current.ino() != inspected.ino()
    {
        return Err(ControlSocketError::Collision);
    }
    fs::remove_file(path).map_err(|_| ControlSocketError::Collision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "termd-control-{label}-{}-{}",
                std::process::id(),
                Uuid::new_v4()
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn prepares_private_staged_socket_before_atomic_publish() {
        let dir = TestDir::new("staging");
        let path = dir.0.join("termd.sock");
        let mut publication = SocketPublication::create(&path).unwrap();
        let listener = publication.bind().unwrap();
        let staging_directory_path = publication.staging_directory_path.clone();

        assert!(matches!(
            fs::symlink_metadata(&path),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        ));
        let directory_metadata = fs::symlink_metadata(&staging_directory_path).unwrap();
        assert!(directory_metadata.is_dir());
        assert_eq!(directory_metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(directory_metadata.mode() & 0o777, STAGING_DIRECTORY_MODE);
        let socket_metadata = fs::symlink_metadata(&publication.staging_socket_path).unwrap();
        assert!(socket_metadata.file_type().is_socket());
        assert_eq!(socket_metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(socket_metadata.mode() & 0o777, PRIVATE_MODE);

        drop(listener);
        drop(publication);
        assert!(!staging_directory_path.exists());
        assert!(!path.exists());
    }

    #[test]
    fn publish_collision_preserves_unknown_path_and_cleans_staging() {
        let dir = TestDir::new("publish-collision");
        let path = dir.0.join("termd.sock");
        let mut publication = SocketPublication::create(&path).unwrap();
        let listener = publication.bind().unwrap();
        let staging_directory_path = publication.staging_directory_path.clone();
        fs::write(&path, b"collision").unwrap();

        assert!(matches!(
            publication.publish(),
            Err(ControlSocketError::Collision)
        ));
        drop(listener);
        drop(publication);

        assert_eq!(fs::read(&path).unwrap(), b"collision");
        assert!(!staging_directory_path.exists());
    }

    #[test]
    fn accepts_sticky_writable_ancestor() {
        let dir = TestDir::new("sticky-ancestor");
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o1777)).unwrap();
        let private_parent = dir.0.join("private");
        fs::create_dir(&private_parent).unwrap();
        fs::set_permissions(&private_parent, fs::Permissions::from_mode(0o700)).unwrap();
        let path = private_parent.join("termd.sock");

        assert_eq!(validated_socket_path(&path).unwrap(), path);
    }

    #[test]
    fn rejects_non_sticky_writable_ancestor() {
        let dir = TestDir::new("writable-ancestor");
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o777)).unwrap();
        let private_parent = dir.0.join("private");
        fs::create_dir(&private_parent).unwrap();
        fs::set_permissions(&private_parent, fs::Permissions::from_mode(0o700)).unwrap();
        let path = private_parent.join("termd.sock");

        assert!(matches!(
            validated_socket_path(&path),
            Err(ControlSocketError::UnsafePath)
        ));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn binds_private_socket_and_removes_only_its_own_inode() {
        let dir = TestDir::new("lifecycle");
        let path = dir.0.join("termd.sock");
        let server = ControlSocketServer::bind(&path, Router::new()).unwrap();
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.mode() & 0o777, 0o600);
        drop(StdUnixStream::connect(&path).unwrap());

        fs::remove_file(&path).unwrap();
        let replacement = StdUnixListener::bind(&path).unwrap();
        drop(server);
        assert!(path.exists());
        drop(replacement);
    }

    #[tokio::test]
    async fn removes_a_verified_stale_socket_but_not_live_or_non_socket_paths() {
        let dir = TestDir::new("stale");
        let stale_path = dir.0.join("stale.sock");
        drop(StdUnixListener::bind(&stale_path).unwrap());
        let stale = ControlSocketServer::bind(&stale_path, Router::new()).unwrap();
        drop(stale);
        assert!(!stale_path.exists());

        let live_path = dir.0.join("live.sock");
        let live = StdUnixListener::bind(&live_path).unwrap();
        assert!(matches!(
            ControlSocketServer::bind(&live_path, Router::new()),
            Err(ControlSocketError::Active)
        ));
        drop(live);

        let file_path = dir.0.join("file.sock");
        fs::write(&file_path, b"not a socket").unwrap();
        assert!(matches!(
            ControlSocketServer::bind(&file_path, Router::new()),
            Err(ControlSocketError::Collision)
        ));
    }
}
