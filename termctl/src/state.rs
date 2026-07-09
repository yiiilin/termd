//! termctl 的本地状态文件。
//!
//! 状态只保存设备身份、设备签名私钥 secret 和已配对 daemon 的公开身份。pairing token、
//! daemon/server private key 和终端明文输出都不属于客户端持久化范围。

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
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
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(_) => return Err(TermctlError::StateRead),
            Ok(_) => {}
        }

        let raw = load_state_file(path)?;
        serde_json::from_str(&raw).map_err(|_| TermctlError::StateRead)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            let parent_existed = parent.exists();
            fs::create_dir_all(parent).map_err(|_| TermctlError::StateWrite)?;
            if !parent_existed {
                set_owner_only_dir_permissions(parent)?;
            }
        }

        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let temp_path = unique_temp_state_path(parent, path);
        let save_result = (|| -> Result<()> {
            let open_options = secure_state_file_open_options();
            let mut file = open_options
                .open(&temp_path)
                .map_err(|_| TermctlError::StateWrite)?;
            set_owner_only_file_permissions(&temp_path)?;

            serde_json::to_writer_pretty(&mut file, self).map_err(|_| TermctlError::StateWrite)?;
            file.write_all(b"\n")
                .map_err(|_| TermctlError::StateWrite)?;
            file.flush().map_err(|_| TermctlError::StateWrite)?;
            file.sync_all().map_err(|_| TermctlError::StateWrite)?;
            drop(file);

            // 同目录 rename 是本地文件系统上的原子替换；不会跟随目标 symlink。
            fs::rename(&temp_path, path).map_err(|_| TermctlError::StateWrite)?;
            fsync_parent_dir(parent)?;
            Ok(())
        })();

        if save_result.is_err() {
            let _ = fs::remove_file(&temp_path);
        }

        save_result
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
fn load_state_file(path: &Path) -> Result<String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(path).map_err(|_| TermctlError::StateRead)?;
    let metadata = file.metadata().map_err(|_| TermctlError::StateRead)?;

    if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(TermctlError::StateRead);
    }

    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|_| TermctlError::StateRead)?;
    Ok(raw)
}

#[cfg(not(unix))]
fn load_state_file(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|_| TermctlError::StateRead)
}

fn fsync_parent_dir(path: &Path) -> Result<()> {
    // 目录 fsync 确保 rename 元数据落盘；不支持目录 open 的平台保留文件级 fsync。
    match File::open(path) {
        Ok(dir) => dir.sync_all().map_err(|_| TermctlError::StateWrite),
        Err(_) => Ok(()),
    }
}

fn secure_state_file_open_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    secure_state_file_open_options_platform(&mut options);
    options
}

#[cfg(unix)]
fn secure_state_file_open_options_platform(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;

    // 临时文件创建时即 0600，避免 create 后 chmod 前的短窗口泄露本机设备私钥。
    options.mode(0o600);
}

#[cfg(not(unix))]
fn secure_state_file_open_options_platform(_options: &mut OpenOptions) {}

fn normalize_persisted_ws_url(value: &str) -> Option<String> {
    normalize_ws_url(value).map(|url| strip_sensitive_query_params(&url))
}

fn normalized_url_match_key(value: &str) -> Option<String> {
    normalize_ws_url(value).map(|url| strip_sensitive_query_params(&url))
}

fn strip_sensitive_query_params(value: &str) -> String {
    let (base, query, _) = split_url_parts(value);
    let Some(query) = query else {
        return base.to_owned();
    };

    let kept = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            let key = pair.split_once('=').map(|(key, _)| key).unwrap_or(pair);
            !is_sensitive_query_key(key)
        })
        .collect::<Vec<_>>();

    rebuild_url_with_query(base, &kept)
}

fn redact_url_for_debug(value: &str) -> String {
    let (base, query, fragment) = split_url_parts(value);
    let mut redacted_pairs = Vec::new();

    if let Some(query) = query {
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let key = pair.split_once('=').map(|(key, _)| key).unwrap_or(pair);
            if is_sensitive_query_key(key) {
                redacted_pairs.push(format!("{key}=<redacted>"));
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

#[cfg(unix)]
fn set_owner_only_dir_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| TermctlError::StateWrite)
}

#[cfg(not(unix))]
fn set_owner_only_dir_permissions(_path: &Path) -> Result<()> {
    // 非 Unix 平台的 owner-only ACL 处理留给后续跨平台硬化；MVP 不放宽保存内容边界。
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_file_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| TermctlError::StateWrite)
}

#[cfg(not(unix))]
fn set_owner_only_file_permissions(_path: &Path) -> Result<()> {
    // 非 Unix 平台不强行模拟 chmod，避免错误地给出不可靠的访问语义。
    Ok(())
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
    fn save_replaces_symlink_atomically_without_mutating_target() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let symlink_target = dir.path().join("must-not-be-overwritten.json");
        fs::write(&symlink_target, b"do-not-touch").unwrap();
        symlink(&symlink_target, &path).unwrap();

        let mut state = TermctlState::default();
        state.ensure_device();
        state.save(&path).unwrap();

        assert_eq!(fs::read_to_string(&symlink_target).unwrap(), "do-not-touch");
        assert!(
            !fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(TermctlState::load(&path).unwrap(), state);
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
