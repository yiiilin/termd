mod automation;
mod download;
mod runtime;
pub mod supervisor;

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep};
use url::Url;

use crate::file_offer::{FileOfferError, InspectedFileOffer};

pub use termd_proto::BrowserSessionId;

pub use automation::{BrowserDownload, BrowserPage, BrowserSnapshot, BrowserSnapshotElement};

pub(crate) use download::BrowserDownloadCandidate;
use download::{BrowserDownloadTracker, cleanup_incomplete_downloads, scan_downloads};
use runtime::{BrowserRuntimeManager, BrowserRuntimePaths, resolve_chromium};
use supervisor::BrowserSupervisorArgs;

use self::automation::{AUTOMATION_SOCKET_FILE_NAME, AutomationRequest, AutomationResult};

const BROWSER_STORE_SCHEMA: u32 = 1;
const BROWSER_STORE_MAX_BYTES: u64 = 1024 * 1024;
const BROWSER_SESSION_LIMIT: usize = 4;
const BROWSER_URL_MAX_BYTES: usize = 2048;
const BROWSER_SELECTOR_MAX_BYTES: usize = 4096;
const BROWSER_FILL_VALUE_MAX_BYTES: usize = 64 * 1024;
const BROWSER_DOWNLOAD_WAIT_MIN_MS: u64 = 100;
pub const BROWSER_DOWNLOAD_WAIT_MAX_MS: u64 = 120_000;
const BROWSER_START_TIMEOUT: Duration = Duration::from_secs(60);
const BROWSER_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const UNIX_SOCKET_PATH_MAX_BYTES: usize = 107;
const BROWSER_SESSION_ENV: &str = "TERMD_BROWSER_SESSION_ID";
const UUID_PATH_PLACEHOLDER: &str = "00000000-0000-0000-0000-000000000000";
const MIN_BROWSER_WIDTH: u16 = 640;
const MAX_BROWSER_WIDTH: u16 = 3840;
const MIN_BROWSER_HEIGHT: u16 = 480;
const MAX_BROWSER_HEIGHT: u16 = 2160;
#[cfg(not(test))]
const BROWSER_PROFILE_ROOT: &str = "/var/tmp/termd-browser-profiles";
#[cfg(not(test))]
const BROWSER_DOWNLOAD_ROOT: &str = "/var/tmp/termd-browser-downloads";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserSessionState {
    Created,
    Running,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSession {
    pub browser_id: BrowserSessionId,
    pub state: BrowserSessionState,
    pub display_url: String,
    pub width: u16,
    pub height: u16,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserCreateRequest {
    pub url: String,
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum BrowserError {
    #[error("browser URL is invalid")]
    InvalidUrl,
    #[error("browser viewport is invalid")]
    InvalidViewport,
    #[error("browser session capacity is exhausted")]
    CapacityExceeded,
    #[error("browser session was not found")]
    SessionNotFound,
    #[error("browser session is not running")]
    SessionNotRunning,
    #[error("browser workspace storage is unavailable")]
    StorageUnavailable,
    #[error("browser workspace state is invalid")]
    StateInvalid,
    #[error("browser workspace persistence failed")]
    StateWriteFailed,
    #[error("browser runtime is unavailable")]
    RuntimeUnavailable,
    #[error("browser runtime architecture is unsupported")]
    UnsupportedArchitecture,
    #[error("browser runtime download failed")]
    RuntimeDownloadFailed,
    #[error("browser runtime manifest is invalid")]
    RuntimeManifestInvalid,
    #[error("browser runtime archive is invalid")]
    RuntimeArchiveInvalid,
    #[error("browser runtime installation failed")]
    RuntimeInstallFailed,
    #[error("Chromium is unavailable")]
    ChromiumUnavailable,
    #[error("browser supervisor arguments are invalid")]
    SupervisorArgumentsInvalid,
    #[error("browser supervisor failed to start")]
    SupervisorStartFailed,
    #[error("browser supervisor startup timed out")]
    SupervisorStartTimeout,
    #[error("browser supervisor failed to stop")]
    SupervisorStopFailed,
    #[error("browser RFB socket is unavailable")]
    RfbUnavailable,
    #[error("browser automation request is invalid")]
    AutomationRequestInvalid,
    #[error("browser automation target was not found")]
    AutomationTargetNotFound,
    #[error("browser automation timed out")]
    AutomationTimeout,
    #[error("browser automation is busy")]
    AutomationBusy,
    #[error("browser automation is unavailable")]
    AutomationUnavailable,
    #[error("browser automation failed")]
    AutomationFailed,
}

impl BrowserError {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidUrl => "browser_url_invalid",
            Self::InvalidViewport => "browser_viewport_invalid",
            Self::CapacityExceeded => "browser_capacity_exceeded",
            Self::SessionNotFound => "browser_not_found",
            Self::SessionNotRunning => "browser_not_running",
            Self::StorageUnavailable | Self::StateInvalid | Self::StateWriteFailed => {
                "browser_storage_failed"
            }
            Self::RuntimeUnavailable | Self::UnsupportedArchitecture => {
                "browser_runtime_unavailable"
            }
            Self::RuntimeDownloadFailed
            | Self::RuntimeManifestInvalid
            | Self::RuntimeArchiveInvalid
            | Self::RuntimeInstallFailed => "browser_runtime_install_failed",
            Self::ChromiumUnavailable => "browser_chromium_unavailable",
            Self::SupervisorArgumentsInvalid
            | Self::SupervisorStartFailed
            | Self::SupervisorStartTimeout => "browser_start_failed",
            Self::SupervisorStopFailed => "browser_stop_failed",
            Self::RfbUnavailable => "browser_rfb_unavailable",
            Self::AutomationRequestInvalid => "browser_automation_invalid",
            Self::AutomationTargetNotFound => "browser_automation_target_not_found",
            Self::AutomationTimeout => "browser_automation_timeout",
            Self::AutomationBusy => "browser_automation_busy",
            Self::AutomationUnavailable => "browser_automation_unavailable",
            Self::AutomationFailed => "browser_automation_failed",
        }
    }

    pub fn safe_message(self) -> &'static str {
        match self {
            Self::InvalidUrl => "URL must be an absolute HTTP or HTTPS address",
            Self::InvalidViewport => "browser viewport is outside the supported range",
            Self::CapacityExceeded => "too many browser sessions are already running",
            Self::SessionNotFound => "browser session was not found",
            Self::SessionNotRunning => "browser session is not running",
            Self::ChromiumUnavailable => "Chromium is not installed on the daemon host",
            Self::RuntimeUnavailable | Self::UnsupportedArchitecture => {
                "browser runtime is unavailable for this host"
            }
            Self::RuntimeDownloadFailed
            | Self::RuntimeManifestInvalid
            | Self::RuntimeArchiveInvalid
            | Self::RuntimeInstallFailed => "browser runtime could not be installed",
            Self::SupervisorArgumentsInvalid
            | Self::SupervisorStartFailed
            | Self::SupervisorStartTimeout => "browser session could not be started",
            Self::SupervisorStopFailed => "browser session could not be stopped",
            Self::RfbUnavailable => "browser display is not ready",
            Self::AutomationRequestInvalid => "browser automation request is invalid",
            Self::AutomationTargetNotFound => "browser selector or page target was not found",
            Self::AutomationTimeout => "browser automation timed out",
            Self::AutomationBusy => "another browser automation action is still running",
            Self::AutomationUnavailable => "browser automation is unavailable for this session",
            Self::AutomationFailed => "browser automation could not complete the action",
            Self::StorageUnavailable | Self::StateInvalid | Self::StateWriteFailed => {
                "browser workspace storage is unavailable"
            }
        }
    }

    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::RuntimeDownloadFailed
                | Self::RuntimeInstallFailed
                | Self::SupervisorStartFailed
                | Self::SupervisorStartTimeout
                | Self::SupervisorStopFailed
                | Self::RfbUnavailable
                | Self::AutomationTimeout
                | Self::AutomationBusy
                | Self::AutomationUnavailable
                | Self::AutomationFailed
        )
    }
}

#[derive(Clone)]
pub struct BrowserWorkspace {
    inner: Arc<BrowserWorkspaceInner>,
}

struct BrowserWorkspaceInner {
    paths: BrowserPaths,
    runtime: BrowserRuntimeManager,
    launcher: Arc<dyn BrowserLauncher>,
    operation: Mutex<()>,
    state: Mutex<Result<BrowserWorkspaceState, BrowserError>>,
    download_tracker: Mutex<BrowserDownloadTracker>,
    #[cfg(test)]
    fixed_runtime: Option<BrowserRuntimePaths>,
    #[cfg(test)]
    fixed_chromium: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct BrowserPaths {
    root: PathBuf,
    records: PathBuf,
    run: PathBuf,
    short_run: PathBuf,
    profiles: PathBuf,
    downloads: PathBuf,
    configs: PathBuf,
    x11_sockets: PathBuf,
    x11_locks: PathBuf,
}

#[derive(Debug, Clone, Default)]
struct BrowserWorkspaceState {
    sessions: BTreeMap<BrowserSessionId, BrowserSessionRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserSessionRecord {
    session: BrowserSession,
    launch_url: String,
    supervisor_pid: u32,
    display: u16,
    rfb_socket: PathBuf,
    ready_file: PathBuf,
    profile_dir: PathBuf,
    openbox_config: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserStoreFile {
    schema_version: u32,
    sessions: Vec<BrowserSessionRecord>,
}

struct BrowserLaunchHandle {
    pid: u32,
    child: Option<Child>,
}

trait BrowserLauncher: Send + Sync {
    fn launch(
        &self,
        id: BrowserSessionId,
        args: &BrowserSupervisorArgs,
    ) -> Result<BrowserLaunchHandle, BrowserError>;
    fn is_alive(&self, pid: u32, id: BrowserSessionId) -> bool;
    fn terminate(&self, pid: u32, id: BrowserSessionId, signal: i32) -> bool;

    fn group_is_alive(&self, pid: u32, id: BrowserSessionId) -> bool {
        self.is_alive(pid, id)
    }

    fn terminate_group(&self, pid: u32, id: BrowserSessionId, signal: i32) -> bool {
        self.terminate(pid, id, signal)
    }
}

struct ProcessBrowserLauncher {
    binary: PathBuf,
}

impl BrowserWorkspace {
    pub fn for_state_path(state_path: impl AsRef<Path>) -> Self {
        let paths = BrowserPaths::for_state_path(state_path.as_ref());
        let initial = prepare_browser_paths(&paths).and_then(|_| load_browser_state(&paths));
        if let Err(error) = initial {
            tracing::warn!(code = error.code(), "browser workspace is unavailable");
        }
        let startup_downloads = initial
            .as_ref()
            .ok()
            .and_then(|state| {
                let active_sessions = state.sessions.keys().map(|id| id.0).collect::<HashSet<_>>();
                match scan_downloads(&paths.downloads, &active_sessions) {
                    Ok(downloads) => Some(downloads),
                    Err(error) => {
                        tracing::warn!(%error, "browser download startup snapshot failed");
                        None
                    }
                }
            })
            .unwrap_or_default();
        let launcher: Arc<dyn BrowserLauncher> = match std::env::current_exe() {
            Ok(binary) => Arc::new(ProcessBrowserLauncher { binary }),
            Err(_) => Arc::new(UnavailableBrowserLauncher),
        };
        Self {
            inner: Arc::new(BrowserWorkspaceInner {
                runtime: BrowserRuntimeManager::new(&paths.root),
                paths,
                launcher,
                operation: Mutex::new(()),
                state: Mutex::new(initial),
                download_tracker: Mutex::new(BrowserDownloadTracker::at_startup(startup_downloads)),
                #[cfg(test)]
                fixed_runtime: None,
                #[cfg(test)]
                fixed_chromium: None,
            }),
        }
    }

    pub async fn list(&self) -> Result<Vec<BrowserSession>, BrowserError> {
        let _operation = self.inner.operation.lock().await;
        let mut guard = self.inner.state.lock().await;
        let state = state_mut(&mut guard)?;
        self.reconcile(state).await?;
        Ok(state
            .sessions
            .values()
            .map(|record| record.session.clone())
            .collect())
    }

    pub async fn create(
        &self,
        request: BrowserCreateRequest,
    ) -> Result<BrowserSession, BrowserError> {
        let launch_url = validate_browser_url(&request.url)?;
        validate_viewport(request.width, request.height)?;
        let _operation = self.inner.operation.lock().await;

        let runtime = self.runtime_paths().await?;
        let chromium = self.chromium_path()?;
        let mut guard = self.inner.state.lock().await;
        let state = state_mut(&mut guard)?;
        self.reconcile(state).await?;
        if state.sessions.len() >= BROWSER_SESSION_LIMIT {
            return Err(BrowserError::CapacityExceeded);
        }

        let browser_id = BrowserSessionId::new();
        let display = allocate_display(&self.inner.paths, state)?;
        let run_dir = self.inner.paths.session_run_dir(browser_id)?;
        let profile_dir = self.inner.paths.profiles.join(browser_id.to_string());
        let download_dir = self.inner.paths.downloads.join(browser_id.to_string());
        let openbox_config = self.inner.paths.configs.join(format!("{browser_id}.xml"));
        create_private_dir(&run_dir)?;
        create_private_dir(&profile_dir)?;
        create_private_dir(&download_dir)?;
        write_openbox_config(&openbox_config)?;
        let rfb_socket = run_dir.join("rfb.sock");
        let automation_socket = run_dir.join(AUTOMATION_SOCKET_FILE_NAME);
        let ready_file = run_dir.join("ready");
        let start_file = run_dir.join("start");
        let runtime_library_path = runtime.library_path();
        let xkb_root = runtime.xkb_root();
        let runtime_data_root = runtime.data_root();
        let runtime_bin_root = runtime
            .root
            .as_ref()
            .map(|root| root.join("bin"))
            .filter(|path| path.is_dir());
        let args = BrowserSupervisorArgs {
            session_id: browser_id.0,
            display,
            width: request.width,
            height: request.height,
            url: launch_url.clone(),
            xvnc: runtime.xvnc,
            openbox: runtime.openbox,
            chromium,
            rfb_socket: rfb_socket.clone(),
            automation_socket: automation_socket.clone(),
            ready_file: ready_file.clone(),
            start_file: start_file.clone(),
            profile_dir: profile_dir.clone(),
            download_dir,
            openbox_config: openbox_config.clone(),
            runtime_library_path,
            xkb_root,
            runtime_data_root,
            runtime_bin_root,
        };
        let session = BrowserSession {
            browser_id,
            state: BrowserSessionState::Created,
            display_url: display_url(&launch_url),
            width: request.width,
            height: request.height,
            created_at_ms: unix_timestamp_millis(),
        };
        let record = BrowserSessionRecord {
            session: session.clone(),
            launch_url,
            supervisor_pid: 0,
            display,
            rfb_socket,
            ready_file,
            profile_dir,
            openbox_config,
        };
        state.sessions.insert(browser_id, record);
        if let Err(error) = persist_browser_state(&self.inner.paths, state) {
            cleanup_session_paths(&self.inner.paths, browser_id, display)?;
            state.sessions.remove(&browser_id);
            return Err(error);
        }

        let mut handle = match self.inner.launcher.launch(browser_id, &args) {
            Ok(handle) => handle,
            Err(error) => {
                rollback_browser_launch(&self.inner.paths, state, browser_id)?;
                return Err(error);
            }
        };
        state
            .sessions
            .get_mut(&browser_id)
            .ok_or(BrowserError::StateInvalid)?
            .supervisor_pid = handle.pid;
        if let Err(error) = persist_browser_state(&self.inner.paths, state) {
            stop_owned_browser_launch(self.inner.launcher.as_ref(), browser_id, &mut handle)
                .await?;
            rollback_browser_launch(&self.inner.paths, state, browser_id)?;
            return Err(error);
        }
        if let Err(error) = write_browser_start_file(&start_file, browser_id) {
            stop_owned_browser_launch(self.inner.launcher.as_ref(), browser_id, &mut handle)
                .await?;
            rollback_browser_launch(&self.inner.paths, state, browser_id)?;
            return Err(error);
        }

        let started = wait_for_browser_ready(
            self.inner.launcher.as_ref(),
            browser_id,
            &mut handle,
            &state.sessions[&browser_id],
            &automation_socket,
        )
        .await;
        if let Err(error) = started {
            stop_owned_browser_launch(self.inner.launcher.as_ref(), browser_id, &mut handle)
                .await?;
            rollback_browser_launch(&self.inner.paths, state, browser_id)?;
            return Err(error);
        }
        let session = {
            let record = state
                .sessions
                .get_mut(&browser_id)
                .ok_or(BrowserError::StateInvalid)?;
            record.session.state = BrowserSessionState::Running;
            record.session.clone()
        };
        if let Err(error) = persist_browser_state(&self.inner.paths, state) {
            stop_owned_browser_launch(self.inner.launcher.as_ref(), browser_id, &mut handle)
                .await?;
            rollback_browser_launch(&self.inner.paths, state, browser_id)?;
            return Err(error);
        }
        spawn_browser_supervisor_reaper(&mut handle);
        Ok(session)
    }

    pub async fn close(&self, id: BrowserSessionId) -> Result<(), BrowserError> {
        let _operation = self.inner.operation.lock().await;
        let mut guard = self.inner.state.lock().await;
        let state = state_mut(&mut guard)?;
        self.reconcile(state).await?;
        let record = state
            .sessions
            .get(&id)
            .cloned()
            .ok_or(BrowserError::SessionNotFound)?;
        stop_browser_group(self.inner.launcher.as_ref(), record.supervisor_pid, id).await?;
        cleanup_session_paths(&self.inner.paths, id, record.display)?;
        remove_browser_record(&self.inner.paths, state, id)?;
        Ok(())
    }

    pub async fn connect_rfb(&self, id: BrowserSessionId) -> Result<UnixStream, BrowserError> {
        let socket = {
            let _operation = self.inner.operation.lock().await;
            let mut guard = self.inner.state.lock().await;
            let state = state_mut(&mut guard)?;
            self.reconcile(state).await?;
            let record = state
                .sessions
                .get(&id)
                .ok_or(BrowserError::SessionNotFound)?;
            if record.session.state != BrowserSessionState::Running {
                return Err(BrowserError::SessionNotRunning);
            }
            record.rfb_socket.clone()
        };
        UnixStream::connect(socket)
            .await
            .map_err(|_| BrowserError::RfbUnavailable)
    }

    pub async fn navigate(
        &self,
        id: BrowserSessionId,
        url: &str,
    ) -> Result<BrowserPage, BrowserError> {
        let url = validate_browser_url(url)?;
        match self
            .automate(id, AutomationRequest::Navigate { url })
            .await?
        {
            AutomationResult::Page(page) => Ok(page),
            _ => Err(BrowserError::AutomationFailed),
        }
    }

    pub async fn snapshot(&self, id: BrowserSessionId) -> Result<BrowserSnapshot, BrowserError> {
        match self.automate(id, AutomationRequest::Snapshot).await? {
            AutomationResult::Snapshot(snapshot) => Ok(snapshot),
            _ => Err(BrowserError::AutomationFailed),
        }
    }

    pub async fn click(
        &self,
        id: BrowserSessionId,
        selector: &str,
    ) -> Result<BrowserPage, BrowserError> {
        validate_selector(selector)?;
        match self
            .automate(
                id,
                AutomationRequest::Click {
                    selector: selector.to_owned(),
                },
            )
            .await?
        {
            AutomationResult::Page(page) => Ok(page),
            _ => Err(BrowserError::AutomationFailed),
        }
    }

    pub async fn fill(
        &self,
        id: BrowserSessionId,
        selector: &str,
        value: &str,
    ) -> Result<BrowserPage, BrowserError> {
        validate_selector(selector)?;
        if value.len() > BROWSER_FILL_VALUE_MAX_BYTES {
            return Err(BrowserError::AutomationRequestInvalid);
        }
        match self
            .automate(
                id,
                AutomationRequest::Fill {
                    selector: selector.to_owned(),
                    value: value.to_owned(),
                },
            )
            .await?
        {
            AutomationResult::Page(page) => Ok(page),
            _ => Err(BrowserError::AutomationFailed),
        }
    }

    pub async fn wait_download(
        &self,
        id: BrowserSessionId,
        timeout_ms: u64,
    ) -> Result<BrowserDownload, BrowserError> {
        if !(BROWSER_DOWNLOAD_WAIT_MIN_MS..=BROWSER_DOWNLOAD_WAIT_MAX_MS).contains(&timeout_ms) {
            return Err(BrowserError::AutomationRequestInvalid);
        }
        match self
            .automate(id, AutomationRequest::WaitDownload { timeout_ms })
            .await?
        {
            AutomationResult::Download(download) => Ok(download),
            _ => Err(BrowserError::AutomationFailed),
        }
    }

    async fn automate(
        &self,
        id: BrowserSessionId,
        request: AutomationRequest,
    ) -> Result<AutomationResult, BrowserError> {
        let socket = {
            let _operation = self.inner.operation.lock().await;
            let mut guard = self.inner.state.lock().await;
            let state = state_mut(&mut guard)?;
            self.reconcile(state).await?;
            let record = state
                .sessions
                .get(&id)
                .ok_or(BrowserError::SessionNotFound)?;
            if record.session.state != BrowserSessionState::Running {
                return Err(BrowserError::SessionNotRunning);
            }
            record
                .rfb_socket
                .parent()
                .ok_or(BrowserError::StateInvalid)?
                .join(AUTOMATION_SOCKET_FILE_NAME)
        };
        automation::request(&socket, request).await
    }

    pub(crate) async fn completed_downloads(
        &self,
    ) -> Result<Vec<BrowserDownloadCandidate>, BrowserError> {
        let active_sessions = {
            let _operation = self.inner.operation.lock().await;
            let mut guard = self.inner.state.lock().await;
            let state = state_mut(&mut guard)?;
            self.reconcile(state).await?;
            state.sessions.keys().map(|id| id.0).collect::<HashSet<_>>()
        };
        let root = self.inner.paths.downloads.clone();
        let downloads =
            tokio::task::spawn_blocking(move || scan_downloads(&root, &active_sessions))
                .await
                .map_err(|_| BrowserError::StorageUnavailable)?
                .map_err(|_| BrowserError::StorageUnavailable)?;
        Ok(self.inner.download_tracker.lock().await.observe(downloads))
    }

    pub(crate) async fn mark_download_handled(&self, candidate: BrowserDownloadCandidate) {
        self.inner
            .download_tracker
            .lock()
            .await
            .mark_handled(candidate);
    }

    pub(crate) async fn inspect_download(
        &self,
        candidate: &BrowserDownloadCandidate,
    ) -> Result<InspectedFileOffer, FileOfferError> {
        if let Some(inspected) = self
            .inner
            .download_tracker
            .lock()
            .await
            .cached_inspection(candidate)
        {
            return Ok(inspected);
        }
        let candidate_to_inspect = candidate.clone();
        let inspected = tokio::task::spawn_blocking(move || candidate_to_inspect.inspect())
            .await
            .map_err(|_| FileOfferError::Unreadable)??;
        self.inner
            .download_tracker
            .lock()
            .await
            .cache_inspection(candidate.clone(), inspected.clone());
        Ok(inspected)
    }

    async fn reconcile(&self, state: &mut BrowserWorkspaceState) -> Result<(), BrowserError> {
        let original = state.clone();
        let dead = state
            .sessions
            .iter()
            .filter_map(|(id, record)| {
                (!self.inner.launcher.is_alive(record.supervisor_pid, *id)).then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in &dead {
            let record = state.sessions.get(id).ok_or(BrowserError::StateInvalid)?;
            if record.supervisor_pid == 0 {
                cleanup_private_session_paths(&self.inner.paths, *id)?;
            } else {
                stop_browser_group(self.inner.launcher.as_ref(), record.supervisor_pid, *id)
                    .await?;
                cleanup_session_paths(&self.inner.paths, *id, record.display)?;
            }
        }
        let mut changed = !dead.is_empty();
        for id in dead {
            state.sessions.remove(&id);
        }
        for (id, record) in &mut state.sessions {
            if record.session.state == BrowserSessionState::Created
                && browser_record_is_ready(*id, record)
            {
                record.session.state = BrowserSessionState::Running;
                changed = true;
            }
        }
        if !changed {
            return Ok(());
        }
        if let Err(error) = persist_browser_state(&self.inner.paths, state) {
            *state = original;
            return Err(error);
        }
        Ok(())
    }

    async fn runtime_paths(&self) -> Result<BrowserRuntimePaths, BrowserError> {
        #[cfg(test)]
        if let Some(runtime) = &self.inner.fixed_runtime {
            return Ok(runtime.clone());
        }
        self.inner.runtime.ensure().await
    }

    fn chromium_path(&self) -> Result<PathBuf, BrowserError> {
        #[cfg(test)]
        if let Some(chromium) = &self.inner.fixed_chromium {
            return Ok(chromium.clone());
        }
        resolve_chromium()
    }
}

impl BrowserPaths {
    fn for_state_path(state_path: &Path) -> Self {
        let parent = state_path.parent().unwrap_or_else(|| Path::new("."));
        let root = parent.join("browser-workspaces");
        let profiles = browser_profile_root(&root);
        let downloads = browser_download_root(&root);
        #[cfg(test)]
        let (x11_sockets, x11_locks) = (root.join("test-x11-sockets"), root.join("test-x11-locks"));
        #[cfg(not(test))]
        let (x11_sockets, x11_locks) = (PathBuf::from("/tmp/.X11-unix"), PathBuf::from("/tmp"));
        Self {
            records: root.join("sessions.json"),
            run: root.join("run"),
            short_run: parent.join("br"),
            profiles,
            downloads,
            configs: root.join("openbox"),
            x11_sockets,
            x11_locks,
            root,
        }
    }

    fn session_run_dir(&self, id: BrowserSessionId) -> Result<PathBuf, BrowserError> {
        let preferred = self.run.join(id.to_string());
        if unix_socket_path_fits(&preferred.join("rfb.sock")) {
            return Ok(preferred);
        }
        let fallback = self.short_run.join(id.to_string());
        if unix_socket_path_fits(&fallback.join("rfb.sock")) {
            return Ok(fallback);
        }
        Err(BrowserError::StorageUnavailable)
    }

    fn expected_record_paths(
        &self,
        id: BrowserSessionId,
    ) -> Result<(PathBuf, PathBuf, PathBuf, PathBuf), BrowserError> {
        let run_dir = self.session_run_dir(id)?;
        Ok((
            run_dir.join("rfb.sock"),
            run_dir.join("ready"),
            self.profiles.join(id.to_string()),
            self.configs.join(format!("{id}.xml")),
        ))
    }
}

impl BrowserLauncher for ProcessBrowserLauncher {
    fn launch(
        &self,
        id: BrowserSessionId,
        args: &BrowserSupervisorArgs,
    ) -> Result<BrowserLaunchHandle, BrowserError> {
        let payload = serde_json::to_vec(args).map_err(|_| BrowserError::SupervisorStartFailed)?;
        let mut child = Command::new(&self.binary)
            .arg("__browser-supervisor")
            .arg(id.to_string())
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| BrowserError::SupervisorStartFailed)?;
        let pid = child.id();
        let write_result = child
            .stdin
            .take()
            .ok_or(BrowserError::SupervisorStartFailed)
            .and_then(|mut stdin| {
                stdin
                    .write_all(&payload)
                    .map_err(|_| BrowserError::SupervisorStartFailed)
            });
        if let Err(error) = write_result {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(BrowserLaunchHandle {
            pid,
            child: Some(child),
        })
    }

    fn is_alive(&self, pid: u32, id: BrowserSessionId) -> bool {
        process_matches_browser_supervisor(pid, id)
    }

    fn terminate(&self, pid: u32, id: BrowserSessionId, signal: i32) -> bool {
        if !self.is_alive(pid, id) {
            return false;
        }
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: process identity is checked immediately before signalling, and no pointer is used.
        unsafe { libc::kill(pid, signal) == 0 }
    }

    fn group_is_alive(&self, pid: u32, id: BrowserSessionId) -> bool {
        browser_process_group_is_owned(pid, id)
    }

    fn terminate_group(&self, pid: u32, id: BrowserSessionId, signal: i32) -> bool {
        if !browser_process_group_is_owned(pid, id) {
            return false;
        }
        let Ok(pgid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: ownership and group membership are checked immediately before signalling.
        unsafe { libc::kill(-pgid, signal) == 0 }
    }
}

struct UnavailableBrowserLauncher;

impl BrowserLauncher for UnavailableBrowserLauncher {
    fn launch(
        &self,
        _id: BrowserSessionId,
        _args: &BrowserSupervisorArgs,
    ) -> Result<BrowserLaunchHandle, BrowserError> {
        Err(BrowserError::SupervisorStartFailed)
    }

    fn is_alive(&self, _pid: u32, _id: BrowserSessionId) -> bool {
        false
    }

    fn terminate(&self, _pid: u32, _id: BrowserSessionId, _signal: i32) -> bool {
        false
    }
}

fn prepare_browser_paths(paths: &BrowserPaths) -> Result<(), BrowserError> {
    for path in [&paths.root, &paths.run, &paths.configs] {
        create_private_dir(path)?;
    }
    if !unix_socket_path_fits(&paths.run.join(UUID_PATH_PLACEHOLDER).join("rfb.sock"))
        && unix_socket_path_fits(&paths.short_run.join(UUID_PATH_PLACEHOLDER).join("rfb.sock"))
    {
        create_private_dir(&paths.short_run)?;
    }
    create_chromium_root(&paths.profiles)?;
    create_chromium_root(&paths.downloads)
}

fn browser_profile_root(browser_root: &Path) -> PathBuf {
    #[cfg(test)]
    {
        browser_root.join("profiles")
    }
    #[cfg(not(test))]
    {
        let _ = browser_root;
        PathBuf::from(BROWSER_PROFILE_ROOT)
    }
}

fn browser_download_root(browser_root: &Path) -> PathBuf {
    #[cfg(test)]
    {
        browser_root.join("downloads")
    }
    #[cfg(not(test))]
    {
        let _ = browser_root;
        PathBuf::from(BROWSER_DOWNLOAD_ROOT)
    }
}

fn create_chromium_root(path: &Path) -> Result<(), BrowserError> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(BrowserError::StorageUnavailable),
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| BrowserError::StorageUnavailable)?;
    // The directory is deliberately execute-only to the Chromium account. It can
    // traverse to its UUID profile but cannot enumerate other browser sessions.
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(BrowserError::StorageUnavailable);
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o711))
        .map_err(|_| BrowserError::StorageUnavailable)
}

fn create_private_dir(path: &Path) -> Result<(), BrowserError> {
    fs::create_dir_all(path).map_err(|_| BrowserError::StorageUnavailable)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| BrowserError::StorageUnavailable)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err(BrowserError::StorageUnavailable);
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| BrowserError::StorageUnavailable)
}

fn load_browser_state(paths: &BrowserPaths) -> Result<BrowserWorkspaceState, BrowserError> {
    if !paths.records.exists() {
        return Ok(BrowserWorkspaceState::default());
    }
    let metadata = fs::symlink_metadata(&paths.records).map_err(|_| BrowserError::StateInvalid)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > BROWSER_STORE_MAX_BYTES
    {
        return Err(BrowserError::StateInvalid);
    }
    let bytes = fs::read(&paths.records).map_err(|_| BrowserError::StateInvalid)?;
    let store: BrowserStoreFile =
        serde_json::from_slice(&bytes).map_err(|_| BrowserError::StateInvalid)?;
    if store.schema_version != BROWSER_STORE_SCHEMA || store.sessions.len() > BROWSER_SESSION_LIMIT
    {
        return Err(BrowserError::StateInvalid);
    }
    let mut sessions = BTreeMap::new();
    for record in store.sessions {
        validate_record(paths, &record)?;
        if sessions.insert(record.session.browser_id, record).is_some() {
            return Err(BrowserError::StateInvalid);
        }
    }
    Ok(BrowserWorkspaceState { sessions })
}

fn validate_record(
    paths: &BrowserPaths,
    record: &BrowserSessionRecord,
) -> Result<(), BrowserError> {
    let id = record.session.browser_id;
    let (rfb_socket, ready_file, profile_dir, openbox_config) = paths
        .expected_record_paths(id)
        .map_err(|_| BrowserError::StateInvalid)?;
    validate_browser_url(&record.launch_url).map_err(|_| BrowserError::StateInvalid)?;
    validate_viewport(record.session.width, record.session.height)?;
    if !(100..=999).contains(&record.display)
        || (record.supervisor_pid == 0 && record.session.state != BrowserSessionState::Created)
        || record.rfb_socket != rfb_socket
        || record.ready_file != ready_file
        || record.profile_dir != profile_dir
        || record.openbox_config != openbox_config
        || record.session.state == BrowserSessionState::Closed
        || record.session.display_url != display_url(&record.launch_url)
    {
        return Err(BrowserError::StateInvalid);
    }
    Ok(())
}

fn persist_browser_state(
    paths: &BrowserPaths,
    state: &BrowserWorkspaceState,
) -> Result<(), BrowserError> {
    let store = BrowserStoreFile {
        schema_version: BROWSER_STORE_SCHEMA,
        sessions: state.sessions.values().cloned().collect(),
    };
    let bytes = serde_json::to_vec_pretty(&store).map_err(|_| BrowserError::StateWriteFailed)?;
    if bytes.len() as u64 > BROWSER_STORE_MAX_BYTES {
        return Err(BrowserError::StateWriteFailed);
    }
    let mut temporary =
        NamedTempFile::new_in(&paths.root).map_err(|_| BrowserError::StateWriteFailed)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| BrowserError::StateWriteFailed)?;
    temporary
        .write_all(&bytes)
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|_| BrowserError::StateWriteFailed)?;
    temporary
        .persist(&paths.records)
        .map_err(|_| BrowserError::StateWriteFailed)?;
    Ok(())
}

fn state_mut(
    state: &mut Result<BrowserWorkspaceState, BrowserError>,
) -> Result<&mut BrowserWorkspaceState, BrowserError> {
    state.as_mut().map_err(|error| *error)
}

fn validate_browser_url(raw: &str) -> Result<String, BrowserError> {
    if raw.is_empty() || raw.len() > BROWSER_URL_MAX_BYTES {
        return Err(BrowserError::InvalidUrl);
    }
    let parsed = Url::parse(raw).map_err(|_| BrowserError::InvalidUrl)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(BrowserError::InvalidUrl);
    }
    Ok(parsed.to_string())
}

fn display_url(raw: &str) -> String {
    let Ok(mut parsed) = Url::parse(raw) else {
        return raw.to_owned();
    };
    parsed.set_query(None);
    parsed.set_fragment(None);
    parsed.to_string()
}

fn validate_viewport(width: u16, height: u16) -> Result<(), BrowserError> {
    if !(MIN_BROWSER_WIDTH..=MAX_BROWSER_WIDTH).contains(&width)
        || !(MIN_BROWSER_HEIGHT..=MAX_BROWSER_HEIGHT).contains(&height)
    {
        return Err(BrowserError::InvalidViewport);
    }
    Ok(())
}

fn validate_selector(selector: &str) -> Result<(), BrowserError> {
    if selector.trim().is_empty()
        || selector.len() > BROWSER_SELECTOR_MAX_BYTES
        || selector.contains('\0')
    {
        return Err(BrowserError::AutomationRequestInvalid);
    }
    Ok(())
}

fn allocate_display(
    paths: &BrowserPaths,
    state: &BrowserWorkspaceState,
) -> Result<u16, BrowserError> {
    let used = state
        .sessions
        .values()
        .map(|record| record.display)
        .collect::<HashSet<_>>();
    (100..=999)
        .find(|display| {
            !used.contains(display)
                && !paths.x11_sockets.join(format!("X{display}")).exists()
                && !paths.x11_locks.join(format!(".X{display}-lock")).exists()
        })
        .ok_or(BrowserError::CapacityExceeded)
}

async fn wait_for_browser_ready(
    launcher: &dyn BrowserLauncher,
    id: BrowserSessionId,
    handle: &mut BrowserLaunchHandle,
    record: &BrowserSessionRecord,
    automation_socket: &Path,
) -> Result<(), BrowserError> {
    let deadline = Instant::now() + BROWSER_START_TIMEOUT;
    loop {
        let automation_ready = fs::symlink_metadata(automation_socket)
            .map(|metadata| metadata.file_type().is_socket())
            .unwrap_or(false);
        if browser_record_is_ready(id, record)
            && automation_ready
            && launcher.is_alive(record.supervisor_pid, id)
        {
            return Ok(());
        }
        if let Some(child) = &mut handle.child
            && child
                .try_wait()
                .map_err(|_| BrowserError::SupervisorStartFailed)?
                .is_some()
        {
            return Err(BrowserError::SupervisorStartFailed);
        }
        if !launcher.is_alive(record.supervisor_pid, id) {
            return Err(BrowserError::SupervisorStartFailed);
        }
        if Instant::now() >= deadline {
            return Err(BrowserError::SupervisorStartTimeout);
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn browser_record_is_ready(id: BrowserSessionId, record: &BrowserSessionRecord) -> bool {
    let socket_ready = fs::symlink_metadata(&record.rfb_socket)
        .map(|metadata| metadata.file_type().is_socket())
        .unwrap_or(false);
    let ready = fs::read_to_string(&record.ready_file)
        .map(|contents| contents == id.to_string())
        .unwrap_or(false);
    socket_ready && ready
}

async fn wait_for_browser_group_stop(
    launcher: &dyn BrowserLauncher,
    pid: u32,
    id: BrowserSessionId,
) -> bool {
    let deadline = Instant::now() + BROWSER_STOP_TIMEOUT;
    loop {
        if !launcher.group_is_alive(pid, id) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn stop_browser_group(
    launcher: &dyn BrowserLauncher,
    pid: u32,
    id: BrowserSessionId,
) -> Result<(), BrowserError> {
    if !launcher.group_is_alive(pid, id) {
        return Ok(());
    }
    let _ = launcher.terminate_group(pid, id, libc::SIGTERM);
    if wait_for_browser_group_stop(launcher, pid, id).await {
        return Ok(());
    }
    let _ = launcher.terminate_group(pid, id, libc::SIGKILL);
    if wait_for_browser_group_stop(launcher, pid, id).await {
        Ok(())
    } else {
        Err(BrowserError::SupervisorStopFailed)
    }
}

async fn stop_owned_browser_launch(
    launcher: &dyn BrowserLauncher,
    id: BrowserSessionId,
    handle: &mut BrowserLaunchHandle,
) -> Result<(), BrowserError> {
    let result = stop_browser_group(launcher, handle.pid, id).await;
    if result.is_err() {
        spawn_browser_supervisor_reaper(handle);
        return result;
    }
    if let Some(mut child) = handle.child.take() {
        let _ = child.wait();
    }
    Ok(())
}

fn spawn_browser_supervisor_reaper(handle: &mut BrowserLaunchHandle) {
    let Some(mut child) = handle.child.take() else {
        return;
    };
    let pid = child.id();
    let _ = thread::Builder::new()
        .name(format!("termd-browser-reaper-{pid}"))
        .spawn(move || {
            let _ = child.wait();
        });
}

fn process_matches_browser_supervisor(pid: u32, id: BrowserSessionId) -> bool {
    let Ok(raw) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let args = raw
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .filter_map(|arg| std::str::from_utf8(arg).ok())
        .collect::<Vec<_>>();
    let id = id.to_string();
    args.windows(2)
        .any(|pair| pair == ["__browser-supervisor", id.as_str()])
}

fn browser_process_group_is_owned(pgid: u32, id: BrowserSessionId) -> bool {
    pgid != 0
        && ((process_matches_browser_supervisor(pgid, id) && process_is_in_group(pgid, pgid))
            || process_group_has_session_member(pgid, id, None))
}

#[cfg(test)]
fn process_group_member_count(pgid: u32) -> usize {
    process_entries()
        .filter(|pid| process_state_and_group(*pid).is_some_and(|(_, group)| group == pgid))
        .count()
}

fn process_group_has_session_member(
    pgid: u32,
    id: BrowserSessionId,
    excluded_pid: Option<u32>,
) -> bool {
    let expected = format!("{BROWSER_SESSION_ENV}={id}");
    process_entries().any(|pid| {
        excluded_pid != Some(pid)
            && process_state_and_group(pid)
                .is_some_and(|(state, group)| group == pgid && state != b'Z')
            && fs::read(format!("/proc/{pid}/environ"))
                .map(|environment| {
                    environment
                        .split(|byte| *byte == 0)
                        .any(|entry| entry == expected.as_bytes())
                })
                .unwrap_or(false)
    })
}

fn process_entries() -> impl Iterator<Item = u32> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
}

fn process_is_in_group(pid: u32, pgid: u32) -> bool {
    process_state_and_group(pid).is_some_and(|(_, group)| group == pgid)
}

fn process_state_and_group(pid: u32) -> Option<(u8, u32)> {
    let stat = fs::read(format!("/proc/{pid}/stat")).ok()?;
    let command_end = stat.iter().rposition(|byte| *byte == b')')?;
    let mut fields = stat
        .get(command_end + 1..)?
        .split(|byte| byte.is_ascii_whitespace());
    let state = fields.find(|field| !field.is_empty())?.first().copied()?;
    let _parent = fields.find(|field| !field.is_empty())?;
    let group = std::str::from_utf8(fields.find(|field| !field.is_empty())?)
        .ok()?
        .parse()
        .ok()?;
    Some((state, group))
}

fn unix_socket_path_fits(path: &Path) -> bool {
    path.as_os_str().as_bytes().len() <= UNIX_SOCKET_PATH_MAX_BYTES
}

fn write_openbox_config(path: &Path) -> Result<(), BrowserError> {
    const CONFIG: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<openbox_config xmlns="http://openbox.org/3.4/rc">
  <focus><focusNew>yes</focusNew><followMouse>no</followMouse></focus>
  <desktops><number>1</number></desktops>
  <applications><application class="*"><decor>yes</decor></application></applications>
</openbox_config>
"#;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| BrowserError::StateWriteFailed)?;
    file.write_all(CONFIG.as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|_| BrowserError::StateWriteFailed)
}

fn write_browser_start_file(path: &Path, browser_id: BrowserSessionId) -> Result<(), BrowserError> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| BrowserError::StateWriteFailed)?;
    file.write_all(browser_id.to_string().as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|_| BrowserError::StateWriteFailed)
}

fn rollback_browser_launch(
    paths: &BrowserPaths,
    state: &mut BrowserWorkspaceState,
    id: BrowserSessionId,
) -> Result<(), BrowserError> {
    let record = state.sessions.get(&id).ok_or(BrowserError::StateInvalid)?;
    if record.supervisor_pid == 0 {
        cleanup_private_session_paths(paths, id)?;
    } else {
        cleanup_session_paths(paths, id, record.display)?;
    }
    remove_browser_record(paths, state, id)
}

fn remove_browser_record(
    paths: &BrowserPaths,
    state: &mut BrowserWorkspaceState,
    id: BrowserSessionId,
) -> Result<(), BrowserError> {
    let record = state
        .sessions
        .remove(&id)
        .ok_or(BrowserError::StateInvalid)?;
    if let Err(error) = persist_browser_state(paths, state) {
        state.sessions.insert(id, record);
        return Err(error);
    }
    Ok(())
}

fn cleanup_session_paths(
    paths: &BrowserPaths,
    id: BrowserSessionId,
    display: u16,
) -> Result<(), BrowserError> {
    cleanup_x11_artifacts(
        &paths.x11_sockets.join(format!("X{display}")),
        &paths.x11_locks.join(format!(".X{display}-lock")),
    )?;
    cleanup_private_session_paths(paths, id)
}

fn cleanup_private_session_paths(
    paths: &BrowserPaths,
    id: BrowserSessionId,
) -> Result<(), BrowserError> {
    let run_dir = paths.session_run_dir(id)?;
    let profile_dir = paths.profiles.join(id.to_string());
    let download_dir = paths.downloads.join(id.to_string());
    let config = paths.configs.join(format!("{id}.xml"));
    cleanup_incomplete_downloads(&download_dir).map_err(|_| BrowserError::StorageUnavailable)?;
    remove_dir_all_if_exists(&run_dir)?;
    remove_dir_all_if_exists(&profile_dir)?;
    remove_file_if_exists(&config)
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

struct X11Lock {
    identity: FileIdentity,
    pid: u32,
}

fn cleanup_x11_artifacts(socket_path: &Path, lock_path: &Path) -> Result<(), BrowserError> {
    let effective_uid = unsafe { libc::geteuid() };
    let lock = read_x11_lock(lock_path, effective_uid)?;
    let socket = x11_socket_identity(socket_path, effective_uid)?;

    let Some(lock) = lock else {
        return if socket.is_none() {
            Ok(())
        } else {
            Err(BrowserError::StorageUnavailable)
        };
    };
    if process_id_is_alive(lock.pid)? {
        return Err(BrowserError::StorageUnavailable);
    }

    if let Some(socket_identity) = socket {
        if x11_socket_has_listener(socket_path)? {
            return Err(BrowserError::StorageUnavailable);
        }
        ensure_same_dead_x11_lock(lock_path, effective_uid, &lock)?;
        match x11_socket_identity(socket_path, effective_uid)? {
            Some(current) if current == socket_identity => {
                remove_file_if_exists(socket_path)?;
            }
            None => {}
            Some(_) => return Err(BrowserError::StorageUnavailable),
        }
    }

    if x11_socket_identity(socket_path, effective_uid)?.is_some() {
        return Err(BrowserError::StorageUnavailable);
    }
    ensure_same_dead_x11_lock(lock_path, effective_uid, &lock)?;
    remove_file_if_exists(lock_path)
}

fn read_x11_lock(path: &Path, effective_uid: u32) -> Result<Option<X11Lock>, BrowserError> {
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(BrowserError::StorageUnavailable),
    };
    let metadata = file
        .metadata()
        .map_err(|_| BrowserError::StorageUnavailable)?;
    if !metadata.is_file()
        || metadata.uid() != effective_uid
        || metadata.nlink() != 1
        || metadata.len() == 0
        || metadata.len() > 32
    {
        return Err(BrowserError::StorageUnavailable);
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|_| BrowserError::StorageUnavailable)?;
    let pid = contents
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 1 && i32::try_from(*pid).is_ok())
        .ok_or(BrowserError::StorageUnavailable)?;
    Ok(Some(X11Lock {
        identity: FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        },
        pid,
    }))
}

fn x11_socket_identity(
    path: &Path,
    effective_uid: u32,
) -> Result<Option<FileIdentity>, BrowserError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(BrowserError::StorageUnavailable),
    };
    if !metadata.file_type().is_socket() || metadata.uid() != effective_uid || metadata.nlink() != 1
    {
        return Err(BrowserError::StorageUnavailable);
    }
    Ok(Some(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }))
}

fn ensure_same_dead_x11_lock(
    path: &Path,
    effective_uid: u32,
    expected: &X11Lock,
) -> Result<(), BrowserError> {
    let current = read_x11_lock(path, effective_uid)?.ok_or(BrowserError::StorageUnavailable)?;
    if current.identity != expected.identity
        || current.pid != expected.pid
        || process_id_is_alive(current.pid)?
    {
        return Err(BrowserError::StorageUnavailable);
    }
    Ok(())
}

fn process_id_is_alive(pid: u32) -> Result<bool, BrowserError> {
    let pid = i32::try_from(pid).map_err(|_| BrowserError::StorageUnavailable)?;
    if unsafe { libc::kill(pid, 0) } == 0 {
        return Ok(true);
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Ok(true),
        _ => Err(BrowserError::StorageUnavailable),
    }
}

fn x11_socket_has_listener(path: &Path) -> Result<bool, BrowserError> {
    let path_bytes = path.as_os_str().as_bytes();
    let mut address = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
    if path_bytes.is_empty()
        || path_bytes.contains(&0)
        || path_bytes.len() >= address.sun_path.len()
    {
        return Err(BrowserError::StorageUnavailable);
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    unsafe {
        std::ptr::copy_nonoverlapping(
            path_bytes.as_ptr().cast::<libc::c_char>(),
            address.sun_path.as_mut_ptr(),
            path_bytes.len(),
        );
    }
    let address_len = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + path_bytes.len() + 1)
        as libc::socklen_t;
    let raw_fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if raw_fd < 0 {
        return Err(BrowserError::StorageUnavailable);
    }
    let socket = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    if unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            address_len,
        )
    } == 0
    {
        return Ok(true);
    }
    match io::Error::last_os_error().raw_os_error() {
        Some(libc::ECONNREFUSED | libc::ENOENT) => Ok(false),
        Some(libc::EAGAIN | libc::EALREADY | libc::EINPROGRESS | libc::EISCONN) => Ok(true),
        _ => Err(BrowserError::StorageUnavailable),
    }
}

fn remove_dir_all_if_exists(path: &Path) -> Result<(), BrowserError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(BrowserError::StorageUnavailable),
    }
}

fn remove_file_if_exists(path: &Path) -> Result<(), BrowserError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(BrowserError::StorageUnavailable),
    }
}

fn unix_timestamp_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    struct ProcessGroupCleanup {
        pgid: u32,
        leader: Option<Child>,
        armed: bool,
    }

    impl Drop for ProcessGroupCleanup {
        fn drop(&mut self) {
            if self.armed
                && let Ok(pgid) = libc::pid_t::try_from(self.pgid)
            {
                // SAFETY: this test created and owns the process group.
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
            if let Some(leader) = &mut self.leader {
                let _ = leader.wait();
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while process_group_member_count(self.pgid) != 0 && std::time::Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    struct FakeLauncher {
        alive: Arc<AtomicBool>,
        next_pid: AtomicU32,
        records: PathBuf,
        durable_intent_seen: Arc<AtomicBool>,
    }

    impl BrowserLauncher for FakeLauncher {
        fn launch(
            &self,
            id: BrowserSessionId,
            args: &BrowserSupervisorArgs,
        ) -> Result<BrowserLaunchHandle, BrowserError> {
            let store: BrowserStoreFile = serde_json::from_slice(
                &fs::read(&self.records).map_err(|_| BrowserError::StateInvalid)?,
            )
            .map_err(|_| BrowserError::StateInvalid)?;
            if store.sessions.iter().any(|record| {
                record.session.browser_id == id
                    && record.session.state == BrowserSessionState::Created
                    && record.supervisor_pid == 0
                    && !args.start_file.exists()
            }) {
                self.durable_intent_seen.store(true, Ordering::Release);
            }
            let listener = UnixListener::bind(&args.rfb_socket)
                .map_err(|_| BrowserError::SupervisorStartFailed)?;
            let automation_listener = UnixListener::bind(&args.automation_socket)
                .map_err(|_| BrowserError::SupervisorStartFailed)?;
            fs::write(&args.ready_file, id.to_string())
                .map_err(|_| BrowserError::SupervisorStartFailed)?;
            let alive = Arc::clone(&self.alive);
            listener.set_nonblocking(true).unwrap();
            automation_listener.set_nonblocking(true).unwrap();
            thread::spawn(move || {
                while alive.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((_stream, _)) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                    match automation_listener.accept() {
                        Ok((_stream, _)) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => break,
                    }
                }
            });
            Ok(BrowserLaunchHandle {
                pid: self.next_pid.fetch_add(1, Ordering::Relaxed),
                child: None,
            })
        }

        fn is_alive(&self, pid: u32, _id: BrowserSessionId) -> bool {
            pid != 0 && self.alive.load(Ordering::Acquire)
        }

        fn terminate(&self, _pid: u32, _id: BrowserSessionId, _signal: i32) -> bool {
            self.alive.store(false, Ordering::Release);
            true
        }
    }

    fn executable(path: &Path) {
        fs::write(path, b"fake").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn exited_process_id() -> u32 {
        let mut child = Command::new("sh").arg("-c").arg("exit 0").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    fn write_x11_lock(path: &Path, pid: u32) {
        fs::write(path, format!("{pid:>10}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o444)).unwrap();
    }

    fn test_workspace_with_probe() -> (
        tempfile::TempDir,
        BrowserWorkspace,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
    ) {
        let root = tempfile::tempdir().unwrap();
        let paths = BrowserPaths::for_state_path(&root.path().join("daemon-state.json"));
        prepare_browser_paths(&paths).unwrap();
        let runtime_root = root.path().join("runtime");
        fs::create_dir_all(runtime_root.join("bin")).unwrap();
        executable(&runtime_root.join("bin/Xtigervnc"));
        executable(&runtime_root.join("bin/openbox"));
        executable(&runtime_root.join("bin/xkbcomp"));
        let theme = runtime_root.join("share/themes/Clearlooks/openbox-3/themerc");
        fs::create_dir_all(theme.parent().unwrap()).unwrap();
        fs::write(theme, b"window.active.title.bg: flat solid").unwrap();
        let chromium = root.path().join("chromium");
        executable(&chromium);
        let alive = Arc::new(AtomicBool::new(true));
        let durable_intent_seen = Arc::new(AtomicBool::new(false));
        let launcher = Arc::new(FakeLauncher {
            alive: Arc::clone(&alive),
            next_pid: AtomicU32::new(10_000),
            records: paths.records.clone(),
            durable_intent_seen: Arc::clone(&durable_intent_seen),
        });
        let state = load_browser_state(&paths);
        let workspace = BrowserWorkspace {
            inner: Arc::new(BrowserWorkspaceInner {
                runtime: BrowserRuntimeManager::new(&paths.root),
                paths,
                launcher,
                operation: Mutex::new(()),
                state: Mutex::new(state),
                download_tracker: Mutex::new(BrowserDownloadTracker::default()),
                fixed_runtime: runtime::runtime_paths_if_valid(&runtime_root),
                fixed_chromium: Some(chromium),
            }),
        };
        (root, workspace, durable_intent_seen, alive)
    }

    fn test_workspace() -> (tempfile::TempDir, BrowserWorkspace) {
        let (root, workspace, _, _) = test_workspace_with_probe();
        (root, workspace)
    }

    #[test]
    fn url_validation_accepts_only_credential_free_http_urls() {
        assert!(validate_browser_url("https://example.com/path?q=1").is_ok());
        for invalid in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://user:pass@example.com",
            "not a url",
        ] {
            assert!(matches!(
                validate_browser_url(invalid),
                Err(BrowserError::InvalidUrl)
            ));
        }
    }

    #[test]
    fn browser_automation_keeps_the_v1_record_shape() {
        let browser_id = BrowserSessionId::new();
        let record: BrowserSessionRecord = serde_json::from_value(serde_json::json!({
            "session": {
                "browser_id": browser_id,
                "state": "running",
                "display_url": "https://example.com/",
                "width": 1280,
                "height": 800,
                "created_at_ms": 1
            },
            "launch_url": "https://example.com/",
            "supervisor_pid": 42,
            "display": 100,
            "rfb_socket": "/tmp/rfb.sock",
            "ready_file": "/tmp/ready",
            "profile_dir": "/tmp/profile",
            "openbox_config": "/tmp/openbox.xml"
        }))
        .unwrap();
        let encoded = serde_json::to_value(record).unwrap();
        assert!(encoded.get("automation_socket").is_none());
    }

    #[test]
    fn unix_socket_path_boundary_is_107_bytes() {
        use std::os::unix::ffi::OsStringExt;

        let at_limit = PathBuf::from(std::ffi::OsString::from_vec(vec![b'x'; 107]));
        let over_limit = PathBuf::from(std::ffi::OsString::from_vec(vec![b'x'; 108]));
        assert!(unix_socket_path_fits(&at_limit));
        assert!(!unix_socket_path_fits(&over_limit));
    }

    #[test]
    fn long_state_path_uses_private_short_run_root() {
        let root = tempfile::tempdir().unwrap();
        let id = BrowserSessionId::new();
        let (parent, paths) = (1..=200)
            .find_map(|padding| {
                let parent = root.path().join("x".repeat(padding));
                let paths = BrowserPaths::for_state_path(&parent.join("daemon-state.json"));
                let preferred = paths.run.join(id.to_string()).join("rfb.sock");
                let fallback = paths.short_run.join(id.to_string()).join("rfb.sock");
                (!unix_socket_path_fits(&preferred) && unix_socket_path_fits(&fallback))
                    .then_some((parent, paths))
            })
            .expect("temporary path should permit a short-root fallback boundary");
        fs::create_dir_all(&parent).unwrap();
        prepare_browser_paths(&paths).unwrap();

        assert_eq!(
            paths.session_run_dir(id).unwrap(),
            paths.short_run.join(id.to_string())
        );
        let metadata = fs::symlink_metadata(&paths.short_run).unwrap();
        assert!(metadata.is_dir());
        assert!(!metadata.file_type().is_symlink());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn socket_path_over_limit_has_no_run_directory_choice() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("x".repeat(200));
        let paths = BrowserPaths::for_state_path(&parent.join("daemon-state.json"));
        prepare_browser_paths(&paths).unwrap();
        assert!(!paths.records.exists());
        assert!(matches!(
            paths.session_run_dir(BrowserSessionId::new()),
            Err(BrowserError::StorageUnavailable)
        ));
    }

    #[test]
    fn display_allocation_avoids_existing_x11_sockets_and_locks() {
        let root = tempfile::tempdir().unwrap();
        let paths = BrowserPaths::for_state_path(&root.path().join("daemon-state.json"));
        fs::create_dir_all(&paths.x11_sockets).unwrap();
        fs::create_dir_all(&paths.x11_locks).unwrap();
        fs::write(paths.x11_locks.join(".X100-lock"), b"occupied").unwrap();
        let _listener = UnixListener::bind(paths.x11_sockets.join("X101")).unwrap();

        assert_eq!(
            allocate_display(&paths, &BrowserWorkspaceState::default()).unwrap(),
            102
        );
    }

    #[test]
    fn stale_x11_socket_and_lock_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("X100");
        let lock = root.path().join(".X100-lock");
        drop(UnixListener::bind(&socket).unwrap());
        write_x11_lock(&lock, exited_process_id());

        cleanup_x11_artifacts(&socket, &lock).unwrap();

        assert!(!socket.exists());
        assert!(!lock.exists());
    }

    #[test]
    fn stale_x11_lock_without_socket_is_removed_on_retry() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("X101");
        let lock = root.path().join(".X101-lock");
        write_x11_lock(&lock, exited_process_id());

        cleanup_x11_artifacts(&socket, &lock).unwrap();

        assert!(!lock.exists());
    }

    #[test]
    fn live_x11_listener_is_preserved() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("X102");
        let lock = root.path().join(".X102-lock");
        let _listener = UnixListener::bind(&socket).unwrap();
        write_x11_lock(&lock, exited_process_id());

        assert!(matches!(
            cleanup_x11_artifacts(&socket, &lock),
            Err(BrowserError::StorageUnavailable)
        ));
        assert!(socket.exists());
        assert!(lock.exists());
    }

    #[test]
    fn live_x11_lock_pid_is_preserved() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("X103");
        let lock = root.path().join(".X103-lock");
        drop(UnixListener::bind(&socket).unwrap());
        write_x11_lock(&lock, std::process::id());

        assert!(matches!(
            cleanup_x11_artifacts(&socket, &lock),
            Err(BrowserError::StorageUnavailable)
        ));
        assert!(socket.exists());
        assert!(lock.exists());
    }

    #[test]
    fn unexpected_x11_path_type_is_preserved() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("X104");
        let lock = root.path().join(".X104-lock");
        fs::write(&socket, b"not a socket").unwrap();
        write_x11_lock(&lock, exited_process_id());

        assert!(matches!(
            cleanup_x11_artifacts(&socket, &lock),
            Err(BrowserError::StorageUnavailable)
        ));
        assert!(socket.exists());
        assert!(lock.exists());
    }

    #[test]
    fn leaderless_owned_process_group_is_killed_without_leaking_descendants() {
        let id = BrowserSessionId::new();
        let leader = Command::new("sh")
            .arg("-c")
            .arg("trap '' HUP TERM; sh -c 'trap \"\" HUP TERM; sleep 30 & wait' & wait")
            .env(BROWSER_SESSION_ENV, id.to_string())
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pgid = leader.id();
        let mut cleanup = ProcessGroupCleanup {
            pgid,
            leader: Some(leader),
            armed: true,
        };

        let spawn_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_group_member_count(pgid) < 3 && std::time::Instant::now() < spawn_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(process_group_member_count(pgid) >= 3);
        assert!(process_group_has_session_member(pgid, id, Some(pgid)));

        let raw_pid = libc::pid_t::try_from(pgid).unwrap();
        // SAFETY: this test created and owns the process group leader.
        assert_eq!(unsafe { libc::kill(raw_pid, libc::SIGKILL) }, 0);
        cleanup.leader.as_mut().unwrap().wait().unwrap();
        assert!(!process_matches_browser_supervisor(pgid, id));
        assert!(process_group_has_session_member(pgid, id, Some(pgid)));

        let launcher = ProcessBrowserLauncher {
            binary: PathBuf::from("unused"),
        };
        assert!(launcher.group_is_alive(pgid, id));
        assert!(launcher.terminate_group(pgid, id, libc::SIGKILL));
        let stop_deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_group_member_count(pgid) != 0 && std::time::Instant::now() < stop_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(process_group_member_count(pgid), 0);
        cleanup.leader = None;
        cleanup.armed = false;
    }

    #[test]
    fn unrelated_process_group_is_not_treated_as_an_owned_browser_group() {
        let id = BrowserSessionId::new();
        let leader = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pgid = leader.id();
        let _cleanup = ProcessGroupCleanup {
            pgid,
            leader: Some(leader),
            armed: true,
        };
        let launcher = ProcessBrowserLauncher {
            binary: PathBuf::from("unused"),
        };

        assert!(!launcher.group_is_alive(pgid, id));
        assert!(!launcher.terminate_group(pgid, id, libc::SIGKILL));
    }

    #[tokio::test]
    async fn create_attach_list_and_close_cross_the_workspace_interface() {
        let (_root, workspace) = test_workspace();
        let session = workspace
            .create(BrowserCreateRequest {
                url: "https://example.com/private?token=secret".to_owned(),
                width: 1280,
                height: 800,
            })
            .await
            .unwrap();
        assert_eq!(session.state, BrowserSessionState::Running);
        assert_eq!(session.display_url, "https://example.com/private");
        assert_eq!(workspace.list().await.unwrap(), vec![session.clone()]);
        let _rfb = workspace.connect_rfb(session.browser_id).await.unwrap();
        let completed_download = workspace
            .inner
            .paths
            .downloads
            .join(session.browser_id.to_string())
            .join("report.zip");
        fs::write(&completed_download, b"report").unwrap();
        workspace.close(session.browser_id).await.unwrap();
        assert!(workspace.list().await.unwrap().is_empty());
        assert_eq!(fs::read(&completed_download).unwrap(), b"report");
        assert!(workspace.completed_downloads().await.unwrap().is_empty());
        let downloads = workspace.completed_downloads().await.unwrap();
        assert_eq!(downloads.len(), 1);
        assert_eq!(downloads[0].path(), completed_download);
    }

    #[tokio::test]
    async fn supervisor_launch_is_gated_by_a_durable_created_intent() {
        let (_root, workspace, durable_intent_seen, _) = test_workspace_with_probe();
        let session = workspace
            .create(BrowserCreateRequest {
                url: "https://example.com".to_owned(),
                width: 1280,
                height: 800,
            })
            .await
            .unwrap();

        assert!(durable_intent_seen.load(Ordering::Acquire));
        let start_file = workspace
            .inner
            .paths
            .run
            .join(session.browser_id.to_string())
            .join("start");
        assert_eq!(
            fs::read_to_string(&start_file).unwrap(),
            session.browser_id.to_string()
        );
        assert_eq!(
            start_file.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        workspace.close(session.browser_id).await.unwrap();
    }

    #[tokio::test]
    async fn download_scan_reconciles_dead_supervisor_before_preserving_partials() {
        let (_root, workspace, _, alive) = test_workspace_with_probe();
        let session = workspace
            .create(BrowserCreateRequest {
                url: "https://example.com".to_owned(),
                width: 1280,
                height: 800,
            })
            .await
            .unwrap();
        let partial = workspace
            .inner
            .paths
            .downloads
            .join(session.browser_id.to_string())
            .join("large.bin.crdownload");
        fs::write(&partial, b"partial").unwrap();

        alive.store(false, Ordering::Release);
        assert!(workspace.completed_downloads().await.unwrap().is_empty());
        assert!(!partial.exists());
        assert!(workspace.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_cleanup_keeps_the_durable_session_record_for_retry() {
        let (_root, workspace) = test_workspace();
        let session = workspace
            .create(BrowserCreateRequest {
                url: "https://example.com".to_owned(),
                width: 1280,
                height: 800,
            })
            .await
            .unwrap();
        let profile = workspace
            .inner
            .paths
            .profiles
            .join(session.browser_id.to_string());
        fs::remove_dir_all(&profile).unwrap();
        fs::write(&profile, b"not a directory").unwrap();

        assert!(matches!(
            workspace.close(session.browser_id).await,
            Err(BrowserError::StorageUnavailable)
        ));
        let persisted = load_browser_state(&workspace.inner.paths).unwrap();
        assert!(persisted.sessions.contains_key(&session.browser_id));

        fs::remove_file(&profile).unwrap();
        assert!(workspace.list().await.unwrap().is_empty());
        assert!(
            load_browser_state(&workspace.inner.paths)
                .unwrap()
                .sessions
                .is_empty()
        );
    }

    #[tokio::test]
    async fn failed_record_persist_restores_in_memory_state_for_retry() {
        let (_root, workspace) = test_workspace();
        let session = workspace
            .create(BrowserCreateRequest {
                url: "https://example.com".to_owned(),
                width: 1280,
                height: 800,
            })
            .await
            .unwrap();
        fs::remove_file(&workspace.inner.paths.records).unwrap();
        fs::create_dir(&workspace.inner.paths.records).unwrap();

        assert!(matches!(
            workspace.close(session.browser_id).await,
            Err(BrowserError::StateWriteFailed)
        ));
        {
            let guard = workspace.inner.state.lock().await;
            assert!(
                guard
                    .as_ref()
                    .unwrap()
                    .sessions
                    .contains_key(&session.browser_id)
            );
        }

        fs::remove_dir(&workspace.inner.paths.records).unwrap();
        assert!(workspace.list().await.unwrap().is_empty());
        assert!(
            load_browser_state(&workspace.inner.paths)
                .unwrap()
                .sessions
                .is_empty()
        );
    }

    #[tokio::test]
    async fn restart_reconciles_a_pre_spawn_intent_without_starting_gui_processes() {
        let root = tempfile::tempdir().unwrap();
        let state_path = root.path().join("daemon-state.json");
        let paths = BrowserPaths::for_state_path(&state_path);
        prepare_browser_paths(&paths).unwrap();
        let browser_id = BrowserSessionId::new();
        let run_dir = paths.run.join(browser_id.to_string());
        let profile_dir = paths.profiles.join(browser_id.to_string());
        let openbox_config = paths.configs.join(format!("{browser_id}.xml"));
        create_private_dir(&run_dir).unwrap();
        create_private_dir(&profile_dir).unwrap();
        write_openbox_config(&openbox_config).unwrap();
        let launch_url = "https://example.com/".to_owned();
        let record = BrowserSessionRecord {
            session: BrowserSession {
                browser_id,
                state: BrowserSessionState::Created,
                display_url: launch_url.clone(),
                width: 1280,
                height: 800,
                created_at_ms: unix_timestamp_millis(),
            },
            launch_url,
            supervisor_pid: 0,
            display: 100,
            rfb_socket: run_dir.join("rfb.sock"),
            ready_file: run_dir.join("ready"),
            profile_dir: profile_dir.clone(),
            openbox_config: openbox_config.clone(),
        };
        let mut state = BrowserWorkspaceState::default();
        state.sessions.insert(browser_id, record);
        persist_browser_state(&paths, &state).unwrap();
        fs::create_dir_all(&paths.x11_sockets).unwrap();
        fs::create_dir_all(&paths.x11_locks).unwrap();
        let x11_socket = paths.x11_sockets.join("X100");
        let x11_lock = paths.x11_locks.join(".X100-lock");
        let _unrelated_listener = UnixListener::bind(&x11_socket).unwrap();
        write_x11_lock(&x11_lock, std::process::id());

        let recovered = BrowserWorkspace::for_state_path(&state_path);
        assert!(recovered.list().await.unwrap().is_empty());
        assert!(!run_dir.exists());
        assert!(!profile_dir.exists());
        assert!(!openbox_config.exists());
        assert!(x11_socket.exists());
        assert!(x11_lock.exists());
        assert!(load_browser_state(&paths).unwrap().sessions.is_empty());
    }

    #[tokio::test]
    async fn viewport_and_capacity_are_enforced_before_unbounded_process_creation() {
        let (_root, workspace) = test_workspace();
        assert!(matches!(
            workspace
                .create(BrowserCreateRequest {
                    url: "https://example.com".to_owned(),
                    width: 320,
                    height: 200,
                })
                .await,
            Err(BrowserError::InvalidViewport)
        ));

        let mut sessions = Vec::new();
        for index in 0..BROWSER_SESSION_LIMIT {
            sessions.push(
                workspace
                    .create(BrowserCreateRequest {
                        url: format!("https://example.com/{index}"),
                        width: 1280,
                        height: 800,
                    })
                    .await
                    .unwrap(),
            );
        }
        assert!(matches!(
            workspace
                .create(BrowserCreateRequest {
                    url: "https://example.com/overflow".to_owned(),
                    width: 1280,
                    height: 800,
                })
                .await,
            Err(BrowserError::CapacityExceeded)
        ));
        workspace.close(sessions[0].browser_id).await.unwrap();
    }

    #[tokio::test]
    async fn successful_owned_supervisor_is_reaped_after_exit() {
        let child = Command::new("sh").arg("-c").arg("exit 0").spawn().unwrap();
        let pid = child.id();
        let mut handle = BrowserLaunchHandle {
            pid,
            child: Some(child),
        };
        spawn_browser_supervisor_reaper(&mut handle);

        let deadline = Instant::now() + Duration::from_secs(2);
        while Path::new(&format!("/proc/{pid}")).exists() && Instant::now() < deadline {
            sleep(Duration::from_millis(10)).await;
        }
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }
}
