//! termctl 的本地状态文件。
//!
//! 状态只保存设备身份、设备签名私钥 secret 和已配对 daemon 的公开身份。pairing token、
//! daemon/server private key 和终端明文输出都不属于客户端持久化范围。

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use termd_proto::{DeviceId, PairAcceptPayload, PublicKey, ServerId, UnixTimestampMillis};

use crate::crypto;
use crate::error::{Result, TermctlError};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceState {
    pub device_id: DeviceId,
    pub device_public_key: PublicKey,
    pub device_signing_key_secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairedServerState {
    pub server_id: ServerId,
    pub daemon_public_key: PublicKey,
    pub url: String,
    pub paired_at_ms: UnixTimestampMillis,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
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
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(path).map_err(|_| TermctlError::StateRead)?;
        serde_json::from_str(&raw).map_err(|_| TermctlError::StateRead)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|_| TermctlError::StateWrite)?;
            set_owner_only_dir_permissions(parent)?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .map_err(|_| TermctlError::StateWrite)?;
        set_owner_only_file_permissions(path)?;

        serde_json::to_writer_pretty(&mut file, self).map_err(|_| TermctlError::StateWrite)?;
        file.write_all(b"\n")
            .map_err(|_| TermctlError::StateWrite)?;
        Ok(())
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

    pub fn selected_url(&self, requested_url: Option<&str>) -> String {
        requested_url
            .map(ToOwned::to_owned)
            .or_else(|| self.default_url.clone())
            .unwrap_or_else(|| crate::cli::DEFAULT_URL.to_owned())
    }
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

    #[test]
    fn missing_state_loads_as_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");

        assert_eq!(TermctlState::load(&path).unwrap(), TermctlState::default());
    }
}
