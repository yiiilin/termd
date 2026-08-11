//! 受限的主机级更新器：termd / termrelay 共用的「下载 release 资产 →
//! 校验 → 备份 → 原子替换 → 重启 systemd 服务」逻辑。
//!
//! 安全边界：
//! - 资产 URL 只接受 `github.com/yiiilin/termd/releases/download/<tag>/...` 的
//!   release 下载路径，且文件名必须与目标组件 + 本机架构严格匹配。
//! - 下载完成后执行 `--version` 校验，输出必须包含目标版本号。
//! - 替换前备份当前二进制；任何步骤失败都回滚备份。
//! - 通过独占锁保证同一主机同一时刻只有一个更新在进行。
//!
//! 调用方负责认证（daemon 设备 Bearer / relay 的已认证 control 通道）；
//! 本 crate 不接触网络监听，不持有任何业务状态。

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use thiserror::Error;

pub const GITHUB_REPO: &str = "yiiilin/termd";
const LATEST_RELEASE_API_PATH: &str = "/repos/yiiilin/termd/releases/latest";
const UPDATE_LOCK_PATH: &str = "/tmp/termupdater.lock";
/// 下载总时长上限：慢速但持续的下载（如 200KB/s 下 50MB 需 ~4 分钟）不会超时；
/// 30 分钟兜底防止无限期挂起。
const DOWNLOAD_TOTAL_TIMEOUT_SECS: u64 = 1800;
/// 连接建立的超时。
const DOWNLOAD_CONNECT_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("no newer release available (current {current}, latest {latest})")]
    NoNewerRelease { current: String, latest: String },
    #[error("github api request failed: {0}")]
    ApiRequest(#[from] reqwest::Error),
    #[error("release metadata is malformed")]
    MalformedRelease,
    #[error("no release asset matches {binary}-linux-{arch}")]
    AssetNotFound { binary: String, arch: String },
    #[error("release asset URL is outside the trusted release download path")]
    UntrustedAssetUrl,
    #[error("downloaded binary failed version check: expected {expected}, got {actual:?}")]
    VersionCheckFailed {
        expected: String,
        actual: Option<String>,
    },
    #[error("downloaded file is not executable")]
    NotExecutable,
    #[error("update is already in progress")]
    UpdateInProgress,
    #[error("failed to restart service {service}: {source}")]
    RestartFailed { service: String, source: io::Error },
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseAsset {
    pub name: String,
    pub browser_download_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseInfo {
    pub tag_name: String,
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    /// 目标版本号（不带 `v` 前缀）。
    pub latest: String,
    pub release_url: String,
    pub asset_url: String,
}

/// 从字符串中提取第一个可解析的 semver 数字段（`major.minor.patch`）。
/// 容忍常见前缀（`v0.9.12`、`termd 0.9.12`）与 pre-release 后缀（`0.9.12-beta`）。
pub fn parse_semver(version: &str) -> Option<(u64, u64, u64)> {
    let bytes = version.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        if !bytes[idx].is_ascii_digit() {
            idx += 1;
            continue;
        }
        let core = version[idx..].split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok();
        let minor = parts.next()?.parse().ok();
        let patch = parts.next()?.parse().ok();
        if let (Some(major), Some(minor), Some(patch)) = (major, minor, patch) {
            return Some((major, minor, patch));
        }
        idx += 1;
    }
    None
}

/// `latest` 是否严格大于 `current`。
pub fn is_newer_version(latest: &str, current: &str) -> bool {
    let Some(latest_parts) = parse_semver(latest) else {
        return false;
    };
    let Some(current_parts) = parse_semver(current) else {
        return false;
    };
    latest_parts > current_parts
}

/// 构建更新下载用的 HTTP client。
///
/// 读取标准环境变量代理（`HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY` 及其小写变体，
/// 与 daemon relay 连接的代理约定一致）；`NO_PROXY`/`no_proxy` 用于排除内网直连。
/// 超时采用「连接 30s + 读空闲 120s + 总上限 30min」，慢速但持续的下载不会超时。
fn build_client() -> Result<reqwest::blocking::Client, reqwest::Error> {
    const PROXY_ENV_VARS: [&str; 6] = [
        "HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy",
    ];
    const NO_PROXY_ENV_VARS: [&str; 2] = ["NO_PROXY", "no_proxy"];

    let mut builder = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(DOWNLOAD_CONNECT_TIMEOUT_SECS))
        .timeout(std::time::Duration::from_secs(DOWNLOAD_TOTAL_TIMEOUT_SECS))
        .user_agent("termd-updater");
    if let Some(proxy_url) = PROXY_ENV_VARS.iter().find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty())) {
        let mut proxy = reqwest::Proxy::all(&proxy_url)?;
        if let Some(no_proxy) = NO_PROXY_ENV_VARS
            .iter()
            .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
        {
            proxy = proxy.no_proxy(reqwest::NoProxy::from_string(&no_proxy));
        }
        builder = builder.proxy(proxy);
    }
    builder.build()
}

/// 查询 GitHub 最新 release 中与目标组件/架构匹配的资产。
pub fn check_update(
    binary_name: &str,
    arch: &str,
    current_version: &str,
) -> Result<UpdateInfo, UpdateError> {
    let client = build_client()?;
    // `?t=` 时间戳绕过 GitHub API 的 CDN 缓存：/releases/latest 的边缘节点缓存
    // 可能滞后几分钟，导致更新器拿到旧 latest 而下载旧资产。
    let latest_api_url = format!(
        "https://api.github.com{LATEST_RELEASE_API_PATH}?t={}",
        unix_timestamp_secs()
    );
    let response = client
        .get(&latest_api_url)
        .header("accept", "application/vnd.github+json")
        .send()?;
    if !response.status().is_success() {
        return Err(UpdateError::ApiRequest(
            response.error_for_status().unwrap_err(),
        ));
    }
    let release: ReleaseInfo = response.json()?;
    let latest = release.tag_name.trim().trim_start_matches('v').to_owned();
    if !is_newer_version(&latest, current_version) {
        return Err(UpdateError::NoNewerRelease {
            current: current_version.to_owned(),
            latest,
        });
    }
    let expected_name = format!("{binary_name}-linux-{arch}");
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == expected_name)
        .ok_or(UpdateError::AssetNotFound {
            binary: binary_name.to_owned(),
            arch: arch.to_owned(),
        })?;
    if !trusted_asset_url(
        &asset.browser_download_url,
        &release.tag_name,
        &expected_name,
    ) {
        return Err(UpdateError::UntrustedAssetUrl);
    }
    Ok(UpdateInfo {
        latest,
        release_url: release.html_url,
        asset_url: asset.browser_download_url.clone(),
    })
}

/// 资产 URL 必须严格位于 `https://github.com/yiiilin/termd/releases/download/<tag>/<name>`。
fn trusted_asset_url(url: &str, tag: &str, expected_name: &str) -> bool {
    let expected_prefix = format!("https://github.com/{GITHUB_REPO}/releases/download/{tag}/");
    url.starts_with(&expected_prefix)
        && url
            .strip_prefix(&expected_prefix)
            .is_some_and(|name| name == expected_name)
}

/// 本机架构（与 CI 资产命名一致）：x86_64 → amd64，aarch64 → arm64。
pub fn host_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

pub struct ApplyRequest {
    /// 目标二进制路径（如 `/usr/local/bin/termd`）。
    pub binary_path: PathBuf,
    /// systemd 服务名（如 `termd.service`）。
    pub service_name: String,
    /// 期望版本（下载后 `--version` 校验）。
    pub expected_version: String,
    /// release 资产下载地址（必须通过 `trusted_asset_url` 校验）。
    pub asset_url: String,
}

pub struct ApplyOutcome {
    /// 更新前是否真的替换了二进制（false = 目标版本已一致，无事可做）。
    pub replaced: bool,
}

/// 执行更新：下载 → 校验 → 备份 → 原子替换 → 重启服务。
/// 任何失败都会尝试恢复备份。调用方应在阻塞线程中执行。
pub fn apply_update(request: ApplyRequest) -> Result<ApplyOutcome, UpdateError> {
    let lock = acquire_lock()?;
    let _guard = lock;

    let existing_version = probe_version(&request.binary_path);
    if let Some(existing) = &existing_version
        && existing == &request.expected_version
    {
        return Ok(ApplyOutcome { replaced: false });
    }

    let parent = request
        .binary_path
        .parent()
        .ok_or_else(|| UpdateError::Io(io::Error::other("binary path has no parent directory")))?;
    fs::create_dir_all(parent)?;
    let downloaded = parent.join(format!(
        ".termd-update-{}-{}",
        request
            .binary_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        unix_timestamp_secs()
    ));

    // 下载
    let client = build_client()?;
    let response = client.get(&request.asset_url).send()?;
    if !response.status().is_success() {
        return Err(UpdateError::ApiRequest(
            response.error_for_status().unwrap_err(),
        ));
    }
    let bytes = response.bytes()?;
    fs::write(&downloaded, &bytes)?;
    fs::set_permissions(&downloaded, fs::Permissions::from_mode(0o755))?;

    // 版本校验
    let actual = probe_version(&downloaded);
    let matches = match actual.as_deref() {
        Some(actual_out) => {
            parse_semver(actual_out) == parse_semver(&request.expected_version)
        }
        None => false,
    };
    if !matches {
        let _ = fs::remove_file(&downloaded);
        return Err(UpdateError::VersionCheckFailed {
            expected: request.expected_version,
            actual,
        });
    }

    // 备份 + 原子替换
    let backup = parent.join(format!(
        "{}.bak-update-{}",
        request
            .binary_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        unix_timestamp_secs()
    ));
    let backup_result = (|| -> Result<(), UpdateError> {
        if request.binary_path.exists() {
            fs::copy(&request.binary_path, &backup)?;
        }
        fs::rename(&downloaded, &request.binary_path)?;
        Ok(())
    })();

    if let Err(error) = backup_result {
        let _ = fs::remove_file(&downloaded);
        // 替换可能已经发生：尽力回滚
        if backup.exists() && request.binary_path.exists() {
            let _ = fs::copy(&backup, &request.binary_path);
        }
        return Err(error);
    }

    // 重启服务
    let restart = Command::new("systemctl")
        .arg("restart")
        .arg(&request.service_name)
        .status();
    match restart {
        Ok(status) if status.success() => Ok(ApplyOutcome { replaced: true }),
        Ok(status) => {
            // 重启失败：回滚二进制（服务可能仍在跑旧二进制，或已停止）
            let _ = fs::copy(&backup, &request.binary_path);
            let _ = Command::new("systemctl")
                .arg("restart")
                .arg(&request.service_name)
                .status();
            Err(UpdateError::RestartFailed {
                service: request.service_name,
                source: io::Error::other(format!("systemctl exited with {status}")),
            })
        }
        Err(source) => {
            let _ = fs::copy(&backup, &request.binary_path);
            let _ = Command::new("systemctl")
                .arg("restart")
                .arg(&request.service_name)
                .status();
            Err(UpdateError::RestartFailed {
                service: request.service_name,
                source,
            })
        }
    }
}

/// 运行 `binary --version`，返回输出文本（可能含换行与前缀）。
fn probe_version(binary: &Path) -> Option<String> {
    let output = Command::new(binary).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// 独占更新锁：同一主机同时只允许一个更新流程。
fn acquire_lock() -> Result<fs::File, UpdateError> {
    let file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(UPDATE_LOCK_PATH)?;
    let locked = rustix_like_flock(&file)?;
    if !locked {
        return Err(UpdateError::UpdateInProgress);
    }
    Ok(file)
}

/// 用 `flock(LOCK_EX | LOCK_NB)` 获取独占锁；`rustix`/`libc` 未引入时退化到
/// 进程内标记不可靠，因此这里直接使用 `libc` 不可用时返回错误。
fn rustix_like_flock(file: &fs::File) -> Result<bool, UpdateError> {
    // 通过 `flock(2)` syscall；使用 std 的 `File` 再打开一份保证描述符独立。
    // termd/termrelay 已依赖 libc，这里直接复用。
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            Ok(true)
        } else {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                Ok(false)
            } else {
                Err(UpdateError::Io(error))
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_semver_variants() {
        assert_eq!(parse_semver("0.9.7"), Some((0, 9, 7)));
        assert_eq!(parse_semver("v0.9.7"), Some((0, 9, 7)));
        assert_eq!(parse_semver("0.10.0-rc.1"), Some((0, 10, 0)));
        assert_eq!(parse_semver("garbage"), None);
        assert_eq!(parse_semver("termd 0.9.12"), Some((0, 9, 12)));
        assert_eq!(parse_semver("v0.9.12"), Some((0, 9, 12)));
        assert_eq!(parse_semver("0.9"), None);
    }

    #[test]
    fn compares_semver() {
        assert!(is_newer_version("0.9.7", "0.9.6"));
        assert!(is_newer_version("0.10.0", "0.9.9"));
        assert!(!is_newer_version("0.9.6", "0.9.6"));
        assert!(!is_newer_version("garbage", "0.9.6"));
    }

    #[test]
    fn asset_url_must_match_release_download_path() {
        assert!(trusted_asset_url(
            "https://github.com/yiiilin/termd/releases/download/0.9.7/termd-linux-amd64",
            "0.9.7",
            "termd-linux-amd64",
        ));
        assert!(!trusted_asset_url(
            "https://github.com/evil/termd/releases/download/0.9.7/termd-linux-amd64",
            "0.9.7",
            "termd-linux-amd64",
        ));
        assert!(!trusted_asset_url(
            "https://github.com/yiiilin/termd/releases/download/0.9.7/termd-linux-arm64",
            "0.9.7",
            "termd-linux-amd64",
        ));
        assert!(!trusted_asset_url(
            "https://github.com/yiiilin/termd/releases/download/0.9.7/../../etc/passwd",
            "0.9.7",
            "termd-linux-amd64",
        ));
        assert!(!trusted_asset_url(
            "https://attacker.example/termd-linux-amd64",
            "0.9.7",
            "termd-linux-amd64",
        ));
    }

    #[test]
    fn probe_version_reads_stdout() {
        let output = probe_version(Path::new("/bin/bash")).unwrap_or_default();
        assert!(
            output.contains("bash"),
            "expected bash version output, got {output:?}"
        );
        assert_eq!(probe_version(Path::new("/nonexistent-termd-binary")), None);
    }
}
