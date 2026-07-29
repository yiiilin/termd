use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::time::{Instant, sleep, timeout};

use super::BrowserError;

pub(super) const AUTOMATION_SOCKET_FILE_NAME: &str = "cdp.sock";

const AUTOMATION_FRAME_MAX_BYTES: usize = 1024 * 1024;
const AUTOMATION_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const AUTOMATION_IO_TIMEOUT: Duration = Duration::from_secs(5);
const AUTOMATION_BUSY_RESPONSE_TIMEOUT: Duration = Duration::from_millis(250);
const CDP_MESSAGE_MAX_BYTES: usize = 8 * 1024 * 1024;
const CDP_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const PAGE_READY_TIMEOUT: Duration = Duration::from_secs(15);
const PAGE_READY_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DOWNLOAD_EVENT_QUEUE_MAX: usize = 64;
const DOWNLOAD_GUID_MAX_BYTES: usize = 256;
const DOWNLOAD_FILENAME_MAX_BYTES: usize = 1024;

const PAGE_INFO_EXPRESSION: &str = "(() => ({
  url: String(location.href).slice(0, 2048),
  title: String(document.title || '').slice(0, 1024)
}))()";

const SNAPSHOT_EXPRESSION: &str = r#"
(() => {
  const normalize = (value) => String(value || '').replace(/\s+/g, ' ').trim();
  const selectorFor = (element) => {
    if (element.id) {
      const selector = `#${CSS.escape(element.id)}`;
      if (selector.length <= 480 && document.querySelectorAll(selector).length === 1) {
        return selector;
      }
    }
    const parts = [];
    let current = element;
    while (current && current.nodeType === Node.ELEMENT_NODE && current !== document.documentElement) {
      let part = current.tagName.toLowerCase();
      const parent = current.parentElement;
      if (parent) {
        const siblings = Array.from(parent.children).filter((child) => child.tagName === current.tagName);
        if (siblings.length > 1) part += `:nth-of-type(${siblings.indexOf(current) + 1})`;
      }
      parts.unshift(part);
      if (parts.join(' > ').length > 480) return '';
      current = parent;
    }
    return parts.join(' > ');
  };
  const candidates = Array.from(document.querySelectorAll(
    'a,button,input,textarea,select,summary,[role="button"],[role="link"],[role="textbox"],[contenteditable="true"]'
  ));
  const elements = [];
  for (const element of candidates) {
    if (elements.length >= 128) break;
    const selector = selectorFor(element);
    if (!selector) continue;
    elements.push({
      selector,
      tag: element.tagName.toLowerCase().slice(0, 80),
      role: normalize(element.getAttribute('role')).slice(0, 80),
      text: normalize(element.innerText || element.textContent).slice(0, 160),
      aria_label: normalize(element.getAttribute('aria-label')).slice(0, 160),
      placeholder: normalize(element.getAttribute('placeholder')).slice(0, 160),
      input_type: normalize(element.getAttribute('type')).slice(0, 80),
      disabled: Boolean(element.disabled || element.getAttribute('aria-disabled') === 'true')
    });
  }
  const bodyText = normalize(document.body ? document.body.innerText : '');
  return {
    url: String(location.href).slice(0, 2048),
    title: String(document.title || '').slice(0, 1024),
    text: bodyText.slice(0, 16000),
    text_truncated: bodyText.length > 16000,
    elements,
    elements_truncated: candidates.length > elements.length
  };
})()
"#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserPage {
    pub url: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSnapshotElement {
    pub selector: String,
    pub tag: String,
    pub role: String,
    pub text: String,
    pub aria_label: String,
    pub placeholder: String,
    pub input_type: String,
    pub disabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSnapshot {
    pub url: String,
    pub title: String,
    pub text: String,
    pub text_truncated: bool,
    pub elements: Vec<BrowserSnapshotElement>,
    pub elements_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserDownload {
    pub guid: String,
    pub suggested_filename: String,
    pub url: String,
    pub total_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum AutomationRequest {
    Navigate { url: String },
    Snapshot,
    Click { selector: String },
    Fill { selector: String, value: String },
    WaitDownload { timeout_ms: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(super) enum AutomationResult {
    Page(BrowserPage),
    Snapshot(BrowserSnapshot),
    Download(BrowserDownload),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AutomationFailure {
    InvalidRequest,
    TargetNotFound,
    TimedOut,
    Busy,
    Failed,
}

impl AutomationFailure {
    fn browser_error(self) -> BrowserError {
        match self {
            Self::InvalidRequest => BrowserError::AutomationRequestInvalid,
            Self::TargetNotFound => BrowserError::AutomationTargetNotFound,
            Self::TimedOut => BrowserError::AutomationTimeout,
            Self::Busy => BrowserError::AutomationBusy,
            Self::Failed => BrowserError::AutomationFailed,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", content = "payload", rename_all = "snake_case")]
enum AutomationResponse {
    Ok(AutomationResult),
    Error(AutomationFailure),
}

pub(super) struct ChromiumCdpPipes {
    command_writer: UnixStream,
    response_reader: UnixStream,
    browser_command_reader: OwnedFd,
    browser_response_writer: OwnedFd,
}

impl ChromiumCdpPipes {
    pub(super) fn new() -> Result<Self, BrowserError> {
        let (command_writer, browser_command_reader) =
            StdUnixStream::pair().map_err(|_| BrowserError::SupervisorStartFailed)?;
        let (browser_response_writer, response_reader) =
            StdUnixStream::pair().map_err(|_| BrowserError::SupervisorStartFailed)?;
        let browser_command_reader = duplicate_high_fd(browser_command_reader.as_raw_fd())
            .map_err(|_| BrowserError::SupervisorStartFailed)?;
        let browser_response_writer = duplicate_high_fd(browser_response_writer.as_raw_fd())
            .map_err(|_| BrowserError::SupervisorStartFailed)?;
        command_writer
            .set_nonblocking(true)
            .map_err(|_| BrowserError::SupervisorStartFailed)?;
        response_reader
            .set_nonblocking(true)
            .map_err(|_| BrowserError::SupervisorStartFailed)?;
        Ok(Self {
            command_writer: UnixStream::from_std(command_writer)
                .map_err(|_| BrowserError::SupervisorStartFailed)?,
            response_reader: UnixStream::from_std(response_reader)
                .map_err(|_| BrowserError::SupervisorStartFailed)?,
            browser_command_reader,
            browser_response_writer,
        })
    }

    pub(super) fn configure_child(&self, command: &mut Command) {
        let browser_command_reader = self.browser_command_reader.as_raw_fd();
        let browser_response_writer = self.browser_response_writer.as_raw_fd();
        // SAFETY: only async-signal-safe descriptor operations run between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(browser_command_reader, 3) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::dup2(browser_response_writer, 4) == -1 {
                    return Err(io::Error::last_os_error());
                }
                libc::close(browser_command_reader);
                libc::close(browser_response_writer);
                Ok(())
            });
        }
    }

    pub(super) fn into_connection(self) -> CdpConnection {
        CdpConnection {
            reader: BufReader::new(self.response_reader),
            writer: self.command_writer,
            next_id: 1,
            page_session_id: None,
            read_buffer: Vec::new(),
            discarding_message: false,
            pending_downloads: HashMap::new(),
            completed_downloads: VecDeque::new(),
        }
    }
}

fn duplicate_high_fd(fd: RawFd) -> io::Result<OwnedFd> {
    let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if duplicated == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fcntl returned a new owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

pub(super) fn bind_automation_socket(path: &Path) -> Result<UnixListener, BrowserError> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() } =>
        {
            fs::remove_file(path).map_err(|_| BrowserError::SupervisorStartFailed)?;
        }
        Ok(_) => return Err(BrowserError::SupervisorStartFailed),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return Err(BrowserError::SupervisorStartFailed),
    }
    let listener = UnixListener::bind(path).map_err(|_| BrowserError::SupervisorStartFailed)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| BrowserError::SupervisorStartFailed)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| BrowserError::SupervisorStartFailed)?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err(BrowserError::SupervisorStartFailed);
    }
    Ok(listener)
}

pub(super) async fn serve(
    listener: UnixListener,
    mut cdp: CdpConnection,
) -> Result<(), BrowserError> {
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => {
                Some(accepted.map_err(|_| BrowserError::AutomationFailed)?)
            }
            message = cdp.read_message() => {
                let message = message.map_err(AutomationFailure::browser_error)?;
                cdp.handle_event(&message);
                None
            }
        };
        let Some((mut stream, _)) = accepted else {
            continue;
        };
        let response = match timeout(AUTOMATION_IO_TIMEOUT, read_frame(&mut stream)).await {
            Ok(Ok(frame)) => match serde_json::from_slice::<AutomationRequest>(&frame) {
                Ok(request) if request_peer_is_open(&stream) => {
                    execute_while_rejecting_busy(&listener, &mut cdp, request).await?
                }
                Ok(_) => continue,
                Err(_) => AutomationResponse::Error(AutomationFailure::InvalidRequest),
            },
            Ok(Err(_)) => AutomationResponse::Error(AutomationFailure::InvalidRequest),
            Err(_) => AutomationResponse::Error(AutomationFailure::TimedOut),
        };
        let response = encode_automation_response(&response)?;
        if timeout(AUTOMATION_IO_TIMEOUT, write_frame(&mut stream, &response))
            .await
            .is_err()
        {
            continue;
        }
    }
}

fn request_peer_is_open(stream: &UnixStream) -> bool {
    let mut trailing = [0_u8; 1];
    matches!(
        stream.try_read(&mut trailing),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock
    )
}

async fn execute_while_rejecting_busy(
    listener: &UnixListener,
    cdp: &mut CdpConnection,
    request: AutomationRequest,
) -> Result<AutomationResponse, BrowserError> {
    let execution = cdp.execute(request);
    tokio::pin!(execution);
    loop {
        tokio::select! {
            biased;
            result = &mut execution => {
                return Ok(match result {
                    Ok(result) => AutomationResponse::Ok(result),
                    Err(failure) => AutomationResponse::Error(failure),
                });
            }
            accepted = listener.accept() => {
                let (mut stream, _) = accepted.map_err(|_| BrowserError::AutomationFailed)?;
                let response = encode_automation_response(&AutomationResponse::Error(
                    AutomationFailure::Busy,
                ))?;
                let _ = timeout(
                    AUTOMATION_BUSY_RESPONSE_TIMEOUT,
                    write_frame(&mut stream, &response),
                )
                .await;
            }
        }
    }
}

fn encode_automation_response(response: &AutomationResponse) -> Result<Vec<u8>, BrowserError> {
    let encoded = serde_json::to_vec(response).map_err(|_| BrowserError::AutomationFailed)?;
    if encoded.len() <= AUTOMATION_FRAME_MAX_BYTES {
        return Ok(encoded);
    }
    serde_json::to_vec(&AutomationResponse::Error(AutomationFailure::Failed))
        .map_err(|_| BrowserError::AutomationFailed)
}

pub(super) async fn request(
    socket: &Path,
    request: AutomationRequest,
) -> Result<AutomationResult, BrowserError> {
    let mut stream = timeout(AUTOMATION_CONNECT_TIMEOUT, UnixStream::connect(socket))
        .await
        .map_err(|_| BrowserError::AutomationTimeout)?
        .map_err(|_| BrowserError::AutomationUnavailable)?;
    let request =
        serde_json::to_vec(&request).map_err(|_| BrowserError::AutomationRequestInvalid)?;
    timeout(AUTOMATION_IO_TIMEOUT, write_frame(&mut stream, &request))
        .await
        .map_err(|_| BrowserError::AutomationTimeout)?
        .map_err(|_| BrowserError::AutomationUnavailable)?;
    let response_timeout = match request_timeout(&request) {
        Some(timeout) => timeout
            .saturating_add(CDP_COMMAND_TIMEOUT)
            .saturating_add(AUTOMATION_IO_TIMEOUT),
        None => CDP_COMMAND_TIMEOUT.saturating_add(AUTOMATION_IO_TIMEOUT),
    };
    let frame = timeout(response_timeout, read_frame(&mut stream))
        .await
        .map_err(|_| BrowserError::AutomationTimeout)?
        .map_err(|_| BrowserError::AutomationUnavailable)?;
    match serde_json::from_slice::<AutomationResponse>(&frame)
        .map_err(|_| BrowserError::AutomationFailed)?
    {
        AutomationResponse::Ok(result) => Ok(result),
        AutomationResponse::Error(failure) => Err(failure.browser_error()),
    }
}

fn request_timeout(encoded_request: &[u8]) -> Option<Duration> {
    let request = serde_json::from_slice::<AutomationRequest>(encoded_request).ok()?;
    match request {
        AutomationRequest::WaitDownload { timeout_ms } => Some(Duration::from_millis(timeout_ms)),
        AutomationRequest::Navigate { .. } => Some(
            PAGE_READY_TIMEOUT
                .saturating_mul(2)
                .saturating_add(CDP_COMMAND_TIMEOUT.saturating_mul(2)),
        ),
        AutomationRequest::Click { .. } | AutomationRequest::Fill { .. } => {
            Some(CDP_COMMAND_TIMEOUT.saturating_mul(2))
        }
        AutomationRequest::Snapshot => Some(CDP_COMMAND_TIMEOUT),
    }
}

async fn read_frame(stream: &mut UnixStream) -> io::Result<Vec<u8>> {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > AUTOMATION_FRAME_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "automation frame length is invalid",
        ));
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

async fn write_frame(stream: &mut UnixStream, payload: &[u8]) -> io::Result<()> {
    let length = u32::try_from(payload.len()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "automation frame is too large")
    })?;
    if payload.is_empty() || payload.len() > AUTOMATION_FRAME_MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "automation frame length is invalid",
        ));
    }
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(payload).await
}

#[derive(Debug, Clone)]
struct PendingDownload {
    suggested_filename: String,
    url: String,
}

pub(super) struct CdpConnection {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
    page_session_id: Option<String>,
    read_buffer: Vec<u8>,
    discarding_message: bool,
    pending_downloads: HashMap<String, PendingDownload>,
    completed_downloads: VecDeque<BrowserDownload>,
}

impl CdpConnection {
    pub(super) async fn initialize(
        &mut self,
        download_dir: &Path,
        initial_url: &str,
    ) -> Result<(), BrowserError> {
        self.send_command(
            "Browser.setDownloadBehavior",
            json!({
                "behavior": "allow",
                "downloadPath": download_dir,
                "eventsEnabled": true
            }),
            None,
        )
        .await
        .map_err(AutomationFailure::browser_error)?;
        self.ensure_page_session()
            .await
            .map_err(AutomationFailure::browser_error)?;
        self.begin_initial_navigation(initial_url)
            .await
            .map_err(AutomationFailure::browser_error)
    }

    async fn begin_initial_navigation(&mut self, url: &str) -> Result<(), AutomationFailure> {
        let session = self.ensure_page_session().await?;
        let navigation = self
            .send_command("Page.navigate", json!({"url": url}), Some(&session))
            .await?;
        initial_navigation_was_accepted(&navigation)
            .then_some(())
            .ok_or(AutomationFailure::Failed)
    }

    async fn execute(
        &mut self,
        request: AutomationRequest,
    ) -> Result<AutomationResult, AutomationFailure> {
        match request {
            AutomationRequest::Navigate { url } => {
                let url = super::validate_browser_url(&url)
                    .map_err(|_| AutomationFailure::InvalidRequest)?;
                self.navigate(&url).await.map(AutomationResult::Page)
            }
            AutomationRequest::Snapshot => self.snapshot().await.map(AutomationResult::Snapshot),
            AutomationRequest::Click { selector } => {
                super::validate_selector(&selector)
                    .map_err(|_| AutomationFailure::InvalidRequest)?;
                self.click(&selector).await.map(AutomationResult::Page)
            }
            AutomationRequest::Fill { selector, value } => {
                super::validate_selector(&selector)
                    .map_err(|_| AutomationFailure::InvalidRequest)?;
                if value.len() > super::BROWSER_FILL_VALUE_MAX_BYTES {
                    return Err(AutomationFailure::InvalidRequest);
                }
                self.fill(&selector, &value)
                    .await
                    .map(AutomationResult::Page)
            }
            AutomationRequest::WaitDownload { timeout_ms } => {
                if !(super::BROWSER_DOWNLOAD_WAIT_MIN_MS..=super::BROWSER_DOWNLOAD_WAIT_MAX_MS)
                    .contains(&timeout_ms)
                {
                    return Err(AutomationFailure::InvalidRequest);
                }
                self.wait_download(Duration::from_millis(timeout_ms))
                    .await
                    .map(AutomationResult::Download)
            }
        }
    }

    async fn navigate(&mut self, url: &str) -> Result<BrowserPage, AutomationFailure> {
        let session = self.ensure_page_session().await?;
        let navigation = self
            .send_command("Page.navigate", json!({"url": url}), Some(&session))
            .await?;
        let is_download = navigation.get("isDownload").and_then(Value::as_bool) == Some(true);
        if navigation_has_fatal_error(&navigation) {
            return Err(AutomationFailure::Failed);
        }
        let frame_id = navigation
            .get("frameId")
            .and_then(Value::as_str)
            .filter(|frame_id| !frame_id.is_empty())
            .ok_or(AutomationFailure::Failed)?;
        if !is_download {
            let loader_id = navigation
                .get("loaderId")
                .and_then(Value::as_str)
                .filter(|loader_id| !loader_id.is_empty());
            self.wait_for_navigation_commit(&session, frame_id, loader_id, url)
                .await?;
            self.wait_for_page_ready(&session).await?;
        }
        self.page_info(&session).await
    }

    async fn wait_for_navigation_commit(
        &mut self,
        session: &str,
        frame_id: &str,
        loader_id: Option<&str>,
        expected_url: &str,
    ) -> Result<(), AutomationFailure> {
        let deadline = Instant::now() + PAGE_READY_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AutomationFailure::TimedOut);
            }
            let frame = timeout(remaining, self.main_frame(session))
                .await
                .map_err(|_| AutomationFailure::TimedOut)?;
            match frame {
                Ok(frame) if navigation_is_committed(&frame, frame_id, loader_id, expected_url) => {
                    return Ok(());
                }
                Ok(_) | Err(AutomationFailure::Failed) => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(AutomationFailure::TimedOut);
            }
            sleep(PAGE_READY_POLL_INTERVAL).await;
        }
    }

    async fn main_frame(&mut self, session: &str) -> Result<Value, AutomationFailure> {
        self.send_command("Page.getFrameTree", json!({}), Some(session))
            .await?
            .get("frameTree")
            .and_then(|tree| tree.get("frame"))
            .cloned()
            .ok_or(AutomationFailure::Failed)
    }

    async fn snapshot(&mut self) -> Result<BrowserSnapshot, AutomationFailure> {
        let session = self.ensure_page_session().await?;
        let value = self.evaluate(SNAPSHOT_EXPRESSION, &session).await?;
        serde_json::from_value(value).map_err(|_| AutomationFailure::Failed)
    }

    async fn click(&mut self, selector: &str) -> Result<BrowserPage, AutomationFailure> {
        let session = self.ensure_page_session().await?;
        let expression = click_expression(selector)?;
        let value = self.evaluate(&expression, &session).await?;
        validate_action_result(&value)?;
        sleep(Duration::from_millis(100)).await;
        self.page_info_with_retry(&session, Duration::from_secs(5))
            .await
    }

    async fn fill(
        &mut self,
        selector: &str,
        value: &str,
    ) -> Result<BrowserPage, AutomationFailure> {
        let session = self.ensure_page_session().await?;
        let expression = fill_expression(selector, value)?;
        let result = self.evaluate(&expression, &session).await?;
        validate_action_result(&result)?;
        self.page_info(&session).await
    }

    async fn page_info(&mut self, session: &str) -> Result<BrowserPage, AutomationFailure> {
        let value = self.evaluate(PAGE_INFO_EXPRESSION, session).await?;
        serde_json::from_value(value).map_err(|_| AutomationFailure::Failed)
    }

    async fn page_info_with_retry(
        &mut self,
        session: &str,
        retry_timeout: Duration,
    ) -> Result<BrowserPage, AutomationFailure> {
        let deadline = Instant::now() + retry_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AutomationFailure::TimedOut);
            }
            let page = timeout(remaining, self.page_info(session))
                .await
                .map_err(|_| AutomationFailure::TimedOut)?;
            match page {
                Ok(page) => return Ok(page),
                Err(AutomationFailure::Failed) if Instant::now() < deadline => {
                    sleep(PAGE_READY_POLL_INTERVAL).await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn wait_for_page_ready(&mut self, session: &str) -> Result<(), AutomationFailure> {
        let deadline = Instant::now() + PAGE_READY_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AutomationFailure::TimedOut);
            }
            let state = timeout(
                remaining,
                self.evaluate("String(document.readyState)", session),
            )
            .await
            .map_err(|_| AutomationFailure::TimedOut)?;
            match state {
                Ok(state) if matches!(state.as_str(), Some("interactive" | "complete")) => {
                    return Ok(());
                }
                Ok(_) | Err(AutomationFailure::Failed) => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(AutomationFailure::TimedOut);
            }
            sleep(PAGE_READY_POLL_INTERVAL).await;
        }
    }

    async fn wait_download(
        &mut self,
        wait_timeout: Duration,
    ) -> Result<BrowserDownload, AutomationFailure> {
        if let Some(download) = self.completed_downloads.pop_front() {
            return Ok(download);
        }
        // A command-response boundary flushes download events that Chromium queued before this
        // request, including events produced by the click that initiated the download.
        self.send_command("Browser.getVersion", json!({}), None)
            .await?;
        if let Some(download) = self.completed_downloads.pop_front() {
            return Ok(download);
        }
        let deadline = Instant::now() + wait_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AutomationFailure::TimedOut);
            }
            let message = timeout(remaining, self.read_message())
                .await
                .map_err(|_| AutomationFailure::TimedOut)??;
            self.handle_event(&message);
            if let Some(download) = self.completed_downloads.pop_front() {
                return Ok(download);
            }
        }
    }

    async fn ensure_page_session(&mut self) -> Result<String, AutomationFailure> {
        if let Some(session) = &self.page_session_id {
            return Ok(session.clone());
        }
        let deadline = Instant::now() + CDP_COMMAND_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AutomationFailure::TargetNotFound);
            }
            let targets = timeout(
                remaining,
                self.send_command("Target.getTargets", json!({}), None),
            )
            .await
            .map_err(|_| AutomationFailure::TimedOut)??;
            let target_id = targets
                .get("targetInfos")
                .and_then(Value::as_array)
                .and_then(|targets| {
                    targets.iter().find_map(|target| {
                        (target.get("type").and_then(Value::as_str) == Some("page"))
                            .then(|| target.get("targetId").and_then(Value::as_str))
                            .flatten()
                    })
                });
            if let Some(target_id) = target_id {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(AutomationFailure::TimedOut);
                }
                let attached = timeout(
                    remaining,
                    self.send_command(
                        "Target.attachToTarget",
                        json!({"targetId": target_id, "flatten": true}),
                        None,
                    ),
                )
                .await
                .map_err(|_| AutomationFailure::TimedOut)??;
                let session = attached
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .ok_or(AutomationFailure::Failed)?
                    .to_owned();
                self.page_session_id = Some(session.clone());
                return Ok(session);
            }
            if Instant::now() >= deadline {
                return Err(AutomationFailure::TargetNotFound);
            }
            sleep(PAGE_READY_POLL_INTERVAL).await;
        }
    }

    async fn evaluate(
        &mut self,
        expression: &str,
        session: &str,
    ) -> Result<Value, AutomationFailure> {
        let result = self
            .send_command(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true,
                    "userGesture": true
                }),
                Some(session),
            )
            .await?;
        if result.get("exceptionDetails").is_some() {
            return Err(AutomationFailure::Failed);
        }
        result
            .get("result")
            .and_then(|result| result.get("value"))
            .cloned()
            .ok_or(AutomationFailure::Failed)
    }

    async fn send_command(
        &mut self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, AutomationFailure> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(AutomationFailure::Failed)?;
        let mut command = json!({"id": id, "method": method, "params": params});
        if let Some(session_id) = session_id {
            command["sessionId"] = Value::String(session_id.to_owned());
        }
        let mut encoded = serde_json::to_vec(&command).map_err(|_| AutomationFailure::Failed)?;
        encoded.push(0);
        self.writer
            .write_all(&encoded)
            .await
            .map_err(|_| AutomationFailure::Failed)?;

        let deadline = Instant::now() + CDP_COMMAND_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AutomationFailure::TimedOut);
            }
            let message = timeout(remaining, self.read_message())
                .await
                .map_err(|_| AutomationFailure::TimedOut)??;
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                if message.get("error").is_some() {
                    return Err(AutomationFailure::Failed);
                }
                return message
                    .get("result")
                    .cloned()
                    .ok_or(AutomationFailure::Failed);
            }
            self.handle_event(&message);
        }
    }

    async fn read_message(&mut self) -> Result<Value, AutomationFailure> {
        loop {
            let encoded = read_nul_terminated(
                &mut self.reader,
                &mut self.read_buffer,
                &mut self.discarding_message,
                CDP_MESSAGE_MAX_BYTES,
            )
            .await
            .map_err(|_| AutomationFailure::Failed)?;
            if let Some(encoded) = encoded {
                return serde_json::from_slice(&encoded).map_err(|_| AutomationFailure::Failed);
            }
        }
    }

    fn handle_event(&mut self, message: &Value) {
        let method = message.get("method").and_then(Value::as_str);
        match method {
            Some("Browser.downloadWillBegin") => {
                let Some(params) = message.get("params") else {
                    return;
                };
                let (Some(guid), Some(suggested_filename), Some(url)) = (
                    params.get("guid").and_then(Value::as_str),
                    params.get("suggestedFilename").and_then(Value::as_str),
                    params.get("url").and_then(Value::as_str),
                ) else {
                    return;
                };
                if guid.len() > DOWNLOAD_GUID_MAX_BYTES
                    || suggested_filename.len() > DOWNLOAD_FILENAME_MAX_BYTES
                    || url.len() > super::BROWSER_URL_MAX_BYTES
                    || (!self.pending_downloads.contains_key(guid)
                        && self.pending_downloads.len() >= DOWNLOAD_EVENT_QUEUE_MAX)
                {
                    return;
                }
                self.pending_downloads.insert(
                    guid.to_owned(),
                    PendingDownload {
                        suggested_filename: suggested_filename.to_owned(),
                        url: url.to_owned(),
                    },
                );
            }
            Some("Browser.downloadProgress") => {
                let Some(params) = message.get("params") else {
                    return;
                };
                let (Some(guid), Some(state)) = (
                    params.get("guid").and_then(Value::as_str),
                    params.get("state").and_then(Value::as_str),
                ) else {
                    return;
                };
                if state == "completed" {
                    if let Some(pending) = self.pending_downloads.remove(guid) {
                        let total_bytes = params.get("totalBytes").and_then(number_as_u64);
                        if self.completed_downloads.len() >= DOWNLOAD_EVENT_QUEUE_MAX {
                            self.completed_downloads.pop_front();
                        }
                        self.completed_downloads.push_back(BrowserDownload {
                            guid: guid.to_owned(),
                            suggested_filename: pending.suggested_filename,
                            url: pending.url,
                            total_bytes,
                        });
                    }
                } else if state == "canceled" {
                    self.pending_downloads.remove(guid);
                }
            }
            Some("Target.detachedFromTarget") => {
                let detached = message
                    .get("params")
                    .and_then(|params| params.get("sessionId"))
                    .and_then(Value::as_str);
                if detached == self.page_session_id.as_deref() {
                    self.page_session_id = None;
                }
            }
            _ => {}
        }
    }
}

fn navigation_is_committed(
    frame: &Value,
    frame_id: &str,
    loader_id: Option<&str>,
    expected_url: &str,
) -> bool {
    frame.get("id").and_then(Value::as_str) == Some(frame_id)
        && loader_id.map_or_else(
            || frame.get("url").and_then(Value::as_str) == Some(expected_url),
            |loader_id| frame.get("loaderId").and_then(Value::as_str) == Some(loader_id),
        )
}

fn navigation_has_fatal_error(navigation: &Value) -> bool {
    let Some(error) = navigation
        .get("errorText")
        .and_then(Value::as_str)
        .filter(|error| !error.is_empty())
    else {
        return false;
    };
    navigation.get("isDownload").and_then(Value::as_bool) != Some(true)
        || error != "net::ERR_ABORTED"
}

fn initial_navigation_was_accepted(navigation: &Value) -> bool {
    navigation
        .get("frameId")
        .and_then(Value::as_str)
        .is_some_and(|frame_id| !frame_id.is_empty())
}

async fn read_nul_terminated(
    reader: &mut BufReader<UnixStream>,
    partial: &mut Vec<u8>,
    discarding: &mut bool,
    max_bytes: usize,
) -> io::Result<Option<Vec<u8>>> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            partial.clear();
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "CDP pipe closed before a complete message",
            ));
        }
        let terminator = available.iter().position(|byte| *byte == 0);
        if *discarding {
            let consumed = terminator.map_or(available.len(), |position| position + 1);
            reader.consume(consumed);
            if terminator.is_some() {
                *discarding = false;
                return Ok(None);
            }
            continue;
        }
        let payload_bytes = terminator.unwrap_or(available.len());
        if partial.len().saturating_add(payload_bytes) > max_bytes {
            partial.clear();
            let consumed = terminator.map_or(available.len(), |position| position + 1);
            reader.consume(consumed);
            if terminator.is_none() {
                *discarding = true;
            }
            return Ok(None);
        }
        partial.extend_from_slice(&available[..payload_bytes]);
        let consumed = payload_bytes + usize::from(terminator.is_some());
        reader.consume(consumed);
        if terminator.is_some() {
            if partial.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "CDP message is empty",
                ));
            }
            return Ok(Some(std::mem::take(partial)));
        }
    }
}

fn number_as_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        let value = value.as_f64()?;
        (value.is_finite() && value >= 0.0 && value <= u64::MAX as f64).then_some(value as u64)
    })
}

fn validate_action_result(value: &Value) -> Result<(), AutomationFailure> {
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(());
    }
    match value.get("reason").and_then(Value::as_str) {
        Some("not_found") => Err(AutomationFailure::TargetNotFound),
        Some("invalid_selector") => Err(AutomationFailure::InvalidRequest),
        _ => Err(AutomationFailure::Failed),
    }
}

fn click_expression(selector: &str) -> Result<String, AutomationFailure> {
    let selector =
        serde_json::to_string(selector).map_err(|_| AutomationFailure::InvalidRequest)?;
    Ok(format!(
        r#"(selector => {{
  let element;
  try {{ element = document.querySelector(selector); }}
  catch (_) {{ return {{ ok: false, reason: 'invalid_selector' }}; }}
  if (!element) return {{ ok: false, reason: 'not_found' }};
  if (element.disabled || element.getAttribute('aria-disabled') === 'true') {{
    return {{ ok: false, reason: 'disabled' }};
  }}
  element.scrollIntoView({{ block: 'center', inline: 'center' }});
  element.focus({{ preventScroll: true }});
  element.click();
  return {{ ok: true }};
}})({selector})"#
    ))
}

fn fill_expression(selector: &str, value: &str) -> Result<String, AutomationFailure> {
    let selector =
        serde_json::to_string(selector).map_err(|_| AutomationFailure::InvalidRequest)?;
    let value = serde_json::to_string(value).map_err(|_| AutomationFailure::InvalidRequest)?;
    Ok(format!(
        r#"((selector, value) => {{
  let element;
  try {{ element = document.querySelector(selector); }}
  catch (_) {{ return {{ ok: false, reason: 'invalid_selector' }}; }}
  if (!element) return {{ ok: false, reason: 'not_found' }};
  if (element.disabled || element.readOnly || element.getAttribute('aria-disabled') === 'true') {{
    return {{ ok: false, reason: 'disabled' }};
  }}
  element.scrollIntoView({{ block: 'center', inline: 'center' }});
  element.focus({{ preventScroll: true }});
  if (element instanceof HTMLInputElement) {{
    if (element.type === 'file') return {{ ok: false, reason: 'unsupported' }};
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set;
    setter.call(element, value);
  }} else if (element instanceof HTMLTextAreaElement) {{
    const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set;
    setter.call(element, value);
  }} else if (element instanceof HTMLSelectElement) {{
    const setter = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value').set;
    setter.call(element, value);
  }} else if (element.isContentEditable) {{
    element.textContent = value;
  }} else {{
    return {{ ok: false, reason: 'unsupported' }};
  }}
  element.dispatchEvent(new Event('input', {{ bubbles: true }}));
  element.dispatchEvent(new Event('change', {{ bubbles: true }}));
  return {{ ok: true }};
}})({selector}, {value})"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn private_frames_are_length_bounded_and_round_trip() {
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        let payload = br#"{"action":"snapshot"}"#.to_vec();
        let expected = payload.clone();
        let write = tokio::spawn(async move { write_frame(&mut writer, &payload).await.unwrap() });
        assert_eq!(read_frame(&mut reader).await.unwrap(), expected);
        write.await.unwrap();
    }

    #[test]
    fn action_expressions_encode_untrusted_strings_as_json_values() {
        let click = click_expression("button[data-name='x\\\"y']").unwrap();
        assert!(click.contains(r#"button[data-name='x\\\"y']"#));
        let fill = fill_expression("#message", "line one\n\"line two\"").unwrap();
        assert!(fill.contains(r#"line one\n\"line two\""#));
    }

    #[test]
    fn action_failures_preserve_useful_public_error_categories() {
        assert_eq!(
            validate_action_result(&json!({"ok": false, "reason": "not_found"})),
            Err(AutomationFailure::TargetNotFound)
        );
        assert_eq!(
            validate_action_result(&json!({"ok": false, "reason": "invalid_selector"})),
            Err(AutomationFailure::InvalidRequest)
        );
    }

    #[test]
    fn private_response_timeouts_cover_each_actions_cdp_round_trips() {
        let navigate = serde_json::to_vec(&AutomationRequest::Navigate {
            url: "https://example.com/".to_owned(),
        })
        .unwrap();
        let fill = serde_json::to_vec(&AutomationRequest::Fill {
            selector: "#field".to_owned(),
            value: "value".to_owned(),
        })
        .unwrap();
        let download =
            serde_json::to_vec(&AutomationRequest::WaitDownload { timeout_ms: 30_000 }).unwrap();

        assert_eq!(
            request_timeout(&navigate),
            Some(
                PAGE_READY_TIMEOUT
                    .saturating_mul(2)
                    .saturating_add(CDP_COMMAND_TIMEOUT.saturating_mul(2))
            )
        );
        assert_eq!(
            request_timeout(&fill),
            Some(CDP_COMMAND_TIMEOUT.saturating_mul(2))
        );
        let snapshot = serde_json::to_vec(&AutomationRequest::Snapshot).unwrap();
        assert_eq!(request_timeout(&snapshot), Some(CDP_COMMAND_TIMEOUT));
        assert_eq!(request_timeout(&download), Some(Duration::from_secs(30)));
    }

    #[test]
    fn navigation_commit_is_bound_to_the_new_main_frame_loader() {
        let old_frame = json!({
            "id": "main-frame",
            "loaderId": "old-loader",
            "url": "https://old.example/"
        });
        assert!(!navigation_is_committed(
            &old_frame,
            "main-frame",
            Some("new-loader"),
            "https://new.example/"
        ));

        let committed = json!({
            "id": "main-frame",
            "loaderId": "new-loader",
            "url": "https://redirected.example/"
        });
        assert!(navigation_is_committed(
            &committed,
            "main-frame",
            Some("new-loader"),
            "https://new.example/"
        ));
        assert!(!navigation_is_committed(
            &committed,
            "other-frame",
            Some("new-loader"),
            "https://new.example/"
        ));
    }

    #[test]
    fn download_navigation_does_not_treat_chromiums_abort_as_a_failure() {
        assert!(!navigation_has_fatal_error(&json!({
            "frameId": "main-frame",
            "errorText": "net::ERR_ABORTED",
            "isDownload": true
        })));
        assert!(navigation_has_fatal_error(&json!({
            "frameId": "main-frame",
            "errorText": "net::ERR_NAME_NOT_RESOLVED",
            "isDownload": true
        })));
        assert!(navigation_has_fatal_error(&json!({
            "frameId": "main-frame",
            "errorText": "net::ERR_NAME_NOT_RESOLVED"
        })));
    }

    #[test]
    fn initial_navigation_keeps_chromiums_network_error_page_running() {
        assert!(initial_navigation_was_accepted(&json!({
            "frameId": "main-frame",
            "errorText": "net::ERR_NAME_NOT_RESOLVED"
        })));
        assert!(!initial_navigation_was_accepted(&json!({
            "errorText": "net::ERR_NAME_NOT_RESOLVED"
        })));
    }

    #[test]
    fn same_document_navigation_commit_requires_the_expected_url() {
        let frame = json!({
            "id": "main-frame",
            "loaderId": "unchanged-loader",
            "url": "https://example.com/page#new"
        });
        assert!(navigation_is_committed(
            &frame,
            "main-frame",
            None,
            "https://example.com/page#new"
        ));
        assert!(!navigation_is_committed(
            &frame,
            "main-frame",
            None,
            "https://example.com/page#old"
        ));
    }

    #[test]
    fn bounded_worst_case_snapshot_fits_the_private_frame() {
        let emoji = "\u{1f642}";
        let element = BrowserSnapshotElement {
            selector: emoji.repeat(480),
            tag: emoji.repeat(80),
            role: emoji.repeat(80),
            text: emoji.repeat(160),
            aria_label: emoji.repeat(160),
            placeholder: emoji.repeat(160),
            input_type: emoji.repeat(80),
            disabled: false,
        };
        let response = AutomationResponse::Ok(AutomationResult::Snapshot(BrowserSnapshot {
            url: emoji.repeat(2048),
            title: emoji.repeat(1024),
            text: emoji.repeat(16_000),
            text_truncated: true,
            elements: vec![element; 128],
            elements_truncated: true,
        }));

        let encoded = serde_json::to_vec(&response).unwrap();
        assert!(encoded.len() < AUTOMATION_FRAME_MAX_BYTES);
        assert!(matches!(
            serde_json::from_slice::<AutomationResponse>(&encoded).unwrap(),
            AutomationResponse::Ok(AutomationResult::Snapshot(_))
        ));
    }

    #[test]
    fn oversized_automation_response_becomes_a_structured_failure() {
        let response = AutomationResponse::Ok(AutomationResult::Snapshot(BrowserSnapshot {
            url: String::new(),
            title: String::new(),
            text: "\u{1f642}".repeat(AUTOMATION_FRAME_MAX_BYTES),
            text_truncated: false,
            elements: Vec::new(),
            elements_truncated: false,
        }));

        let encoded = encode_automation_response(&response).unwrap();
        assert!(encoded.len() < AUTOMATION_FRAME_MAX_BYTES);
        assert!(matches!(
            serde_json::from_slice::<AutomationResponse>(&encoded).unwrap(),
            AutomationResponse::Error(AutomationFailure::Failed)
        ));
    }

    #[tokio::test]
    async fn download_events_are_queued_until_wait_download_consumes_them() {
        let (writer, _command_peer) = UnixStream::pair().unwrap();
        let (response_reader, _response_peer) = UnixStream::pair().unwrap();
        let mut cdp = CdpConnection {
            reader: BufReader::new(response_reader),
            writer,
            next_id: 1,
            page_session_id: None,
            read_buffer: Vec::new(),
            discarding_message: false,
            pending_downloads: HashMap::new(),
            completed_downloads: VecDeque::new(),
        };
        cdp.handle_event(&json!({
            "method": "Browser.downloadWillBegin",
            "params": {
                "guid": "download-guid",
                "suggestedFilename": "report.zip",
                "url": "https://example.com/report.zip"
            }
        }));
        cdp.handle_event(&json!({
            "method": "Browser.downloadProgress",
            "params": {
                "guid": "download-guid",
                "state": "completed",
                "totalBytes": 4096.0
            }
        }));

        assert_eq!(
            cdp.completed_downloads.pop_front(),
            Some(BrowserDownload {
                guid: "download-guid".to_owned(),
                suggested_filename: "report.zip".to_owned(),
                url: "https://example.com/report.zip".to_owned(),
                total_bytes: Some(4096),
            })
        );
        assert!(cdp.pending_downloads.is_empty());
    }

    #[tokio::test]
    async fn cdp_messages_are_bounded_while_reading() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(reader);
        let write = tokio::spawn(async move {
            writer.write_all(b"123456789\0{}\0").await.unwrap();
        });
        let mut partial = Vec::new();
        let mut discarding = false;
        assert_eq!(
            read_nul_terminated(&mut reader, &mut partial, &mut discarding, 8)
                .await
                .unwrap(),
            None
        );
        assert!(partial.is_empty());
        assert!(!discarding);
        assert_eq!(
            read_nul_terminated(&mut reader, &mut partial, &mut discarding, 8)
                .await
                .unwrap(),
            Some(b"{}".to_vec())
        );
        write.await.unwrap();
    }

    #[tokio::test]
    async fn automation_server_drains_cdp_events_without_a_client_request() {
        let root = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(root.path().join("cdp.sock")).unwrap();
        let (writer, _command_peer) = UnixStream::pair().unwrap();
        let (response_reader, response_peer) = UnixStream::pair().unwrap();
        let cdp = CdpConnection {
            reader: BufReader::new(response_reader),
            writer,
            next_id: 1,
            page_session_id: None,
            read_buffer: Vec::new(),
            discarding_message: false,
            pending_downloads: HashMap::new(),
            completed_downloads: VecDeque::new(),
        };
        let server = tokio::spawn(serve(listener, cdp));
        drop(response_peer);

        let result = timeout(Duration::from_secs(1), server)
            .await
            .expect("server should observe the idle CDP pipe closing")
            .unwrap();
        assert_eq!(result, Err(BrowserError::AutomationFailed));
    }

    #[tokio::test]
    async fn long_action_rejects_concurrent_requests_as_busy() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("cdp.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (writer, command_peer) = UnixStream::pair().unwrap();
        let (response_reader, mut response_peer) = UnixStream::pair().unwrap();
        let mut cdp = CdpConnection {
            reader: BufReader::new(response_reader),
            writer,
            next_id: 1,
            page_session_id: None,
            read_buffer: Vec::new(),
            discarding_message: false,
            pending_downloads: HashMap::new(),
            completed_downloads: VecDeque::new(),
        };
        let (command_seen, wait_for_command) = tokio::sync::oneshot::channel();
        let browser = tokio::spawn(async move {
            let mut command_peer = BufReader::new(command_peer);
            let mut command = Vec::new();
            command_peer.read_until(0, &mut command).await.unwrap();
            command.pop();
            let command: Value = serde_json::from_slice(&command).unwrap();
            let id = command["id"].as_u64().unwrap();
            let mut response = serde_json::to_vec(&json!({"id": id, "result": {}})).unwrap();
            response.push(0);
            response_peer.write_all(&response).await.unwrap();
            let _ = command_seen.send(());
            sleep(Duration::from_secs(1)).await;
        });

        let busy_client = async {
            wait_for_command.await.unwrap();
            request(&socket, AutomationRequest::Snapshot).await
        };
        let (active, busy) = tokio::join!(
            execute_while_rejecting_busy(
                &listener,
                &mut cdp,
                AutomationRequest::WaitDownload { timeout_ms: 200 },
            ),
            busy_client,
        );
        assert!(matches!(
            active.unwrap(),
            AutomationResponse::Error(AutomationFailure::TimedOut)
        ));
        assert_eq!(busy, Err(BrowserError::AutomationBusy));
        browser.abort();
    }

    #[tokio::test]
    async fn disconnected_queued_peer_is_not_eligible_for_execution() {
        let (server, client) = UnixStream::pair().unwrap();
        assert!(request_peer_is_open(&server));
        drop(client);
        server.readable().await.unwrap();
        assert!(!request_peer_is_open(&server));
    }
}
