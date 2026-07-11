//! termctl 的本地状态文件。
//!
//! 状态只保存设备身份、设备签名私钥 secret 和已配对 daemon 的公开身份。pairing token、
//! daemon/server private key 和终端明文输出都不属于客户端持久化范围。

use std::fmt;
use std::fs::{self, File};
#[cfg(unix)]
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use termd_proto::{DeviceId, PairAcceptPayload, PublicKey, ServerId, UnixTimestampMillis};

use crate::crypto;
use crate::error::{Result, TermctlError};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceState {
    pub device_id: DeviceId,
    pub device_public_key: PublicKey,
    pub device_signing_key_secret: String,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairedServerState {
    pub server_id: ServerId,
    pub daemon_public_key: PublicKey,
    pub url: String,
    pub paired_at_ms: UnixTimestampMillis,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SelectedServerTarget {
    pub server: PairedServerState,
    pub url: String,
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TermctlState {
    #[serde(default)]
    pub device: Option<DeviceState>,
    #[serde(default)]
    pub paired_servers: Vec<PairedServerState>,
    pub default_server_id: Option<ServerId>,
    pub default_url: Option<String>,
}

impl TermctlState {
    pub fn load(path: &Path) -> Result<Self> {
        let Some(raw) = load_state_file(path)? else {
            return Ok(Self::default());
        };
        let mut state: Self = serde_json::from_str(&raw).map_err(|_| TermctlError::StateRead)?;
        state.sanitize_persisted_urls();
        Ok(state)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut persisted = self.clone();
        persisted.sanitize_persisted_urls();
        save_state_file(path, &persisted)
    }

    pub fn ensure_device(&mut self) -> DeviceState {
        if let Some(device) = &self.device {
            return device.clone();
        }

        let generated = crypto::generate_device_identity();
        let device = DeviceState {
            device_id: generated.device_id,
            device_public_key: generated.device_public_key,
            device_signing_key_secret: generated.device_signing_key_secret,
        };
        self.device = Some(device.clone());
        device
    }

    pub fn require_device(&self) -> Result<DeviceState> {
        self.device.clone().ok_or(TermctlError::MissingPairing)
    }

    pub fn record_pairing(&mut self, accepted: PairAcceptPayload, url: String) {
        let url =
            normalize_persisted_ws_url(&url).unwrap_or_else(|| strip_sensitive_query_params(&url));
        let server = PairedServerState {
            server_id: accepted.server_id,
            daemon_public_key: accepted.daemon_public_key,
            url: url.clone(),
            paired_at_ms: crypto::now_ms(),
        };

        if let Some(existing) = self
            .paired_servers
            .iter_mut()
            .find(|existing| existing.server_id == server.server_id)
        {
            *existing = server;
        } else {
            self.paired_servers.push(server);
        }

        self.default_server_id = Some(accepted.server_id);
        self.default_url = Some(url);
    }

    fn sanitize_persisted_urls(&mut self) {
        for server in &mut self.paired_servers {
            server.url = strip_sensitive_query_params(&server.url);
        }
        if let Some(url) = &mut self.default_url {
            *url = strip_sensitive_query_params(url);
        }
    }

    pub fn paired_server(&self, server_id: ServerId) -> Option<PairedServerState> {
        self.paired_servers
            .iter()
            .find(|server| server.server_id == server_id)
            .cloned()
    }

    pub fn selected_route_server_id(&self) -> Option<ServerId> {
        if let Some(server_id) = self.default_server_id {
            return Some(server_id);
        }

        // 旧状态可能没有 default_server_id；只有唯一 daemon 时才自动选择，避免多 daemon
        // 场景中用 URL 猜路由身份。
        let mut server_ids = self.paired_servers.iter().map(|server| server.server_id);
        let first = server_ids.next()?;
        server_ids
            .all(|server_id| server_id == first)
            .then_some(first)
    }

    pub fn selected_url_for_server(
        &self,
        server_id: ServerId,
        requested_url: Option<&str>,
    ) -> Result<String> {
        if let Some(url) = requested_url {
            return normalize_ws_url(url).ok_or(TermctlError::InvalidWsUrl);
        }

        self.paired_server(server_id)
            .and_then(|server| normalize_ws_url(&server.url))
            .or_else(|| {
                self.default_url
                    .as_deref()
                    .and_then(normalize_ws_url)
                    .filter(|_| self.default_server_id == Some(server_id))
            })
            .or_else(|| normalize_ws_url(crate::cli::DEFAULT_URL))
            .ok_or(TermctlError::InvalidWsUrl)
    }

    pub fn selected_paired_target(
        &self,
        requested_url: Option<&str>,
    ) -> Result<SelectedServerTarget> {
        let requested_url = requested_url
            .map(|url| normalize_ws_url(url).ok_or(TermctlError::InvalidWsUrl))
            .transpose()?;
        let requested_match_key = requested_url.as_deref().and_then(normalized_url_match_key);

        // 如果 URL 与已保存 daemon 完全匹配，就用对应 server_id；这是读取本地状态，
        // 不是从 URL 结构中反推 server_id。
        if let Some(url) = requested_url.as_deref() {
            if let Some(default_server_id) = self.default_server_id
                && let Some(server) = self.paired_server(default_server_id)
                && normalized_url_match_key(&server.url)
                    .as_deref()
                    .zip(requested_match_key.as_deref())
                    .is_some_and(|(saved_url, requested_url)| saved_url == requested_url)
            {
                return Ok(SelectedServerTarget {
                    server,
                    url: url.to_owned(),
                });
            }

            if let Some(server) = self.paired_servers.iter().find(|server| {
                normalized_url_match_key(&server.url)
                    .as_deref()
                    .zip(requested_match_key.as_deref())
                    .is_some_and(|(saved_url, requested_url)| saved_url == requested_url)
            }) {
                return Ok(SelectedServerTarget {
                    server: server.clone(),
                    url: url.to_owned(),
                });
            }
        }

        let server_id = self
            .selected_route_server_id()
            .ok_or(TermctlError::MissingPairing)?;
        let server = self
            .paired_server(server_id)
            .ok_or(TermctlError::MissingPairing)?;
        let url = match requested_url {
            Some(url) => url,
            None => self.selected_url_for_server(server_id, None)?,
        };

        Ok(SelectedServerTarget { server, url })
    }
}

impl fmt::Debug for DeviceState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceState")
            .field("device_id", &self.device_id)
            .field("device_public_key", &self.device_public_key)
            .field("device_signing_key_secret", &"<redacted>")
            .finish()
    }
}

impl fmt::Debug for PairedServerState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let url = redact_url_for_debug(&self.url);
        formatter
            .debug_struct("PairedServerState")
            .field("server_id", &self.server_id)
            .field("daemon_public_key", &self.daemon_public_key)
            .field("url", &url)
            .field("paired_at_ms", &self.paired_at_ms)
            .finish()
    }
}

impl fmt::Debug for SelectedServerTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let url = redact_url_for_debug(&self.url);
        formatter
            .debug_struct("SelectedServerTarget")
            .field("server", &self.server)
            .field("url", &url)
            .finish()
    }
}

impl fmt::Debug for TermctlState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let default_url = self.default_url.as_deref().map(redact_url_for_debug);
        formatter
            .debug_struct("TermctlState")
            .field("device", &self.device)
            .field("paired_servers", &self.paired_servers)
            .field("default_server_id", &self.default_server_id)
            .field("default_url", &default_url)
            .finish()
    }
}

#[cfg(not(unix))]
fn unique_temp_state_path(parent: &Path, final_path: &Path) -> PathBuf {
    let file_name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("termctl-state.json");
    let now_ms = crypto::now_ms().0;

    for attempt in 0..100_u32 {
        let temp_path = parent.join(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            now_ms.saturating_add(u64::from(attempt))
        ));
        if !temp_path.exists() {
            return temp_path;
        }
    }

    parent.join(format!(".{file_name}.{}.fallback.tmp", std::process::id()))
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateSyncPoint {
    Directory,
    ParentDirectory,
    TempFile,
    ParentAfterRename,
}

#[cfg(unix)]
trait StateSync {
    fn sync(&mut self, file: &File, point: StateSyncPoint) -> std::io::Result<()>;
}

#[cfg(unix)]
struct OsStateSync;

#[cfg(unix)]
impl StateSync for OsStateSync {
    fn sync(&mut self, file: &File, _point: StateSyncPoint) -> std::io::Result<()> {
        file.sync_all()
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StateFileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
struct TempStateFile {
    file: File,
    name: std::ffi::OsString,
    identity: StateFileIdentity,
}

#[cfg(unix)]
fn load_state_file(path: &Path) -> Result<Option<String>> {
    let mut sync = OsStateSync;
    let (parent, target_name) = match open_state_parent(path, false, &mut sync) {
        Ok(opened) => opened,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(TermctlError::StateRead),
    };
    validate_state_parent(&parent).map_err(|_| TermctlError::StateRead)?;
    let mut file = match openat_file(
        &parent,
        &target_name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        0,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(TermctlError::StateRead),
    };
    validate_loadable_state_file(&file).map_err(|_| TermctlError::StateRead)?;

    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|_| TermctlError::StateRead)?;
    Ok(Some(raw))
}

#[cfg(not(unix))]
fn load_state_file(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(raw) => Ok(Some(raw)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(TermctlError::StateRead),
    }
}

#[cfg(unix)]
fn save_state_file(path: &Path, state: &TermctlState) -> Result<()> {
    let mut sync = OsStateSync;
    save_state_file_with_sync(path, state, &mut sync)
}

#[cfg(unix)]
fn save_state_file_with_sync(
    path: &Path,
    state: &TermctlState,
    sync: &mut dyn StateSync,
) -> Result<()> {
    let (parent, target_name) =
        open_state_parent(path, true, sync).map_err(|_| TermctlError::StateWrite)?;
    validate_state_parent(&parent).map_err(|_| TermctlError::StateWrite)?;
    let mut temp = create_unique_temp_state_file(&parent)?;

    let save_result = (|| -> Result<()> {
        serde_json::to_writer_pretty(&mut temp.file, state)
            .map_err(|_| TermctlError::StateWrite)?;
        temp.file
            .write_all(b"\n")
            .map_err(|_| TermctlError::StateWrite)?;
        temp.file.flush().map_err(|_| TermctlError::StateWrite)?;
        sync.sync(&temp.file, StateSyncPoint::TempFile)
            .map_err(|_| TermctlError::StateWrite)?;

        validate_named_state_file(&parent, &temp.name, Some(0o600), Some(temp.identity))?;
        validate_existing_state_target(&parent, &target_name)?;
        renameat_file(&parent, &temp.name, &target_name).map_err(|_| TermctlError::StateWrite)?;
        sync.sync(&parent, StateSyncPoint::ParentAfterRename)
            .map_err(|_| TermctlError::StateWrite)?;
        Ok(())
    })();

    if save_result.is_err() {
        cleanup_temp_if_unchanged(&parent, &temp);
    }
    save_result
}

#[cfg(not(unix))]
fn save_state_file(path: &Path, state: &TermctlState) -> Result<()> {
    use std::fs::OpenOptions;

    // 非 Unix 平台保留原有持久化行为，不声称提供 Unix owner/mode 保证。
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|_| TermctlError::StateWrite)?;
    }

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let temp_path = unique_temp_state_path(parent, path);
    let save_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)
            .map_err(|_| TermctlError::StateWrite)?;
        serde_json::to_writer_pretty(&mut file, state).map_err(|_| TermctlError::StateWrite)?;
        file.write_all(b"\n")
            .map_err(|_| TermctlError::StateWrite)?;
        file.flush().map_err(|_| TermctlError::StateWrite)?;
        file.sync_all().map_err(|_| TermctlError::StateWrite)?;
        drop(file);
        fs::rename(&temp_path, path).map_err(|_| TermctlError::StateWrite)?;
        if let Ok(directory) = File::open(parent) {
            directory.sync_all().map_err(|_| TermctlError::StateWrite)?;
        }
        Ok(())
    })();

    if save_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    save_result
}

#[cfg(unix)]
fn open_state_parent(
    path: &Path,
    create: bool,
    sync: &mut dyn StateSync,
) -> std::io::Result<(File, std::ffi::OsString)> {
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let target_name = path
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?
        .to_os_string();
    let parent = open_directory_path_nofollow(parent_path, create, sync)?;
    Ok((parent, target_name))
}

#[cfg(unix)]
fn open_directory_path_nofollow(
    path: &Path,
    create: bool,
    sync: &mut dyn StateSync,
) -> std::io::Result<File> {
    use std::path::Component;

    let mut current = if path.is_absolute() {
        open_directory_nofollow(Path::new("/"))?
    } else {
        open_directory_nofollow(Path::new("."))?
    };
    validate_state_ancestor(&current)?;

    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::ParentDir => std::ffi::OsStr::new(".."),
            Component::Normal(name) => name,
            Component::Prefix(_) => unreachable!("Unix paths do not have prefixes"),
        };
        let (next, created) = match open_directory_at_nofollow(&current, name) {
            Ok(next) => (next, false),
            Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                let created = match mkdirat_private(&current, name) {
                    Ok(()) => true,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
                    Err(error) => return Err(error),
                };
                (open_directory_at_nofollow(&current, name)?, created)
            }
            Err(error) => return Err(error),
        };
        if created {
            secure_created_directory(&next)?;
        }
        validate_state_ancestor(&next)?;
        if create {
            sync.sync(&next, StateSyncPoint::Directory)?;
            sync.sync(&current, StateSyncPoint::ParentDirectory)?;
        }
        current = next;
    }
    Ok(current)
}

#[cfg(unix)]
fn open_directory_nofollow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(unix)]
fn open_directory_at_nofollow(parent: &File, name: &std::ffi::OsStr) -> std::io::Result<File> {
    openat_file(
        parent,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )
}

#[cfg(unix)]
fn mkdirat_private(parent: &File, name: &std::ffi::OsStr) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let name = path_component_cstring(name)?;
    // SAFETY: parent and name remain valid for this call; mkdirat does not retain either value.
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn secure_created_directory(directory: &File) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = directory.metadata()?;
    if !metadata.file_type().is_dir() || metadata.uid() != current_euid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "created state directory changed ownership",
        ));
    }
    fchmod_file(directory, 0o700)?;
    let metadata = directory.metadata()?;
    if metadata.uid() != current_euid() || metadata.mode() & 0o7777 != 0o700 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "created state directory is not mode 0700",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_state_ancestor(directory: &File) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = directory.metadata()?;
    if !metadata.file_type().is_dir()
        || !state_directory_is_trusted(metadata.uid(), metadata.mode(), current_euid())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "state ancestor is not trusted",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn state_directory_is_trusted(uid: libc::uid_t, mode: u32, effective_uid: libc::uid_t) -> bool {
    let owner_is_trusted = uid == effective_uid || uid == 0;
    let group_or_world_writable = mode & 0o022 != 0;
    let sticky = mode & 0o1000 != 0;
    owner_is_trusted && (!group_or_world_writable || sticky)
}

#[cfg(unix)]
fn validate_state_parent(parent: &File) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = parent.metadata()?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != current_euid()
        || metadata.mode() & 0o022 != 0
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "state parent is not private and owned by the current user",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn create_unique_temp_state_file(parent: &File) -> Result<TempStateFile> {
    for attempt in 0..100_u32 {
        let name = std::ffi::OsString::from(format!(
            ".termctl-state.{}.{}.{}.tmp",
            std::process::id(),
            crypto::now_ms().0,
            attempt
        ));
        match openat_file(
            parent,
            &name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        ) {
            Ok(file) => {
                let identity = validate_opened_state_file(&file, None)
                    .map_err(|_| TermctlError::StateWrite)?;
                let temp = TempStateFile {
                    file,
                    name,
                    identity,
                };
                let secure_result = fchmod_file(&temp.file, 0o600)
                    .and_then(|()| validate_opened_state_file(&temp.file, Some(0o600)).map(drop));
                if secure_result.is_err() {
                    cleanup_temp_if_unchanged(parent, &temp);
                    return Err(TermctlError::StateWrite);
                }
                return Ok(temp);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(TermctlError::StateWrite),
        }
    }
    Err(TermctlError::StateWrite)
}

#[cfg(unix)]
fn validate_existing_state_target(parent: &File, name: &std::ffi::OsStr) -> Result<()> {
    let file = match openat_file(
        parent,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        0,
    ) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(TermctlError::StateWrite),
    };
    validate_opened_state_file(&file, None).map_err(|_| TermctlError::StateWrite)?;
    Ok(())
}

#[cfg(unix)]
fn validate_opened_state_file(
    file: &File,
    expected_mode: Option<u32>,
) -> std::io::Result<StateFileIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    if !metadata.file_type().is_file()
        || metadata.uid() != current_euid()
        || expected_mode.is_some_and(|mode| metadata.mode() & 0o7777 != mode)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "state file is not a private current-user regular file",
        ));
    }
    Ok(StateFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(unix)]
fn validate_loadable_state_file(file: &File) -> std::io::Result<StateFileIdentity> {
    use std::os::unix::fs::MetadataExt;

    let identity = validate_opened_state_file(file, None)?;
    let mode = file.metadata()?.mode() & 0o7777;
    if mode & 0o400 == 0 || mode & !0o700 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "state file is not owner-readable and private",
        ));
    }
    Ok(identity)
}

#[cfg(unix)]
fn validate_named_state_file(
    parent: &File,
    name: &std::ffi::OsStr,
    expected_mode: Option<u32>,
    expected_identity: Option<StateFileIdentity>,
) -> Result<StateFileIdentity> {
    let file = openat_file(
        parent,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        0,
    )
    .map_err(|_| TermctlError::StateWrite)?;
    let identity =
        validate_opened_state_file(&file, expected_mode).map_err(|_| TermctlError::StateWrite)?;
    if expected_identity.is_some_and(|expected| identity != expected) {
        return Err(TermctlError::StateWrite);
    }
    Ok(identity)
}

#[cfg(unix)]
fn cleanup_temp_if_unchanged(parent: &File, temp: &TempStateFile) {
    if validate_named_state_file(parent, &temp.name, None, Some(temp.identity)).is_ok() {
        let _ = unlinkat_file(parent, &temp.name);
    }
}

#[cfg(unix)]
fn openat_file(
    parent: &File,
    name: &std::ffi::OsStr,
    flags: i32,
    mode: u32,
) -> std::io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let name = path_component_cstring(name)?;
    // SAFETY: parent and name remain valid for this call; a successful call returns a new FD.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags,
            mode as libc::mode_t,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: openat returned this new owned descriptor, which is transferred to File once.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(unix)]
fn renameat_file(
    parent: &File,
    from: &std::ffi::OsStr,
    to: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let from = path_component_cstring(from)?;
    let to = path_component_cstring(to)?;
    // SAFETY: the directory FD and both names remain valid for the duration of renameat.
    let result = unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            from.as_ptr(),
            parent.as_raw_fd(),
            to.as_ptr(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn unlinkat_file(parent: &File, name: &std::ffi::OsStr) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let name = path_component_cstring(name)?;
    // SAFETY: parent and name remain valid for this call; unlinkat does not retain them.
    let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn fchmod_file(file: &File, mode: u32) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    // SAFETY: file owns a valid descriptor for the duration of fchmod.
    let result = unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn path_component_cstring(name: &std::ffi::OsStr) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
}

#[cfg(unix)]
fn current_euid() -> libc::uid_t {
    unsafe { libc::geteuid() }
}

fn normalize_persisted_ws_url(value: &str) -> Option<String> {
    normalize_ws_url(value).map(|url| strip_sensitive_query_params(&url))
}

fn normalized_url_match_key(value: &str) -> Option<String> {
    normalize_ws_url(value).map(|url| strip_sensitive_query_params(&url))
}

fn strip_sensitive_query_params(value: &str) -> String {
    let parsed = url::Url::parse(value).ok();
    let (base, fallback_query, _) = split_url_parts(value);
    // 优先让 Url 按结构定位 query；fallback 只服务旧的非标准输入保存路径。
    let query = parsed
        .as_ref()
        .and_then(|url| url.query())
        .or(fallback_query);
    let Some(query) = query else {
        return base.to_owned();
    };

    let kept = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| !query_pair_has_sensitive_key(pair))
        .collect::<Vec<_>>();

    rebuild_url_with_query(base, &kept)
}

fn redact_url_for_debug(value: &str) -> String {
    let parsed = url::Url::parse(value).ok();
    let (base, fallback_query, fragment) = split_url_parts(value);
    // Debug 只暴露脱敏后的敏感 query；普通 query 保持原始片段，方便定位配置问题。
    let query = parsed
        .as_ref()
        .and_then(|url| url.query())
        .or(fallback_query);
    let mut redacted_pairs = Vec::new();

    if let Some(query) = query {
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            if query_pair_has_sensitive_key(pair) {
                redacted_pairs.push(redacted_sensitive_query_pair(pair));
            } else {
                redacted_pairs.push(pair.to_owned());
            }
        }
    }

    let mut debug_url = rebuild_url_with_query(base, &redacted_pairs);
    if fragment.is_some() {
        debug_url.push_str("#<redacted>");
    }
    debug_url
}

fn query_pair_has_sensitive_key(pair: &str) -> bool {
    // form_urlencoded 负责 percent decode key，避免手写解码导致大小写编码遗漏。
    url::form_urlencoded::parse(pair.as_bytes())
        .next()
        .is_some_and(|(key, _)| is_sensitive_query_key(&key))
}

fn redacted_sensitive_query_pair(pair: &str) -> String {
    let key = url::form_urlencoded::parse(pair.as_bytes())
        .next()
        .map(|(key, _)| key.into_owned())
        .unwrap_or_default();
    format!("{key}=<redacted>")
}

fn split_url_parts(value: &str) -> (&str, Option<&str>, Option<&str>) {
    let (without_fragment, fragment) = match value.split_once('#') {
        Some((without_fragment, fragment)) => (without_fragment, Some(fragment)),
        None => (value, None),
    };
    let (base, query) = match without_fragment.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (without_fragment, None),
    };
    (base, query, fragment)
}

fn rebuild_url_with_query(base: &str, query_pairs: &[impl AsRef<str>]) -> String {
    if query_pairs.is_empty() {
        return base.to_owned();
    }

    let query = query_pairs
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}?{query}")
}

fn is_sensitive_query_key(key: &str) -> bool {
    let normalized = key.trim().to_ascii_lowercase().replace('-', "_");
    matches!(
        normalized.as_str(),
        "relay_token"
            | "token"
            | "access_token"
            | "refresh_token"
            | "session_token"
            | "data_token"
            | "authorization"
            | "auth"
            | "bearer"
    )
}

pub fn normalize_ws_url(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed != value
        || trimmed.chars().any(char::is_whitespace)
        || trimmed.contains('#')
    {
        return None;
    }

    let (scheme, rest) = if let Some(rest) = trimmed.strip_prefix("ws://") {
        ("ws", rest)
    } else if let Some(rest) = trimmed.strip_prefix("wss://") {
        ("wss", rest)
    } else {
        return None;
    };

    let (without_query, query) = match rest.split_once('?') {
        Some((without_query, query)) => (without_query, Some(query)),
        None => (rest, None),
    };
    let (authority, raw_path) = match without_query.split_once('/') {
        Some((authority, raw_path)) => (authority, raw_path),
        None => (without_query, ""),
    };
    if authority.is_empty() {
        return None;
    }

    let mut segments: Vec<&str> = raw_path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() {
        segments.push("ws");
    }

    // 兼容旧 relay client URL：`/ws/<server_id>/client` 或 `/prefix/ws/<server_id>/client`。
    // 这里只把传输入口收敛回 `/ws`，不会把路径里的 server_id 当成路由身份使用。
    if segments.len() >= 3
        && segments.last() == Some(&"client")
        && segments.get(segments.len().saturating_sub(3)) == Some(&"ws")
    {
        segments.truncate(segments.len() - 2);
    }

    if segments.last() != Some(&"ws")
        || segments
            .iter()
            .any(|segment| *segment == "." || *segment == "..")
    {
        return None;
    }

    let mut normalized = format!("{scheme}://{authority}/{}", segments.join("/"));
    if let Some(query) = query {
        normalized.push('?');
        normalized.push_str(query);
    }
    Some(normalized)
}

pub fn resolve_state_path(override_path: Option<PathBuf>) -> PathBuf {
    if let Some(path) = override_path {
        return path;
    }

    if let Some(path) = std::env::var_os("TERMD_CTL_STATE").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }

    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(home)
            .join(".termd")
            .join("termctl-state.json");
    }

    PathBuf::from(".termctl-state.json")
}

#[cfg(test)]
mod tests {
    use termd_proto::{PairAcceptPayload, PairingToken};

    use super::*;

    #[test]
    fn state_roundtrip_excludes_pairing_token_and_server_private_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut state = TermctlState::default();
        let device = state.ensure_device();

        state.record_pairing(
            PairAcceptPayload {
                server_id: ServerId::new(),
                daemon_public_key: PublicKey("daemon-public".to_owned()),
                device_id: device.device_id,
                expires_at_ms: UnixTimestampMillis(2_000),
            },
            "ws://127.0.0.1:8765/ws".to_owned(),
        );
        state.save(&path).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        let loaded = TermctlState::load(&path).unwrap();

        assert_eq!(loaded, state);
        assert!(raw.contains("device_signing_key_secret"));
        assert!(!raw.contains("pairing_token"));
        assert!(!raw.contains("server_private_key"));
        assert!(!raw.contains(&PairingToken("secret-token".to_owned()).0));
    }

    #[cfg(unix)]
    #[test]
    fn save_rejects_symlink_without_mutating_target_or_unrelated_temp() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let symlink_target = dir.path().join("must-not-be-overwritten.json");
        let unrelated_temp = dir.path().join(".state.json.unrelated.tmp");
        fs::write(&symlink_target, b"do-not-touch").unwrap();
        fs::write(&unrelated_temp, b"keep-me").unwrap();
        symlink(&symlink_target, &path).unwrap();

        let mut state = TermctlState::default();
        state.ensure_device();
        assert!(matches!(state.save(&path), Err(TermctlError::StateWrite)));

        assert_eq!(fs::read_to_string(&symlink_target).unwrap(), "do-not-touch");
        assert!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(&unrelated_temp).unwrap(), "keep-me");
        assert_eq!(
            fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_does_not_chmod_existing_parent_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.path().join("state.json");

        let mut state = TermctlState::default();
        state.ensure_device();
        state.save(&path).unwrap();

        assert_eq!(
            fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_rejects_group_or_world_writable_parent_without_chmod() {
        use std::os::unix::fs::PermissionsExt;

        for mode in [0o770, 0o707] {
            let dir = tempfile::tempdir().unwrap();
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(mode)).unwrap();
            let path = dir.path().join("state.json");
            let mut state = TermctlState::default();
            state.ensure_device();

            assert!(matches!(state.save(&path), Err(TermctlError::StateWrite)));
            assert_eq!(
                fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
                mode
            );
            assert!(!path.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn save_creates_every_missing_parent_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = first.join("second");
        let parent = second.join("state");
        let path = parent.join("state.json");
        let mut state = TermctlState::default();
        state.ensure_device();

        state.save(&path).unwrap();

        for directory in [&first, &second, &parent] {
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn save_rejects_non_regular_and_foreign_owned_targets() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let directory_target = dir.path().join("directory-target.json");
        fs::create_dir(&directory_target).unwrap();
        let mut state = TermctlState::default();
        state.ensure_device();
        assert!(matches!(
            state.save(&directory_target),
            Err(TermctlError::StateWrite)
        ));

        if unsafe { libc::geteuid() } == 0 {
            let foreign_target = dir.path().join("foreign.json");
            fs::write(&foreign_target, "{}").unwrap();
            fs::set_permissions(&foreign_target, fs::Permissions::from_mode(0o600)).unwrap();
            chown_for_test(&foreign_target, 65_534);

            assert!(matches!(
                state.save(&foreign_target),
                Err(TermctlError::StateWrite)
            ));
            assert_eq!(fs::read_to_string(&foreign_target).unwrap(), "{}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn save_rejects_foreign_owned_parent() {
        use std::os::unix::fs::PermissionsExt;

        if unsafe { libc::geteuid() } != 0 {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("foreign-parent");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let path = parent.join("state.json");
        fs::write(&path, "{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        chown_for_test(&parent, 65_534);
        let mut state = TermctlState::default();
        state.ensure_device();

        assert!(matches!(
            TermctlState::load(&path),
            Err(TermctlError::StateRead)
        ));
        assert!(matches!(state.save(&path), Err(TermctlError::StateWrite)));
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
    }

    #[cfg(unix)]
    #[test]
    fn save_rejects_foreign_owned_ancestors_regardless_of_mode() {
        use std::os::unix::fs::PermissionsExt;

        if unsafe { libc::geteuid() } != 0 {
            return;
        }

        for mode in [0o777, 0o555] {
            let root = tempfile::tempdir().unwrap();
            let foreign = root.path().join("foreign");
            let parent = foreign.join("private");
            fs::create_dir_all(&parent).unwrap();
            fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
            chown_for_test(&foreign, 65_534);
            fs::set_permissions(&foreign, fs::Permissions::from_mode(mode)).unwrap();
            let path = parent.join("state.json");
            let mut state = TermctlState::default();
            state.ensure_device();

            assert!(matches!(state.save(&path), Err(TermctlError::StateWrite)));
            assert!(!path.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn ancestor_trust_matches_daemon_owner_and_sticky_predicate() {
        let effective_uid = 1_000;

        assert!(state_directory_is_trusted(
            effective_uid,
            0o755,
            effective_uid
        ));
        assert!(state_directory_is_trusted(0, 0o755, effective_uid));
        assert!(state_directory_is_trusted(0, 0o1777, effective_uid));
        assert!(state_directory_is_trusted(
            effective_uid,
            0o1777,
            effective_uid
        ));
        assert!(!state_directory_is_trusted(0, 0o777, effective_uid));
        assert!(!state_directory_is_trusted(
            effective_uid,
            0o777,
            effective_uid
        ));
        assert!(!state_directory_is_trusted(65_534, 0o555, effective_uid));
        assert!(!state_directory_is_trusted(65_534, 0o1777, effective_uid));
    }

    #[cfg(unix)]
    #[test]
    fn save_allows_trusted_sticky_writable_ancestor() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let sticky = root.path().join("sticky");
        let parent = sticky.join("private");
        fs::create_dir(&sticky).unwrap();
        fs::set_permissions(&sticky, fs::Permissions::from_mode(0o1777)).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let path = parent.join("state.json");

        TermctlState::default().save(&path).unwrap();

        assert_eq!(TermctlState::load(&path).unwrap(), TermctlState::default());
    }

    #[cfg(unix)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TestSyncBehavior {
        Record,
        FailAt(StateSyncPoint),
    }

    #[cfg(unix)]
    struct TestStateSync {
        behavior: TestSyncBehavior,
        calls: Vec<StateSyncPoint>,
    }

    #[cfg(unix)]
    impl StateSync for TestStateSync {
        fn sync(&mut self, _file: &File, point: StateSyncPoint) -> std::io::Result<()> {
            self.calls.push(point);
            if self.behavior == TestSyncBehavior::FailAt(point) {
                return Err(std::io::Error::other("injected state sync failure"));
            }
            Ok(())
        }
    }

    #[cfg(unix)]
    struct FailDirectoryIdentitySync {
        point: StateSyncPoint,
        directory: PathBuf,
        failures_remaining: usize,
        matching_calls: usize,
    }

    #[cfg(unix)]
    impl StateSync for FailDirectoryIdentitySync {
        fn sync(&mut self, file: &File, point: StateSyncPoint) -> std::io::Result<()> {
            use std::os::unix::fs::MetadataExt;

            if point != self.point {
                return Ok(());
            }
            let expected = match fs::metadata(&self.directory) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
            let actual = file.metadata()?;
            if (expected.dev(), expected.ino()) != (actual.dev(), actual.ino()) {
                return Ok(());
            }

            self.matching_calls += 1;
            if self.failures_remaining > 0 {
                self.failures_remaining -= 1;
                return Err(std::io::Error::other("injected directory sync failure"));
            }
            Ok(())
        }
    }

    #[cfg(unix)]
    #[test]
    fn retry_resyncs_retained_directory_edge_after_mkdir_sync_failure() {
        for fail_child in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let child = root.path().join("new");
            let path = child.join("state.json");
            let mut sync = FailDirectoryIdentitySync {
                point: if fail_child {
                    StateSyncPoint::Directory
                } else {
                    StateSyncPoint::ParentDirectory
                },
                directory: if fail_child {
                    child.clone()
                } else {
                    root.path().to_path_buf()
                },
                failures_remaining: 2,
                matching_calls: 0,
            };

            assert!(matches!(
                save_state_file_with_sync(&path, &TermctlState::default(), &mut sync),
                Err(TermctlError::StateWrite)
            ));
            assert!(child.is_dir());
            assert!(!path.exists());

            assert!(matches!(
                save_state_file_with_sync(&path, &TermctlState::default(), &mut sync),
                Err(TermctlError::StateWrite)
            ));
            assert_eq!(sync.matching_calls, 2);
            assert!(!path.exists());

            save_state_file_with_sync(&path, &TermctlState::default(), &mut sync).unwrap();
            assert_eq!(sync.matching_calls, 3);
            assert_eq!(TermctlState::load(&path).unwrap(), TermctlState::default());
        }
    }

    #[cfg(unix)]
    #[test]
    fn save_syncs_complete_directory_chain_then_temp_and_renamed_leaf_in_order() {
        use std::path::Component;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("first/second/state/state.json");
        let mut sync = TestStateSync {
            behavior: TestSyncBehavior::Record,
            calls: Vec::new(),
        };

        save_state_file_with_sync(&path, &TermctlState::default(), &mut sync).unwrap();

        let traversed_edges = path
            .parent()
            .unwrap()
            .components()
            .filter(|component| matches!(component, Component::ParentDir | Component::Normal(_)))
            .count();
        let mut expected = Vec::with_capacity(traversed_edges * 2 + 2);
        for _ in 0..traversed_edges {
            expected.push(StateSyncPoint::Directory);
            expected.push(StateSyncPoint::ParentDirectory);
        }
        expected.push(StateSyncPoint::TempFile);
        expected.push(StateSyncPoint::ParentAfterRename);

        assert_eq!(sync.calls, expected);
    }

    #[cfg(unix)]
    #[test]
    fn save_reports_every_injected_sync_failure() {
        for fail_at in [
            StateSyncPoint::Directory,
            StateSyncPoint::ParentDirectory,
            StateSyncPoint::TempFile,
            StateSyncPoint::ParentAfterRename,
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("new/state.json");
            let mut sync = TestStateSync {
                behavior: TestSyncBehavior::FailAt(fail_at),
                calls: Vec::new(),
            };

            assert!(matches!(
                save_state_file_with_sync(&path, &TermctlState::default(), &mut sync),
                Err(TermctlError::StateWrite)
            ));
            assert!(sync.calls.contains(&fail_at));
        }
    }

    #[cfg(unix)]
    struct ReplaceTempOnSync {
        parent: PathBuf,
        replacement: Option<PathBuf>,
        moved_original: PathBuf,
    }

    #[cfg(unix)]
    impl StateSync for ReplaceTempOnSync {
        fn sync(&mut self, _file: &File, point: StateSyncPoint) -> std::io::Result<()> {
            if point != StateSyncPoint::TempFile {
                return Ok(());
            }

            let temp = fs::read_dir(&self.parent)?
                .filter_map(|entry| entry.ok())
                .find(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    name.starts_with(".termctl-state.") && name.ends_with(".tmp")
                })
                .ok_or_else(|| std::io::Error::other("state temp not found"))?
                .path();
            fs::rename(&temp, &self.moved_original)?;
            fs::write(&temp, b"replacement-must-remain")?;
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
            self.replacement = Some(temp);
            Ok(())
        }
    }

    #[cfg(unix)]
    #[test]
    fn replaced_temp_name_is_not_renamed_or_removed() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.json");
        fs::write(&path, b"original-target").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let moved_original = root.path().join("moved-original.tmp");
        let mut sync = ReplaceTempOnSync {
            parent: root.path().to_path_buf(),
            replacement: None,
            moved_original: moved_original.clone(),
        };

        assert!(matches!(
            save_state_file_with_sync(&path, &TermctlState::default(), &mut sync),
            Err(TermctlError::StateWrite)
        ));

        assert_eq!(fs::read_to_string(&path).unwrap(), "original-target");
        let replacement = sync.replacement.unwrap();
        assert_eq!(
            fs::read_to_string(&replacement).unwrap(),
            "replacement-must-remain"
        );
        assert!(moved_original.exists());
    }

    #[cfg(unix)]
    #[test]
    fn save_uses_open_parent_after_path_is_replaced() {
        use std::thread;
        use std::time::{Duration, Instant};

        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("state-dir");
        let moved_parent = root.path().join("opened-state-dir");
        fs::create_dir(&parent).unwrap();
        let path = parent.join("state.json");
        let state = TermctlState {
            default_url: Some(format!(
                "ws://localhost/ws?padding={}",
                "x".repeat(8 * 1024 * 1024)
            )),
            ..TermctlState::default()
        };

        let watched_parent = parent.clone();
        let replacement_parent = parent.clone();
        let moved = moved_parent.clone();
        let replacer = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let temp_exists = fs::read_dir(&watched_parent)
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| entry.ok())
                    .any(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"));
                if temp_exists {
                    fs::rename(&watched_parent, &moved).unwrap();
                    fs::create_dir(&replacement_parent).unwrap();
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "state temp file was not observed"
                );
                thread::yield_now();
            }
        });

        state.save(&path).unwrap();
        replacer.join().unwrap();

        assert!(!path.exists());
        assert_eq!(
            TermctlState::load(&moved_parent.join("state.json")).unwrap(),
            state
        );
    }

    #[cfg(unix)]
    #[test]
    fn save_replaces_owned_file_with_mode_0600_and_persists_atomically() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, "stale").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        let mut state = TermctlState::default();
        state.ensure_device();

        state.save(&path).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(TermctlState::load(&path).unwrap(), state);
    }

    #[test]
    fn missing_state_loads_as_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");

        assert_eq!(TermctlState::load(&path).unwrap(), TermctlState::default());
    }

    #[cfg(unix)]
    #[test]
    fn load_rejects_symlink_non_regular_and_loose_permission_state_files() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempfile::tempdir().unwrap();

        let target = dir.path().join("target.json");
        fs::write(&target, "{}").unwrap();
        let symlink_path = dir.path().join("state-symlink.json");
        symlink(&target, &symlink_path).unwrap();
        assert!(matches!(
            TermctlState::load(&symlink_path),
            Err(TermctlError::StateRead)
        ));

        assert!(matches!(
            TermctlState::load(dir.path()),
            Err(TermctlError::StateRead)
        ));

        let world_readable = dir.path().join("world-readable.json");
        fs::write(&world_readable, "{}").unwrap();
        fs::set_permissions(&world_readable, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            TermctlState::load(&world_readable),
            Err(TermctlError::StateRead)
        ));

        let group_writable = dir.path().join("group-writable.json");
        fs::write(&group_writable, "{}").unwrap();
        fs::set_permissions(&group_writable, fs::Permissions::from_mode(0o620)).unwrap();
        assert!(matches!(
            TermctlState::load(&group_writable),
            Err(TermctlError::StateRead)
        ));

        if unsafe { libc::geteuid() } == 0 {
            let foreign_owned = dir.path().join("foreign-owned.json");
            fs::write(&foreign_owned, "{}").unwrap();
            fs::set_permissions(&foreign_owned, fs::Permissions::from_mode(0o600)).unwrap();
            chown_for_test(&foreign_owned, 65_534);
            assert!(matches!(
                TermctlState::load(&foreign_owned),
                Err(TermctlError::StateRead)
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn load_accepts_owner_read_only_state_and_save_replaces_it_with_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner-read-only.json");
        let mut state = TermctlState::default();
        state.ensure_device();
        fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();

        assert_eq!(TermctlState::load(&path).unwrap(), state);
        state.save(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(TermctlState::load(&path).unwrap(), state);
    }

    #[cfg(unix)]
    fn chown_for_test(path: &Path, uid: libc::uid_t) {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(
            unsafe { libc::chown(path.as_ptr(), uid, uid as libc::gid_t) },
            0
        );
    }

    #[test]
    fn normalizes_legacy_relay_client_urls_to_unified_ws_endpoint() {
        assert_eq!(
            normalize_ws_url(
                "wss://relay.example/termd/ws/00000000-0000-0000-0000-000000000001/client?relay_token=redacted"
            )
            .unwrap(),
            "wss://relay.example/termd/ws?relay_token=redacted"
        );
        assert_eq!(
            normalize_ws_url("ws://127.0.0.1:8765").unwrap(),
            "ws://127.0.0.1:8765/ws"
        );
        assert!(normalize_ws_url("https://relay.example/ws").is_none());
        assert!(normalize_ws_url("wss://relay.example/ws#fragment").is_none());
    }

    #[test]
    fn record_pairing_strips_sensitive_query_params_but_keeps_regular_query() {
        let mut state = TermctlState::default();
        let device = state.ensure_device();

        state.record_pairing(
            PairAcceptPayload {
                server_id: ServerId::new(),
                daemon_public_key: PublicKey("daemon-public".to_owned()),
                device_id: device.device_id,
                expires_at_ms: UnixTimestampMillis(2_000),
            },
            "wss://relay.example/ws?relay_token=secret&region=cn&token=drop-me".to_owned(),
        );

        assert_eq!(
            state.paired_servers[0].url,
            "wss://relay.example/ws?region=cn"
        );
        assert_eq!(
            state.default_url.as_deref(),
            Some("wss://relay.example/ws?region=cn")
        );
    }

    #[test]
    fn record_pairing_strips_percent_encoded_relay_token_keys_and_keeps_regular_query() {
        let cases = [
            ("relay%5Ftoken", "secret-a"),
            ("relay%5ftoken", "secret-b"),
            ("%72%65%6c%61%79%5f%74%6f%6b%65%6e", "secret-c"),
        ];

        for (encoded_key, secret) in cases {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("state.json");
            let mut state = TermctlState::default();
            let device = state.ensure_device();

            state.record_pairing(
                PairAcceptPayload {
                    server_id: ServerId::new(),
                    daemon_public_key: PublicKey("daemon-public".to_owned()),
                    device_id: device.device_id,
                    expires_at_ms: UnixTimestampMillis(2_000),
                },
                format!(
                    "wss://relay.example/ws?region=cn&{encoded_key}={secret}&relay_token_hint=keep"
                ),
            );
            state.save(&path).unwrap();

            let raw = fs::read_to_string(&path).unwrap();
            assert_eq!(
                state.paired_servers[0].url,
                "wss://relay.example/ws?region=cn&relay_token_hint=keep"
            );
            assert_eq!(
                state.default_url.as_deref(),
                Some("wss://relay.example/ws?region=cn&relay_token_hint=keep")
            );
            assert!(!raw.contains(secret));
            assert!(!format!("{state:?}").contains(secret));
        }
    }

    #[test]
    fn legacy_state_load_and_save_removes_encoded_relay_tokens_preserving_regular_query() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let first_server_id = ServerId::new();
        let second_server_id = ServerId::new();
        let legacy = TermctlState {
            paired_servers: vec![
                PairedServerState {
                    server_id: first_server_id,
                    daemon_public_key: PublicKey("daemon-first".to_owned()),
                    url: "wss://relay.example/ws?region=cn&relay%5Ftoken=secret-a&filter=a%2Bb+z&relay_token_hint=keep".to_owned(),
                    paired_at_ms: UnixTimestampMillis(1),
                },
                PairedServerState {
                    server_id: second_server_id,
                    daemon_public_key: PublicKey("daemon-second".to_owned()),
                    url: "wss://relay.example/ws?%72%65%6c%61%79%5f%74%6f%6b%65%6e=secret-b&region=us&x=1%202".to_owned(),
                    paired_at_ms: UnixTimestampMillis(2),
                },
            ],
            default_server_id: Some(first_server_id),
            default_url: Some(
                "wss://relay.example/ws?relay_token=secret-c&region=cn&filter=a%2Bb+z"
                    .to_owned(),
            ),
            ..TermctlState::default()
        };
        fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let loaded = TermctlState::load(&path).unwrap();

        assert_eq!(
            loaded.paired_servers[0].url,
            "wss://relay.example/ws?region=cn&filter=a%2Bb+z&relay_token_hint=keep"
        );
        assert_eq!(
            loaded.paired_servers[1].url,
            "wss://relay.example/ws?region=us&x=1%202"
        );
        assert_eq!(
            loaded.default_url.as_deref(),
            Some("wss://relay.example/ws?region=cn&filter=a%2Bb+z")
        );
        assert!(!format!("{loaded:?}").contains("secret-"));

        loaded.save(&path).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("secret-a"));
        assert!(!raw.contains("secret-b"));
        assert!(!raw.contains("secret-c"));
        assert!(raw.contains("a%2Bb+z"));
        assert!(raw.contains("relay_token_hint=keep"));
    }

    #[test]
    fn save_sanitizes_legacy_server_urls_before_serializing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let state = TermctlState {
            paired_servers: vec![PairedServerState {
                server_id: ServerId::new(),
                daemon_public_key: PublicKey("daemon".to_owned()),
                url: "wss://relay.example/ws?region=cn&relay%5Ftoken=secret&x=a%2Bb".to_owned(),
                paired_at_ms: UnixTimestampMillis(1),
            }],
            ..TermctlState::default()
        };

        state.save(&path).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("secret"));
        assert!(raw.contains("region=cn&x=a%2Bb"));
        assert!(state.paired_servers[0].url.contains("secret"));
    }

    #[test]
    fn selected_paired_target_matches_runtime_url_after_stripping_secret_query() {
        let server_id = ServerId::new();
        let state = TermctlState {
            paired_servers: vec![PairedServerState {
                server_id,
                daemon_public_key: PublicKey("daemon-public".to_owned()),
                url: "wss://relay.example/ws?region=cn".to_owned(),
                paired_at_ms: UnixTimestampMillis(1),
            }],
            default_server_id: Some(server_id),
            default_url: Some("wss://relay.example/ws?region=cn".to_owned()),
            ..TermctlState::default()
        };

        let target = state
            .selected_paired_target(Some("wss://relay.example/ws?relay_token=secret&region=cn"))
            .unwrap();

        assert_eq!(target.server.server_id, server_id);
        assert_eq!(
            target.url,
            "wss://relay.example/ws?relay_token=secret&region=cn"
        );
    }

    #[test]
    fn selected_paired_target_matches_percent_encoded_relay_token_keys() {
        let default_server_id = ServerId::new();
        let matched_server_id = ServerId::new();
        let state = TermctlState {
            paired_servers: vec![
                PairedServerState {
                    server_id: default_server_id,
                    daemon_public_key: PublicKey("daemon-public-default".to_owned()),
                    url: "wss://relay.example/ws?region=us".to_owned(),
                    paired_at_ms: UnixTimestampMillis(1),
                },
                PairedServerState {
                    server_id: matched_server_id,
                    daemon_public_key: PublicKey("daemon-public-matched".to_owned()),
                    url: "wss://relay.example/ws?region=cn".to_owned(),
                    paired_at_ms: UnixTimestampMillis(2),
                },
            ],
            default_server_id: Some(default_server_id),
            default_url: Some("wss://relay.example/ws?region=us".to_owned()),
            ..TermctlState::default()
        };

        for encoded_key in [
            "relay%5Ftoken",
            "relay%5ftoken",
            "%72%65%6c%61%79%5f%74%6f%6b%65%6e",
        ] {
            let runtime_url = format!("wss://relay.example/ws?{encoded_key}=secret&region=cn");
            let target = state.selected_paired_target(Some(&runtime_url)).unwrap();

            assert_eq!(target.server.server_id, matched_server_id);
            assert_eq!(target.url, runtime_url);
        }
    }

    #[test]
    fn debug_redacts_state_secret_and_url_secret_fields() {
        let server_id = ServerId::new();
        let state = TermctlState {
            device: Some(DeviceState {
                device_id: DeviceId::new(),
                device_public_key: PublicKey("device-public".to_owned()),
                device_signing_key_secret: "super-secret-signing-key".to_owned(),
            }),
            paired_servers: vec![PairedServerState {
                server_id,
                daemon_public_key: PublicKey("daemon-public".to_owned()),
                url: "wss://relay.example/ws?relay_token=secret&region=cn#fragment-secret"
                    .to_owned(),
                paired_at_ms: UnixTimestampMillis(1),
            }],
            default_server_id: Some(server_id),
            default_url: Some(
                "wss://relay.example/ws?relay_token=secret&region=cn#fragment-secret".to_owned(),
            ),
        };

        let debug = format!("{state:?}");

        assert!(!debug.contains("super-secret-signing-key"));
        assert!(!debug.contains("fragment-secret"));
        assert!(!debug.contains("relay_token=secret"));
        assert!(debug.contains("device_signing_key_secret"));
        assert!(debug.contains("relay_token=<redacted>"));
        assert!(debug.contains("region=cn"));
        assert!(debug.contains("#<redacted>"));
    }

    #[test]
    fn debug_redacts_percent_encoded_relay_token_query_keys() {
        let server_id = ServerId::new();
        let state = TermctlState {
            paired_servers: vec![PairedServerState {
                server_id,
                daemon_public_key: PublicKey("daemon-public".to_owned()),
                url: "wss://relay.example/ws?relay%5Ftoken=secret-a&relay%5ftoken=secret-b&%72%65%6c%61%79%5f%74%6f%6b%65%6e=secret-c&region=cn".to_owned(),
                paired_at_ms: UnixTimestampMillis(1),
            }],
            default_server_id: Some(server_id),
            default_url: Some(
                "wss://relay.example/ws?relay%5Ftoken=secret-a&region=cn".to_owned(),
            ),
            ..TermctlState::default()
        };

        let debug = format!("{state:?}");

        assert!(!debug.contains("secret-a"));
        assert!(!debug.contains("secret-b"));
        assert!(!debug.contains("secret-c"));
        assert!(debug.contains("region=cn"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn selects_paired_target_from_saved_state_without_url_route_inference() {
        let first_server_id = ServerId::new();
        let second_server_id = ServerId::new();
        let first = PairedServerState {
            server_id: first_server_id,
            daemon_public_key: PublicKey("daemon-public-1".to_owned()),
            url: "wss://relay.example/first/ws".to_owned(),
            paired_at_ms: UnixTimestampMillis(1),
        };
        let second = PairedServerState {
            server_id: second_server_id,
            daemon_public_key: PublicKey("daemon-public-2".to_owned()),
            url: "wss://relay.example/second/ws".to_owned(),
            paired_at_ms: UnixTimestampMillis(2),
        };
        let state = TermctlState {
            paired_servers: vec![first.clone(), second],
            default_server_id: Some(second_server_id),
            ..TermctlState::default()
        };

        let target = state
            .selected_paired_target(Some("wss://relay.example/first/ws"))
            .unwrap();

        assert_eq!(target.server.server_id, first_server_id);
        assert_eq!(target.url, first.url);
    }

    #[test]
    fn selects_default_paired_target_when_multiple_servers_share_the_same_url() {
        let first_server_id = ServerId::new();
        let second_server_id = ServerId::new();
        let shared_url = "wss://relay.example/shared/ws?relay_token=abc".to_owned();
        let first = PairedServerState {
            server_id: first_server_id,
            daemon_public_key: PublicKey("daemon-public-1".to_owned()),
            url: shared_url.clone(),
            paired_at_ms: UnixTimestampMillis(1),
        };
        let second = PairedServerState {
            server_id: second_server_id,
            daemon_public_key: PublicKey("daemon-public-2".to_owned()),
            url: shared_url.clone(),
            paired_at_ms: UnixTimestampMillis(2),
        };
        let state = TermctlState {
            paired_servers: vec![first, second.clone()],
            default_server_id: Some(second_server_id),
            default_url: Some(shared_url.clone()),
            ..TermctlState::default()
        };

        let target = state.selected_paired_target(Some(&shared_url)).unwrap();

        assert_eq!(target.server.server_id, second_server_id);
        assert_eq!(target.url, shared_url);
    }
}
