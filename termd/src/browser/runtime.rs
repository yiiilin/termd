use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::Builder;

use super::BrowserError;

const RUNTIME_MANIFEST_SCHEMA: u32 = 1;
const RUNTIME_GLIBC_MIN_VERSION: &str = "2.31";
const RUNTIME_ARCHIVE_MAX_BYTES: u64 = 128 * 1024 * 1024;
const RUNTIME_EXTRACTED_MAX_BYTES: u64 = 256 * 1024 * 1024;
const RUNTIME_ARCHIVE_MAX_ENTRIES: usize = 20_000;
const RUNTIME_MANIFEST_MAX_BYTES: u64 = 64 * 1024;
const RUNTIME_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub(crate) struct BrowserRuntimePaths {
    pub(crate) root: Option<PathBuf>,
    pub(crate) xvnc: PathBuf,
    pub(crate) openbox: PathBuf,
}

impl BrowserRuntimePaths {
    pub(crate) fn library_path(&self) -> Option<PathBuf> {
        self.root
            .as_ref()
            .map(|root| root.join("lib"))
            .filter(|path| path.is_dir())
    }

    pub(crate) fn xkb_root(&self) -> Option<PathBuf> {
        self.root
            .as_ref()
            .map(|root| root.join("share/X11/xkb"))
            .filter(|path| path.is_dir())
    }

    pub(crate) fn data_root(&self) -> Option<PathBuf> {
        self.root
            .as_ref()
            .map(|root| root.join("share"))
            .filter(|path| path.is_dir())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BrowserRuntimeManager {
    install_root: PathBuf,
    release_base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserRuntimeManifest {
    schema_version: u32,
    termd_version: String,
    runtime_version: String,
    os: String,
    arch: String,
    glibc_min_version: String,
    archive_file: String,
    archive_size_bytes: u64,
    archive_sha256: String,
}

impl BrowserRuntimeManager {
    pub(crate) fn new(browser_root: &Path) -> Self {
        let version = env!("CARGO_PKG_VERSION");
        let release_base_url = env::var("TERMD_BROWSER_RUNTIME_BASE_URL").unwrap_or_else(|_| {
            format!("https://github.com/yiiilin/termd/releases/download/{version}")
        });
        Self {
            install_root: browser_root.join("runtimes"),
            release_base_url: release_base_url.trim_end_matches('/').to_owned(),
        }
    }

    pub(crate) async fn ensure(&self) -> Result<BrowserRuntimePaths, BrowserError> {
        let arch = runtime_arch()?;
        let managed = self.install_root.join(env!("CARGO_PKG_VERSION")).join(arch);
        if let Some(paths) = runtime_paths_if_valid(&managed) {
            return Ok(paths);
        }
        if let Some(paths) = configured_runtime_paths()? {
            return Ok(paths);
        }

        let manager = self.clone();
        let arch = arch.to_owned();
        tokio::task::spawn_blocking(move || manager.download_and_install(&arch, &managed))
            .await
            .map_err(|_| BrowserError::RuntimeInstallFailed)?
    }

    fn download_and_install(
        &self,
        arch: &str,
        target: &Path,
    ) -> Result<BrowserRuntimePaths, BrowserError> {
        let archive_file = format!("termd-browser-runtime-linux-{arch}.tar.gz");
        let manifest_file = format!("termd-browser-runtime-linux-{arch}.json");
        let manifest_url = format!("{}/{manifest_file}", self.release_base_url);
        let archive_url = format!("{}/{archive_file}", self.release_base_url);
        let client = Client::builder()
            .timeout(RUNTIME_DOWNLOAD_TIMEOUT)
            .user_agent(concat!("termd/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| BrowserError::RuntimeDownloadFailed)?;

        let manifest_bytes = download_bounded(&client, &manifest_url, RUNTIME_MANIFEST_MAX_BYTES)?;
        let manifest: BrowserRuntimeManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|_| BrowserError::RuntimeManifestInvalid)?;
        validate_manifest(&manifest, arch, &archive_file)?;

        let parent = target.parent().ok_or(BrowserError::RuntimeInstallFailed)?;
        create_private_dir_all(parent)?;
        let staging = Builder::new()
            .prefix(".browser-runtime-")
            .tempdir_in(parent)
            .map_err(|_| BrowserError::RuntimeInstallFailed)?;
        let archive_path = staging.path().join("runtime.tar.gz");
        download_archive(&client, &archive_url, &archive_path, &manifest)?;
        let unpacked = staging.path().join("unpacked");
        fs::create_dir(&unpacked).map_err(|_| BrowserError::RuntimeInstallFailed)?;
        extract_runtime_archive(&archive_path, &unpacked)?;

        let paths = runtime_paths_if_valid(&unpacked).ok_or(BrowserError::RuntimeArchiveInvalid)?;
        let manifest_path = unpacked.join("runtime-manifest.json");
        let manifest_json = serde_json::to_vec_pretty(&manifest)
            .map_err(|_| BrowserError::RuntimeManifestInvalid)?;
        write_private_file(&manifest_path, &manifest_json)?;

        if target.exists() {
            fs::remove_dir_all(target).map_err(|_| BrowserError::RuntimeInstallFailed)?;
        }
        fs::rename(&unpacked, target).map_err(|_| BrowserError::RuntimeInstallFailed)?;
        let _ = paths;
        runtime_paths_if_valid(target).ok_or(BrowserError::RuntimeInstallFailed)
    }
}

pub(crate) fn resolve_chromium() -> Result<PathBuf, BrowserError> {
    if let Some(path) = env::var_os("TERMD_BROWSER_CHROMIUM") {
        let path = PathBuf::from(path);
        return executable_file(&path)
            .then_some(path)
            .ok_or(BrowserError::ChromiumUnavailable);
    }
    [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
    ]
    .into_iter()
    .find_map(find_in_path)
    .ok_or(BrowserError::ChromiumUnavailable)
}

fn configured_runtime_paths() -> Result<Option<BrowserRuntimePaths>, BrowserError> {
    configured_runtime_paths_from(
        env::var_os("TERMD_BROWSER_RUNTIME_DIR").map(PathBuf::from),
        env::var_os("TERMD_BROWSER_XVNC").map(PathBuf::from),
        env::var_os("TERMD_BROWSER_OPENBOX").map(PathBuf::from),
    )
}

fn configured_runtime_paths_from(
    root: Option<PathBuf>,
    xvnc: Option<PathBuf>,
    openbox: Option<PathBuf>,
) -> Result<Option<BrowserRuntimePaths>, BrowserError> {
    if let Some(root) = root {
        return runtime_paths_if_valid(&root)
            .map(Some)
            .ok_or(BrowserError::RuntimeUnavailable);
    }
    match (xvnc, openbox) {
        (None, None) => Ok(None),
        (Some(xvnc), Some(openbox)) if executable_file(&xvnc) && executable_file(&openbox) => {
            Ok(Some(BrowserRuntimePaths {
                root: None,
                xvnc,
                openbox,
            }))
        }
        _ => Err(BrowserError::RuntimeUnavailable),
    }
}

pub(super) fn runtime_paths_if_valid(root: &Path) -> Option<BrowserRuntimePaths> {
    let xvnc = root.join("bin/Xtigervnc");
    let openbox = root.join("bin/openbox");
    let xkbcomp = root.join("bin/xkbcomp");
    let openbox_theme = root.join("share/themes/Clearlooks/openbox-3/themerc");
    (executable_file(&xvnc)
        && executable_file(&openbox)
        && executable_file(&xkbcomp)
        && openbox_theme.is_file())
    .then(|| BrowserRuntimePaths {
        root: Some(root.to_path_buf()),
        xvnc,
        openbox,
    })
}

fn executable_file(path: &Path) -> bool {
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 {
        return executable_file(path).then(|| path.to_path_buf());
    }
    env::split_paths(&env::var_os("PATH")?)
        .filter(|part| !part.as_os_str().is_empty())
        .map(|part| part.join(name))
        .find(|candidate| executable_file(candidate))
}

fn runtime_arch() -> Result<&'static str, BrowserError> {
    match env::consts::ARCH {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        _ => Err(BrowserError::UnsupportedArchitecture),
    }
}

fn validate_manifest(
    manifest: &BrowserRuntimeManifest,
    arch: &str,
    archive_file: &str,
) -> Result<(), BrowserError> {
    let valid_hash = manifest.archive_sha256.len() == 64
        && manifest
            .archive_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
    if manifest.schema_version != RUNTIME_MANIFEST_SCHEMA
        || manifest.termd_version != env!("CARGO_PKG_VERSION")
        || manifest.runtime_version.trim().is_empty()
        || manifest.os != "linux"
        || manifest.arch != arch
        || manifest.glibc_min_version != RUNTIME_GLIBC_MIN_VERSION
        || manifest.archive_file != archive_file
        || manifest.archive_size_bytes == 0
        || manifest.archive_size_bytes > RUNTIME_ARCHIVE_MAX_BYTES
        || !valid_hash
    {
        return Err(BrowserError::RuntimeManifestInvalid);
    }
    Ok(())
}

fn download_bounded(client: &Client, url: &str, max_bytes: u64) -> Result<Vec<u8>, BrowserError> {
    let response = client
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|_| BrowserError::RuntimeDownloadFailed)?;
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(BrowserError::RuntimeDownloadFailed);
    }
    let mut bytes = Vec::new();
    response
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| BrowserError::RuntimeDownloadFailed)?;
    if bytes.len() as u64 > max_bytes {
        return Err(BrowserError::RuntimeDownloadFailed);
    }
    Ok(bytes)
}

fn download_archive(
    client: &Client,
    url: &str,
    path: &Path,
    manifest: &BrowserRuntimeManifest,
) -> Result<(), BrowserError> {
    let mut response = client
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|_| BrowserError::RuntimeDownloadFailed)?;
    if response
        .content_length()
        .is_some_and(|length| length != manifest.archive_size_bytes)
    {
        return Err(BrowserError::RuntimeArchiveInvalid);
    }
    let mut file = File::create(path).map_err(|_| BrowserError::RuntimeInstallFailed)?;
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|_| BrowserError::RuntimeDownloadFailed)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or(BrowserError::RuntimeArchiveInvalid)?;
        if total > RUNTIME_ARCHIVE_MAX_BYTES || total > manifest.archive_size_bytes {
            return Err(BrowserError::RuntimeArchiveInvalid);
        }
        file.write_all(&buffer[..read])
            .map_err(|_| BrowserError::RuntimeInstallFailed)?;
        hasher.update(&buffer[..read]);
    }
    if total != manifest.archive_size_bytes
        || format!("{:x}", hasher.finalize()) != manifest.archive_sha256
    {
        return Err(BrowserError::RuntimeArchiveInvalid);
    }
    file.sync_all()
        .map_err(|_| BrowserError::RuntimeInstallFailed)
}

fn extract_runtime_archive(archive_path: &Path, destination: &Path) -> Result<(), BrowserError> {
    let file = File::open(archive_path).map_err(|_| BrowserError::RuntimeArchiveInvalid)?;
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let entries = archive
        .entries()
        .map_err(|_| BrowserError::RuntimeArchiveInvalid)?;
    let mut extracted_bytes = 0_u64;
    for (index, entry) in entries.enumerate() {
        if index >= RUNTIME_ARCHIVE_MAX_ENTRIES {
            return Err(BrowserError::RuntimeArchiveInvalid);
        }
        let mut entry = entry.map_err(|_| BrowserError::RuntimeArchiveInvalid)?;
        let path = entry
            .path()
            .map_err(|_| BrowserError::RuntimeArchiveInvalid)?
            .into_owned();
        validate_archive_path(&path)?;
        let kind = entry.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            return Err(BrowserError::RuntimeArchiveInvalid);
        }
        if kind.is_file() {
            extracted_bytes = extracted_bytes
                .checked_add(entry.size())
                .ok_or(BrowserError::RuntimeArchiveInvalid)?;
            if extracted_bytes > RUNTIME_EXTRACTED_MAX_BYTES {
                return Err(BrowserError::RuntimeArchiveInvalid);
            }
        }
        entry
            .unpack_in(destination)
            .map_err(|_| BrowserError::RuntimeArchiveInvalid)?;
        let unpacked = destination.join(&path);
        let mode = if kind.is_dir() || path.starts_with("bin") {
            0o755
        } else {
            0o644
        };
        fs::set_permissions(unpacked, fs::Permissions::from_mode(mode))
            .map_err(|_| BrowserError::RuntimeArchiveInvalid)?;
    }
    Ok(())
}

fn validate_archive_path(path: &Path) -> Result<(), BrowserError> {
    let mut components = path.components();
    let first = components
        .next()
        .ok_or(BrowserError::RuntimeArchiveInvalid)?;
    let Component::Normal(first) = first else {
        return Err(BrowserError::RuntimeArchiveInvalid);
    };
    if !matches!(first.to_str(), Some("bin" | "lib" | "share"))
        || components.any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(BrowserError::RuntimeArchiveInvalid);
    }
    Ok(())
}

fn create_private_dir_all(path: &Path) -> Result<(), BrowserError> {
    fs::create_dir_all(path).map_err(|_| BrowserError::RuntimeInstallFailed)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| BrowserError::RuntimeInstallFailed)
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), BrowserError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options
        .open(path)
        .map_err(|_| BrowserError::RuntimeInstallFailed)?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| BrowserError::RuntimeInstallFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_manifest() -> BrowserRuntimeManifest {
        BrowserRuntimeManifest {
            schema_version: 1,
            termd_version: env!("CARGO_PKG_VERSION").to_owned(),
            runtime_version: "tigervnc-1.16.2-openbox-3.6.1".to_owned(),
            os: "linux".to_owned(),
            arch: "amd64".to_owned(),
            glibc_min_version: RUNTIME_GLIBC_MIN_VERSION.to_owned(),
            archive_file: "termd-browser-runtime-linux-amd64.tar.gz".to_owned(),
            archive_size_bytes: 42,
            archive_sha256: "a".repeat(64),
        }
    }

    #[test]
    fn manifest_is_bound_to_version_arch_and_asset_name() {
        assert!(
            validate_manifest(
                &valid_manifest(),
                "amd64",
                "termd-browser-runtime-linux-amd64.tar.gz"
            )
            .is_ok()
        );

        let mut wrong = valid_manifest();
        wrong.arch = "arm64".to_owned();
        assert!(matches!(
            validate_manifest(&wrong, "amd64", "termd-browser-runtime-linux-amd64.tar.gz"),
            Err(BrowserError::RuntimeManifestInvalid)
        ));
    }

    #[test]
    fn archive_paths_reject_traversal_and_links_outside_layout() {
        for invalid in [
            "../bin/Xtigervnc",
            "/bin/Xtigervnc",
            "etc/passwd",
            "bin/../escape",
        ] {
            assert!(matches!(
                validate_archive_path(Path::new(invalid)),
                Err(BrowserError::RuntimeArchiveInvalid)
            ));
        }
        assert!(validate_archive_path(Path::new("bin/Xtigervnc")).is_ok());
        assert!(validate_archive_path(Path::new("share/X11/xkb/rules/base")).is_ok());
    }

    #[test]
    fn runtime_layout_requires_both_executables() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("bin")).unwrap();
        fs::write(root.path().join("bin/Xtigervnc"), b"x").unwrap();
        fs::set_permissions(
            root.path().join("bin/Xtigervnc"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(runtime_paths_if_valid(root.path()).is_none());

        fs::write(root.path().join("bin/openbox"), b"x").unwrap();
        fs::set_permissions(
            root.path().join("bin/openbox"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(runtime_paths_if_valid(root.path()).is_none());

        fs::write(root.path().join("bin/xkbcomp"), b"x").unwrap();
        fs::set_permissions(
            root.path().join("bin/xkbcomp"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(runtime_paths_if_valid(root.path()).is_none());

        let theme = root
            .path()
            .join("share/themes/Clearlooks/openbox-3/themerc");
        fs::create_dir_all(theme.parent().unwrap()).unwrap();
        fs::write(theme, b"window.active.title.bg: flat solid").unwrap();
        assert!(runtime_paths_if_valid(root.path()).is_some());
    }

    #[test]
    fn runtime_selection_requires_a_managed_layout_or_explicit_pair() {
        assert!(matches!(
            configured_runtime_paths_from(None, None, None),
            Ok(None)
        ));
        assert!(matches!(
            configured_runtime_paths_from(None, Some(PathBuf::from("/usr/bin/Xvnc")), None),
            Err(BrowserError::RuntimeUnavailable)
        ));
    }
}
