use std::env;
use std::ffi::{CStr, CString, OsString};
use std::fs;
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use tokio::process::{Child, Command};
use tokio::signal::unix::{Signal, SignalKind};
use tokio::time::{Instant, sleep, timeout};

use super::BrowserError;
use super::automation::{ChromiumCdpPipes, bind_automation_socket};
use super::download::BROWSER_DOWNLOAD_FILE_MAX_BYTES;

const XVNC_START_TIMEOUT: Duration = Duration::from_secs(20);
const START_INTENT_TIMEOUT: Duration = Duration::from_secs(25);
const CHILD_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const CHROMIUM_START_SETTLE_TIME: Duration = Duration::from_millis(500);
const CHROMIUM_ACCOUNT: &CStr = c"nobody";
const PASSWD_BUFFER_FALLBACK_BYTES: usize = 16 * 1024;
const PASSWD_BUFFER_MAX_BYTES: usize = 1024 * 1024;
const XAUTH_COOKIE_BYTES: usize = 16;
const XAUTH_FAMILY_WILD: u16 = u16::MAX;
const XAUTH_PROTOCOL: &[u8] = b"MIT-MAGIC-COOKIE-1";
const BROWSER_SESSION_ENV: &str = "TERMD_BROWSER_SESSION_ID";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChromiumIdentity {
    uid: u32,
    gid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserSupervisorArgs {
    pub session_id: uuid::Uuid,
    pub display: u16,
    pub width: u16,
    pub height: u16,
    pub url: String,
    pub xvnc: PathBuf,
    pub openbox: PathBuf,
    pub chromium: PathBuf,
    pub rfb_socket: PathBuf,
    pub automation_socket: PathBuf,
    pub ready_file: PathBuf,
    pub start_file: PathBuf,
    pub profile_dir: PathBuf,
    pub download_dir: PathBuf,
    pub openbox_config: PathBuf,
    pub runtime_library_path: Option<PathBuf>,
    pub xkb_root: Option<PathBuf>,
    pub runtime_data_root: Option<PathBuf>,
    pub runtime_bin_root: Option<PathBuf>,
}

pub async fn run_browser_supervisor(args: BrowserSupervisorArgs) -> Result<(), BrowserError> {
    validate_args(&args)?;
    let mut terminate = tokio::signal::unix::signal(SignalKind::terminate())
        .map_err(|_| BrowserError::SupervisorStartFailed)?;
    let mut interrupt = tokio::signal::unix::signal(SignalKind::interrupt())
        .map_err(|_| BrowserError::SupervisorStartFailed)?;
    wait_for_start_intent(&args, &mut terminate, &mut interrupt).await?;
    consume_start_file(&args.start_file)?;
    let chromium_identity = resolve_chromium_identity()?;
    prepare_paths(&args, chromium_identity)?;

    let display = format!(":{}", args.display);
    let geometry = format!("{}x{}", args.width, args.height);
    let xauthority = args.profile_dir.join(".Xauthority");
    let mut xvnc_command = Command::new(&args.xvnc);
    xvnc_command.args([
        display.as_str(),
        "-geometry",
        geometry.as_str(),
        "-depth",
        "24",
        "-rfbport",
        "-1",
        "-rfbunixpath",
    ]);
    xvnc_command
        .arg(&args.rfb_socket)
        .args([
            "-rfbunixmode",
            "0600",
            "-SecurityTypes",
            "None",
            "-AlwaysShared=1",
            "-DisconnectClients=0",
            "-localhost",
            "-nolisten",
            "tcp",
            "-auth",
        ])
        .arg(&xauthority);
    if let Some(xkb_root) = &args.xkb_root {
        xvnc_command.arg("-xkbdir").arg(xkb_root);
    }
    configure_runtime_child(&mut xvnc_command, &args, &display, &xauthority);
    let mut xvnc = xvnc_command
        .spawn()
        .map_err(|_| BrowserError::SupervisorStartFailed)?;
    if let Err(error) = wait_for_rfb_socket(&mut xvnc, &args.rfb_socket).await {
        shutdown_child(&mut xvnc).await;
        return Err(error);
    }

    let mut openbox_command = Command::new(&args.openbox);
    openbox_command
        .arg("--config-file")
        .arg(&args.openbox_config);
    configure_runtime_child(&mut openbox_command, &args, &display, &xauthority);
    configure_openbox_data_path(&mut openbox_command, &args);
    let mut openbox = match openbox_command.spawn() {
        Ok(child) => child,
        Err(_) => {
            shutdown_child(&mut xvnc).await;
            return Err(BrowserError::SupervisorStartFailed);
        }
    };
    sleep(Duration::from_millis(150)).await;
    if openbox
        .try_wait()
        .map_err(|_| BrowserError::SupervisorStartFailed)?
        .is_some()
    {
        shutdown_child(&mut xvnc).await;
        return Err(BrowserError::SupervisorStartFailed);
    }

    let cdp_pipes = match ChromiumCdpPipes::new() {
        Ok(pipes) => pipes,
        Err(error) => {
            shutdown_child(&mut openbox).await;
            shutdown_child(&mut xvnc).await;
            return Err(error);
        }
    };
    let mut chromium_command = Command::new(&args.chromium);
    chromium_command
        .arg(format!("--user-data-dir={}", args.profile_dir.display()))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-session-crashed-bubble",
            "--disable-gpu",
            "--ozone-platform=x11",
            "--start-maximized",
            "--remote-debugging-pipe",
        ])
        .arg(format!("--window-size={},{}", args.width, args.height))
        .arg("--new-window")
        .arg("about:blank");
    if let Err(error) = configure_chromium_child(
        &mut chromium_command,
        &args,
        &display,
        &xauthority,
        chromium_identity,
    ) {
        shutdown_child(&mut openbox).await;
        shutdown_child(&mut xvnc).await;
        return Err(error);
    }
    cdp_pipes.configure_child(&mut chromium_command);
    let mut chromium = match chromium_command.spawn() {
        Ok(child) => child,
        Err(_) => {
            shutdown_child(&mut openbox).await;
            shutdown_child(&mut xvnc).await;
            return Err(BrowserError::SupervisorStartFailed);
        }
    };
    sleep(CHROMIUM_START_SETTLE_TIME).await;
    if chromium
        .try_wait()
        .map_err(|_| BrowserError::SupervisorStartFailed)?
        .is_some()
    {
        shutdown_child(&mut openbox).await;
        shutdown_child(&mut xvnc).await;
        return Err(BrowserError::SupervisorStartFailed);
    }
    let mut cdp = cdp_pipes.into_connection();
    if cdp.initialize(&args.download_dir, &args.url).await.is_err() {
        shutdown_child(&mut chromium).await;
        shutdown_child(&mut openbox).await;
        shutdown_child(&mut xvnc).await;
        remove_owned_socket(&args.rfb_socket);
        return Err(BrowserError::SupervisorStartFailed);
    }
    let automation_listener = match bind_automation_socket(&args.automation_socket) {
        Ok(listener) => listener,
        Err(error) => {
            shutdown_child(&mut chromium).await;
            shutdown_child(&mut openbox).await;
            shutdown_child(&mut xvnc).await;
            remove_owned_socket(&args.rfb_socket);
            remove_owned_socket(&args.automation_socket);
            return Err(error);
        }
    };
    if let Err(error) = write_ready_file(&args.ready_file, args.session_id) {
        shutdown_child(&mut chromium).await;
        shutdown_child(&mut openbox).await;
        shutdown_child(&mut xvnc).await;
        remove_owned_socket(&args.rfb_socket);
        remove_owned_socket(&args.automation_socket);
        return Err(error);
    }

    let automation = super::automation::serve(automation_listener, cdp);
    tokio::pin!(automation);
    tokio::select! {
        _ = xvnc.wait() => {}
        _ = openbox.wait() => {}
        _ = chromium.wait() => {}
        _ = &mut automation => {}
        _ = terminate.recv() => {}
        _ = interrupt.recv() => {}
    }

    shutdown_child(&mut chromium).await;
    shutdown_child(&mut openbox).await;
    shutdown_child(&mut xvnc).await;
    remove_owned_regular_file(&args.ready_file);
    remove_owned_socket(&args.rfb_socket);
    remove_owned_socket(&args.automation_socket);
    Ok(())
}

fn validate_args(args: &BrowserSupervisorArgs) -> Result<(), BrowserError> {
    let expected_profile_name = args.session_id.to_string();
    if !(100..=999).contains(&args.display)
        || !(640..=3840).contains(&args.width)
        || !(480..=2160).contains(&args.height)
        || !args.xvnc.is_file()
        || !args.openbox.is_file()
        || !args.chromium.is_file()
        || args.rfb_socket.file_name().and_then(|name| name.to_str()) != Some("rfb.sock")
        || args
            .automation_socket
            .file_name()
            .and_then(|name| name.to_str())
            != Some("cdp.sock")
        || args.ready_file.file_name().and_then(|name| name.to_str()) != Some("ready")
        || args.start_file.file_name().and_then(|name| name.to_str()) != Some("start")
        || args.ready_file.parent() != args.rfb_socket.parent()
        || args.start_file.parent() != args.rfb_socket.parent()
        || args.automation_socket.parent() != args.rfb_socket.parent()
        || args.profile_dir.file_name().and_then(|name| name.to_str())
            != Some(expected_profile_name.as_str())
        || args.download_dir.file_name().and_then(|name| name.to_str())
            != Some(expected_profile_name.as_str())
        || args.download_dir == args.profile_dir
        || args
            .runtime_data_root
            .as_ref()
            .is_some_and(|path| !path.is_dir())
        || args
            .runtime_bin_root
            .as_ref()
            .is_some_and(|path| !path.is_dir())
    {
        return Err(BrowserError::SupervisorArgumentsInvalid);
    }
    Ok(())
}

async fn wait_for_start_intent(
    args: &BrowserSupervisorArgs,
    terminate: &mut Signal,
    interrupt: &mut Signal,
) -> Result<(), BrowserError> {
    let deadline = Instant::now() + START_INTENT_TIMEOUT;
    loop {
        if start_file_matches(&args.start_file, args.session_id) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(BrowserError::SupervisorStartTimeout);
        }
        tokio::select! {
            _ = sleep(Duration::from_millis(50)) => {}
            Some(_) = terminate.recv() => return Err(BrowserError::SupervisorStartFailed),
            Some(_) = interrupt.recv() => return Err(BrowserError::SupervisorStartFailed),
        }
    }
}

fn start_file_matches(path: &Path, session_id: uuid::Uuid) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return false;
    }
    fs::read_to_string(path)
        .map(|contents| contents == session_id.to_string())
        .unwrap_or(false)
}

fn consume_start_file(path: &Path) -> Result<(), BrowserError> {
    fs::remove_file(path).map_err(|_| BrowserError::SupervisorStartFailed)
}

fn prepare_paths(
    args: &BrowserSupervisorArgs,
    chromium_identity: Option<ChromiumIdentity>,
) -> Result<(), BrowserError> {
    if let Some(parent) = args.rfb_socket.parent() {
        fs::create_dir_all(parent).map_err(|_| BrowserError::SupervisorStartFailed)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|_| BrowserError::SupervisorStartFailed)?;
    }
    fs::create_dir_all(&args.profile_dir).map_err(|_| BrowserError::SupervisorStartFailed)?;
    let profile_metadata =
        fs::symlink_metadata(&args.profile_dir).map_err(|_| BrowserError::SupervisorStartFailed)?;
    if !profile_metadata.is_dir() || profile_metadata.file_type().is_symlink() {
        return Err(BrowserError::SupervisorStartFailed);
    }
    fs::set_permissions(&args.profile_dir, fs::Permissions::from_mode(0o700))
        .map_err(|_| BrowserError::SupervisorStartFailed)?;
    prepare_download_dir(&args.download_dir, chromium_identity)?;
    write_chromium_preferences(&args.profile_dir, &args.download_dir, chromium_identity)?;
    let xauthority = args.profile_dir.join(".Xauthority");
    write_xauthority_file(&xauthority, chromium_identity)?;
    if let Some(identity) = chromium_identity {
        chown_profile(&args.profile_dir, identity)?;
    }
    remove_owned_regular_file(&args.ready_file);
    remove_owned_socket(&args.rfb_socket);
    remove_owned_socket(&args.automation_socket);
    Ok(())
}

fn prepare_download_dir(
    path: &Path,
    identity: Option<ChromiumIdentity>,
) -> Result<(), BrowserError> {
    fs::create_dir_all(path).map_err(|_| BrowserError::SupervisorStartFailed)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| BrowserError::SupervisorStartFailed)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(BrowserError::SupervisorStartFailed);
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|_| BrowserError::SupervisorStartFailed)?;
    if let Some(identity) = identity {
        chown_path(path, identity)?;
    }
    Ok(())
}

fn write_chromium_preferences(
    profile_dir: &Path,
    download_dir: &Path,
    identity: Option<ChromiumIdentity>,
) -> Result<(), BrowserError> {
    let default_dir = profile_dir.join("Default");
    fs::create_dir(&default_dir).map_err(|_| BrowserError::SupervisorStartFailed)?;
    let metadata =
        fs::symlink_metadata(&default_dir).map_err(|_| BrowserError::SupervisorStartFailed)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(BrowserError::SupervisorStartFailed);
    }
    fs::set_permissions(&default_dir, fs::Permissions::from_mode(0o700))
        .map_err(|_| BrowserError::SupervisorStartFailed)?;

    let download_dir = download_dir
        .to_str()
        .ok_or(BrowserError::SupervisorStartFailed)?;
    let preferences = serde_json::to_vec(&serde_json::json!({
        "download": {
            "default_directory": download_dir,
            "directory_upgrade": true,
            "prompt_for_download": false
        }
    }))
    .map_err(|_| BrowserError::SupervisorStartFailed)?;
    let preferences_path = default_dir.join("Preferences");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&preferences_path)
        .map_err(|_| BrowserError::SupervisorStartFailed)?;
    file.write_all(&preferences)
        .and_then(|_| file.sync_all())
        .map_err(|_| BrowserError::SupervisorStartFailed)?;
    drop(file);

    if let Some(identity) = identity {
        chown_path(&preferences_path, identity)?;
        chown_path(&default_dir, identity)?;
    }
    Ok(())
}

fn write_ready_file(path: &Path, session_id: uuid::Uuid) -> Result<(), BrowserError> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| BrowserError::SupervisorStartFailed)?;
    file.write_all(session_id.to_string().as_bytes())
        .and_then(|_| file.sync_all())
        .map_err(|_| BrowserError::SupervisorStartFailed)
}

fn configure_common_child(
    command: &mut Command,
    args: &BrowserSupervisorArgs,
    display: &str,
    xauthority: &Path,
) {
    command
        .env("DISPLAY", display)
        .env("XAUTHORITY", xauthority)
        .env(BROWSER_SESSION_ENV, args.session_id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(false);
    configure_parent_death_signal(command);
}

fn configure_parent_death_signal(command: &mut Command) {
    let expected_parent = unsafe { libc::getpid() };
    // SAFETY: the closure only invokes async-signal-safe libc calls between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != expected_parent {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            Ok(())
        });
    }
}

fn configure_runtime_child(
    command: &mut Command,
    args: &BrowserSupervisorArgs,
    display: &str,
    xauthority: &Path,
) {
    configure_common_child(command, args, display, xauthority);
    if let Some(library_path) = &args.runtime_library_path {
        let combined = match std::env::var_os("LD_LIBRARY_PATH") {
            Some(existing) if !existing.is_empty() => {
                let mut paths = vec![library_path.clone()];
                paths.extend(std::env::split_paths(&existing));
                std::env::join_paths(paths).ok()
            }
            _ => Some(library_path.clone().into_os_string()),
        };
        if let Some(combined) = combined {
            command.env("LD_LIBRARY_PATH", combined);
        }
    }
    if let Some(xkb_root) = &args.xkb_root {
        command.env("XKB_CONFIG_ROOT", xkb_root);
    }
    if let Some(runtime_bin_root) = &args.runtime_bin_root {
        let mut paths = vec![runtime_bin_root.clone()];
        if let Some(existing) = env::var_os("PATH") {
            paths.extend(env::split_paths(&existing));
        }
        if let Ok(combined) = env::join_paths(paths) {
            command.env("PATH", combined);
        }
    }
}

fn configure_chromium_child(
    command: &mut Command,
    args: &BrowserSupervisorArgs,
    display: &str,
    xauthority: &Path,
    identity: Option<ChromiumIdentity>,
) -> Result<(), BrowserError> {
    configure_common_child(command, args, display, xauthority);
    let file_size_limit = chromium_file_size_limit()?;
    // SAFETY: these libc calls allocate no memory and the captured value owns no heap allocation.
    unsafe {
        command.pre_exec(move || {
            let mut current = MaybeUninit::<libc::rlimit>::zeroed();
            if libc::getrlimit(libc::RLIMIT_FSIZE, current.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            let current = current.assume_init();
            let capped = cap_file_size_limit(current, file_size_limit);
            if libc::setrlimit(libc::RLIMIT_FSIZE, &capped) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("XKB_CONFIG_ROOT")
        .env("HOME", &args.profile_dir)
        .env("XDG_CONFIG_HOME", args.profile_dir.join("config"))
        .env("XDG_CACHE_HOME", args.profile_dir.join("cache"));
    if let Some(identity) = identity {
        // std::process clears supplementary groups before applying uid/gid.
        command.gid(identity.gid).uid(identity.uid);
    }
    Ok(())
}

fn chromium_file_size_limit() -> Result<libc::rlimit, BrowserError> {
    let bytes = libc::rlim_t::try_from(BROWSER_DOWNLOAD_FILE_MAX_BYTES)
        .map_err(|_| BrowserError::SupervisorStartFailed)?;
    Ok(libc::rlimit {
        rlim_cur: bytes,
        rlim_max: bytes,
    })
}

fn cap_file_size_limit(current: libc::rlimit, configured: libc::rlimit) -> libc::rlimit {
    libc::rlimit {
        rlim_cur: current.rlim_cur.min(configured.rlim_cur),
        rlim_max: current.rlim_max.min(configured.rlim_max),
    }
}

fn resolve_chromium_identity() -> Result<Option<ChromiumIdentity>, BrowserError> {
    // Chromium refuses uid 0 when its sandbox is enabled. Non-root daemon
    // deployments retain their existing identity.
    if unsafe { libc::geteuid() } != 0 {
        return Ok(None);
    }

    let configured_size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut buffer_size = if configured_size > 0 {
        usize::try_from(configured_size).unwrap_or(PASSWD_BUFFER_FALLBACK_BYTES)
    } else {
        PASSWD_BUFFER_FALLBACK_BYTES
    }
    .clamp(1024, PASSWD_BUFFER_MAX_BYTES);

    loop {
        let mut entry = MaybeUninit::<libc::passwd>::zeroed();
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; buffer_size];
        let status = unsafe {
            libc::getpwnam_r(
                CHROMIUM_ACCOUNT.as_ptr(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && buffer_size < PASSWD_BUFFER_MAX_BYTES {
            buffer_size = (buffer_size * 2).min(PASSWD_BUFFER_MAX_BYTES);
            continue;
        }
        if status != 0 || result.is_null() {
            return Err(BrowserError::SupervisorStartFailed);
        }
        let entry = unsafe { entry.assume_init() };
        if entry.pw_uid == 0 || entry.pw_gid == 0 {
            return Err(BrowserError::SupervisorStartFailed);
        }
        return Ok(Some(ChromiumIdentity {
            uid: entry.pw_uid,
            gid: entry.pw_gid,
        }));
    }
}

fn chown_profile(path: &Path, identity: ChromiumIdentity) -> Result<(), BrowserError> {
    chown_path(path, identity)
}

fn chown_path(path: &Path, identity: ChromiumIdentity) -> Result<(), BrowserError> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| BrowserError::SupervisorStartFailed)?;
    if unsafe { libc::chown(path.as_ptr(), identity.uid, identity.gid) } != 0 {
        return Err(BrowserError::SupervisorStartFailed);
    }
    let metadata = fs::symlink_metadata(Path::new(std::ffi::OsStr::from_bytes(path.as_bytes())))
        .map_err(|_| BrowserError::SupervisorStartFailed)?;
    if metadata.uid() != identity.uid || metadata.gid() != identity.gid {
        return Err(BrowserError::SupervisorStartFailed);
    }
    Ok(())
}

fn write_xauthority_file(
    path: &Path,
    identity: Option<ChromiumIdentity>,
) -> Result<(), BrowserError> {
    remove_owned_regular_file(path);
    let mut cookie = [0_u8; XAUTH_COOKIE_BYTES];
    OsRng.fill_bytes(&mut cookie);

    let mut record = Vec::with_capacity(2 + 2 + 2 + 2 + XAUTH_PROTOCOL.len() + 2 + cookie.len());
    record.extend_from_slice(&XAUTH_FAMILY_WILD.to_be_bytes());
    write_xauthority_field(&mut record, &[]);
    write_xauthority_field(&mut record, &[]);
    write_xauthority_field(&mut record, XAUTH_PROTOCOL);
    write_xauthority_field(&mut record, &cookie);

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| BrowserError::SupervisorStartFailed)?;
    file.write_all(&record)
        .and_then(|_| file.sync_all())
        .map_err(|_| BrowserError::SupervisorStartFailed)?;
    drop(file);
    if let Some(identity) = identity {
        chown_path(path, identity)?;
    }
    Ok(())
}

fn write_xauthority_field(record: &mut Vec<u8>, value: &[u8]) {
    record.extend_from_slice(&(value.len() as u16).to_be_bytes());
    record.extend_from_slice(value);
}

fn configure_openbox_data_path(command: &mut Command, args: &BrowserSupervisorArgs) {
    let Some(runtime_data_root) = &args.runtime_data_root else {
        return;
    };
    let existing = env::var_os("XDG_DATA_DIRS")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| OsString::from("/usr/local/share:/usr/share"));
    let mut paths = vec![runtime_data_root.clone()];
    paths.extend(env::split_paths(&existing));
    if let Ok(combined) = env::join_paths(paths) {
        command.env("XDG_DATA_DIRS", combined);
    }
}

async fn wait_for_rfb_socket(child: &mut Child, socket: &Path) -> Result<(), BrowserError> {
    let deadline = Instant::now() + XVNC_START_TIMEOUT;
    loop {
        if let Ok(metadata) = fs::symlink_metadata(socket)
            && metadata.file_type().is_socket()
        {
            return Ok(());
        }
        if child
            .try_wait()
            .map_err(|_| BrowserError::SupervisorStartFailed)?
            .is_some()
        {
            return Err(BrowserError::SupervisorStartFailed);
        }
        if Instant::now() >= deadline {
            return Err(BrowserError::SupervisorStartTimeout);
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn shutdown_child(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let _ = child.start_kill();
    let _ = timeout(CHILD_SHUTDOWN_TIMEOUT, child.wait()).await;
}

fn remove_owned_socket(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.file_type().is_socket() || metadata.file_type().is_symlink() {
        let _ = fs::remove_file(path);
    }
}

fn remove_owned_regular_file(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_file() || metadata.file_type().is_symlink() {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn supervisor_args(root: &Path) -> BrowserSupervisorArgs {
        let session_id = uuid::Uuid::new_v4();
        let run_dir = root.join("run");
        let xvnc = root.join("Xvnc");
        let openbox = root.join("openbox");
        let chromium = root.join("chromium");
        for executable in [&xvnc, &openbox, &chromium] {
            fs::write(executable, []).unwrap();
        }
        BrowserSupervisorArgs {
            session_id,
            display: 100,
            width: 1280,
            height: 800,
            url: "https://example.com/".to_owned(),
            xvnc,
            openbox,
            chromium,
            rfb_socket: run_dir.join("rfb.sock"),
            automation_socket: run_dir.join("cdp.sock"),
            ready_file: run_dir.join("ready"),
            start_file: run_dir.join("start"),
            profile_dir: root.join("profiles").join(session_id.to_string()),
            download_dir: root.join("downloads").join(session_id.to_string()),
            openbox_config: root.join("openbox.xml"),
            runtime_library_path: None,
            xkb_root: None,
            runtime_data_root: None,
            runtime_bin_root: None,
        }
    }

    #[test]
    fn chromium_identity_never_keeps_root_uid_or_gid() {
        let identity = resolve_chromium_identity().unwrap();
        if unsafe { libc::geteuid() } == 0 {
            assert!(identity.is_some_and(|identity| identity.uid != 0 && identity.gid != 0));
        } else {
            assert_eq!(identity, None);
        }
    }

    #[test]
    fn xauthority_cookie_is_random_and_private() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first.Xauthority");
        let second = root.path().join("second.Xauthority");
        write_xauthority_file(&first, None).unwrap();
        write_xauthority_file(&second, None).unwrap();

        let first_bytes = fs::read(&first).unwrap();
        let second_bytes = fs::read(&second).unwrap();
        assert_eq!(
            first.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            first_bytes
                .windows(XAUTH_PROTOCOL.len())
                .any(|part| part == XAUTH_PROTOCOL)
        );
        assert_eq!(first_bytes.len(), second_bytes.len());
        assert_ne!(first_bytes, second_bytes);
    }

    #[test]
    fn chromium_preferences_use_the_persistent_download_directory() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        let downloads = root.path().join("downloads");
        fs::create_dir(&profile).unwrap();
        prepare_download_dir(&downloads, None).unwrap();
        write_chromium_preferences(&profile, &downloads, None).unwrap();

        let preferences_path = profile.join("Default/Preferences");
        let preferences: serde_json::Value =
            serde_json::from_slice(&fs::read(&preferences_path).unwrap()).unwrap();
        assert_eq!(
            preferences["download"]["default_directory"],
            downloads.to_string_lossy().as_ref()
        );
        assert_eq!(preferences["download"]["prompt_for_download"], false);
        assert!(preferences.get("profile").is_none());
        assert_eq!(
            preferences_path.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn chromium_file_size_limit_matches_download_policy() {
        let limit = chromium_file_size_limit().unwrap();
        assert_eq!(limit.rlim_cur, BROWSER_DOWNLOAD_FILE_MAX_BYTES);
        assert_eq!(limit.rlim_max, BROWSER_DOWNLOAD_FILE_MAX_BYTES);
    }

    #[test]
    fn chromium_file_size_limit_preserves_lower_inherited_limits() {
        let configured = chromium_file_size_limit().unwrap();
        let inherited = libc::rlimit {
            rlim_cur: 1024,
            rlim_max: 2048,
        };
        let capped = cap_file_size_limit(inherited, configured);
        assert_eq!(capped.rlim_cur, inherited.rlim_cur);
        assert_eq!(capped.rlim_max, inherited.rlim_max);
    }

    #[test]
    fn start_file_must_be_private_owned_and_session_bound() {
        let root = tempfile::tempdir().unwrap();
        let start_file = root.path().join("start");
        let session_id = uuid::Uuid::new_v4();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&start_file)
            .unwrap();
        file.write_all(session_id.to_string().as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);

        assert!(start_file_matches(&start_file, session_id));
        assert!(!start_file_matches(&start_file, uuid::Uuid::new_v4()));
        fs::set_permissions(&start_file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!start_file_matches(&start_file, session_id));
    }

    #[test]
    fn supervisor_control_files_must_share_the_socket_directory() {
        let root = tempfile::tempdir().unwrap();
        let args = supervisor_args(root.path());
        assert_eq!(validate_args(&args), Ok(()));

        let mut misplaced = args;
        misplaced.start_file = root.path().join("other").join("start");
        assert_eq!(
            validate_args(&misplaced),
            Err(BrowserError::SupervisorArgumentsInvalid)
        );
    }
}
