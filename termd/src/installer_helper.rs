use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Url;
use reqwest::blocking::Client;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

const MAX_HELPER_INPUT_BYTES: u64 = 2 * 1024 * 1024;
const ALLOWED_STATE_TABLES: [&str; 8] = [
    "daemon_meta",
    "trusted_devices",
    "runtime_sessions",
    "http_uploads",
    "daemon_clients",
    "daemon_client_attached_sessions",
    "daemon_sessions",
    "session_ownership",
];

#[derive(Debug, Error)]
pub enum InstallerHelperError {
    #[error("unknown internal termd installer helper operation")]
    UnknownOperation,
    #[error("invalid arguments for internal termd installer helper")]
    InvalidArguments,
    #[error("internal termd installer helper input is too large")]
    InputTooLarge,
    #[error("failed to read internal termd installer helper input")]
    Input(#[source] io::Error),
    #[error("failed to write internal termd installer helper output")]
    Output(#[source] io::Error),
    #[error("installer state database operation failed")]
    Database(#[source] rusqlite::Error),
    #[error("installer state database does not satisfy the required safety checks: {0}")]
    UnsafeState(&'static str),
    #[error("failed to inspect installer filesystem state")]
    Filesystem(#[source] io::Error),
    #[error("invalid installer URL or listen address")]
    InvalidUrl,
    #[error("local daemon health request failed")]
    HealthRequest(#[source] reqwest::Error),
    #[error("invalid daemon or relay JSON response")]
    InvalidJson(#[source] serde_json::Error),
    #[error("invalid daemon identity in health response")]
    InvalidIdentity,
    #[error("relay daemon token file is empty or invalid")]
    InvalidDaemonToken,
    #[error("failed to signal session supervisor {pid}")]
    Signal {
        pid: u32,
        #[source]
        source: io::Error,
    },
    #[error("session supervisors did not exit after supervisor compatibility changed")]
    SupervisorTimeout,
}

pub fn run(request: terminstall::InternalHelperRequest) -> Result<(), InstallerHelperError> {
    let operation = request.operation();
    let input = if operation_uses_stdin(operation) {
        read_stdin()?
    } else {
        Vec::new()
    };
    let output = execute(operation, request.args(), &input)?;
    io::stdout()
        .lock()
        .write_all(&output)
        .map_err(InstallerHelperError::Output)
}

fn operation_uses_stdin(operation: &OsStr) -> bool {
    matches!(
        operation.to_str(),
        Some(
            "local-pairing-base-url"
                | "local-health-get"
                | "relay-api-url"
                | "health-identity"
                | "relay-registration-payload"
                | "relay-status-payload"
                | "relay-status-connected"
        )
    )
}

fn read_stdin() -> Result<Vec<u8>, InstallerHelperError> {
    let mut input = Vec::new();
    io::stdin()
        .lock()
        .take(MAX_HELPER_INPUT_BYTES + 1)
        .read_to_end(&mut input)
        .map_err(InstallerHelperError::Input)?;
    if input.len() as u64 > MAX_HELPER_INPUT_BYTES {
        return Err(InstallerHelperError::InputTooLarge);
    }
    Ok(input)
}

fn execute(
    operation: &OsStr,
    args: &[OsString],
    input: &[u8],
) -> Result<Vec<u8>, InstallerHelperError> {
    let output = match operation.to_str() {
        Some("self-check") => {
            require_arg_count(args, 0)?;
            String::new()
        }
        Some("generate-secret-token") => {
            require_arg_count(args, 0)?;
            generate_secret_token()?
        }
        Some("sqlite-meta-read") => {
            require_arg_count(args, 2)?;
            read_meta_value(Path::new(&args[0]), text_arg(args, 1)?)?
                .map(|value| format!("{value}\n"))
                .unwrap_or_default()
        }
        Some("sqlite-meta-upsert") => {
            require_arg_count(args, 3)?;
            upsert_meta_value(Path::new(&args[0]), text_arg(args, 1)?, text_arg(args, 2)?)?;
            String::new()
        }
        Some("sqlite-repair-installer-state") => {
            require_arg_count(args, 2)?;
            repair_installer_state(Path::new(&args[0]), Path::new(&args[1]))?;
            String::new()
        }
        Some("sqlite-has-runtime-sessions") => {
            require_arg_count(args, 1)?;
            format!("{}\n", has_runtime_sessions(Path::new(&args[0]))?)
        }
        Some("sqlite-clear-runtime-state") => {
            require_arg_count(args, 1)?;
            clear_runtime_state(Path::new(&args[0]))?;
            String::new()
        }
        Some("terminate-session-supervisors") => {
            require_arg_count(args, 1)?;
            terminate_session_supervisors(Path::new(&args[0]))?;
            String::new()
        }
        Some("sqlite-clear-all-session-state") => {
            require_arg_count(args, 1)?;
            clear_all_session_state(Path::new(&args[0]))?;
            String::new()
        }
        Some("sqlite-pending-ownership-count") => {
            require_arg_count(args, 1)?;
            format!("{}\n", pending_ownership_count(Path::new(&args[0]))?)
        }
        Some("local-pairing-base-url") => {
            require_arg_count(args, 1)?;
            let listen = input_text(input)?;
            format!("{}\n", local_pairing_base_url(listen, text_arg(args, 0)?)?)
        }
        Some("local-health-get") => {
            require_arg_count(args, 0)?;
            return local_health_get(input_text(input)?);
        }
        Some("relay-api-url") => {
            require_arg_count(args, 1)?;
            format!(
                "{}\n",
                relay_api_url(input_text(input)?, text_arg(args, 0)?)?
            )
        }
        Some("health-identity") => {
            require_arg_count(args, 0)?;
            let identity = health_identity(input)?;
            format!("{}\n{}\n", identity.server_id, identity.daemon_public_key)
        }
        Some("relay-registration-payload") => {
            require_arg_count(args, 1)?;
            relay_registration_payload(input, Path::new(&args[0]))?
        }
        Some("relay-status-payload") => {
            require_arg_count(args, 0)?;
            relay_status_payload(input)?
        }
        Some("relay-status-connected") => {
            require_arg_count(args, 1)?;
            format!("{}\n", relay_status_connected(input, text_arg(args, 0)?)?)
        }
        _ => return Err(InstallerHelperError::UnknownOperation),
    };
    Ok(output.into_bytes())
}

fn require_arg_count(args: &[OsString], expected: usize) -> Result<(), InstallerHelperError> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(InstallerHelperError::InvalidArguments)
    }
}

fn text_arg(args: &[OsString], index: usize) -> Result<&str, InstallerHelperError> {
    args.get(index)
        .and_then(|value| value.to_str())
        .ok_or(InstallerHelperError::InvalidArguments)
}

fn input_text(input: &[u8]) -> Result<&str, InstallerHelperError> {
    std::str::from_utf8(input)
        .map(str::trim)
        .map_err(|_| InstallerHelperError::InvalidArguments)
}

fn generate_secret_token() -> Result<String, InstallerHelperError> {
    let mut random = [0_u8; 32];
    fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut random))
        .map_err(InstallerHelperError::Filesystem)?;
    let mut token = String::with_capacity(65);
    for byte in random {
        use std::fmt::Write as _;
        write!(&mut token, "{byte:02x}").expect("writing to a String cannot fail");
    }
    token.push('\n');
    Ok(token)
}

fn open_database(path: &Path) -> Result<Connection, InstallerHelperError> {
    Connection::open(path).map_err(InstallerHelperError::Database)
}

fn table_names(connection: &Connection) -> Result<BTreeSet<String>, InstallerHelperError> {
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .map_err(InstallerHelperError::Database)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(InstallerHelperError::Database)?;
    rows.collect::<Result<BTreeSet<_>, _>>()
        .map_err(InstallerHelperError::Database)
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool, InstallerHelperError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get(0),
        )
        .map_err(InstallerHelperError::Database)
}

fn table_columns(
    connection: &Connection,
    table: &'static str,
) -> Result<BTreeSet<String>, InstallerHelperError> {
    if !ALLOWED_STATE_TABLES.contains(&table) {
        return Err(InstallerHelperError::UnsafeState("unexpected table name"));
    }
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info(\"{table}\")"))
        .map_err(InstallerHelperError::Database)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(InstallerHelperError::Database)?;
    rows.collect::<Result<BTreeSet<_>, _>>()
        .map_err(InstallerHelperError::Database)
}

fn read_meta_value(path: &Path, key: &str) -> Result<Option<String>, InstallerHelperError> {
    let connection = open_database(path)?;
    if !table_exists(&connection, "daemon_meta")? {
        return Ok(None);
    }
    connection
        .query_row(
            "SELECT value FROM daemon_meta WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()
        .map_err(InstallerHelperError::Database)
}

fn now_millis() -> Result<i64, InstallerHelperError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| InstallerHelperError::UnsafeState("system clock is before the Unix epoch"))?
        .as_millis();
    i64::try_from(millis)
        .map_err(|_| InstallerHelperError::UnsafeState("system clock is out of range"))
}

fn upsert_meta_value(path: &Path, key: &str, value: &str) -> Result<(), InstallerHelperError> {
    let connection = open_database(path)?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS daemon_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );",
        )
        .map_err(InstallerHelperError::Database)?;
    connection
        .execute(
            "INSERT INTO daemon_meta (key, value, updated_at_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at_ms = excluded.updated_at_ms",
            (key, value, now_millis()?),
        )
        .map_err(InstallerHelperError::Database)?;
    Ok(())
}

fn repair_installer_state(path: &Path, supervisor_dir: &Path) -> Result<(), InstallerHelperError> {
    let mut connection = open_database(path)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(InstallerHelperError::Database)?;
    let tables = table_names(&transaction)?;
    if !tables.contains("daemon_meta")
        || tables
            .iter()
            .any(|table| !ALLOWED_STATE_TABLES.contains(&table.as_str()))
    {
        return Err(InstallerHelperError::UnsafeState("unexpected state tables"));
    }
    let mut statement = transaction
        .prepare("SELECT key FROM daemon_meta")
        .map_err(InstallerHelperError::Database)?;
    let keys = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(InstallerHelperError::Database)?
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(InstallerHelperError::Database)?;
    drop(statement);
    if keys != BTreeSet::from(["supervisor_version".to_owned()]) {
        return Err(InstallerHelperError::UnsafeState(
            "unexpected daemon metadata",
        ));
    }
    for table in tables
        .iter()
        .filter(|table| table.as_str() != "daemon_meta")
    {
        let occupied: Option<i64> = transaction
            .query_row(&format!("SELECT 1 FROM \"{table}\" LIMIT 1"), [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(InstallerHelperError::Database)?;
        if occupied.is_some() {
            return Err(InstallerHelperError::UnsafeState(
                "state database contains user data",
            ));
        }
    }
    if directory_has_socket(supervisor_dir)? {
        return Err(InstallerHelperError::UnsafeState(
            "live supervisor socket exists",
        ));
    }
    transaction
        .execute(
            "DELETE FROM daemon_meta WHERE key = 'supervisor_version'",
            [],
        )
        .map_err(InstallerHelperError::Database)?;
    transaction.commit().map_err(InstallerHelperError::Database)
}

fn directory_has_socket(path: &Path) -> Result<bool, InstallerHelperError> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(InstallerHelperError::Filesystem(error)),
    };
    for entry in entries {
        let entry = entry.map_err(InstallerHelperError::Filesystem)?;
        if entry
            .file_type()
            .map_err(InstallerHelperError::Filesystem)?
            .is_socket()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn has_runtime_sessions(path: &Path) -> Result<bool, InstallerHelperError> {
    let connection = open_database(path)?;
    let tables = table_names(&connection)?;
    for table in [
        "daemon_client_attached_sessions",
        "daemon_sessions",
        "runtime_sessions",
    ] {
        if tables.contains(table) {
            let found: Option<i64> = connection
                .query_row(&format!("SELECT 1 FROM {table} LIMIT 1"), [], |row| {
                    row.get(0)
                })
                .optional()
                .map_err(InstallerHelperError::Database)?;
            if found.is_some() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn clear_runtime_state(path: &Path) -> Result<(), InstallerHelperError> {
    let mut connection = open_database(path)?;
    let transaction = connection
        .transaction()
        .map_err(InstallerHelperError::Database)?;
    let tables = table_names(&transaction)?;
    if tables.contains("daemon_client_attached_sessions") {
        transaction
            .execute("DELETE FROM daemon_client_attached_sessions", [])
            .map_err(InstallerHelperError::Database)?;
    }
    let now = now_millis()?;
    if tables.contains("daemon_sessions") {
        let columns = table_columns(&transaction, "daemon_sessions")?;
        if columns.contains("state") && columns.contains("updated_at_ms") {
            transaction
                .execute(
                    "UPDATE daemon_sessions SET state = 'closed', updated_at_ms = ?1",
                    [now],
                )
                .map_err(InstallerHelperError::Database)?;
        }
    }
    if tables.contains("runtime_sessions") {
        let columns = table_columns(&transaction, "runtime_sessions")?;
        if ["state", "updated_at_ms", "restore_kind", "restore_value"]
            .iter()
            .all(|column| columns.contains(*column))
        {
            transaction
                .execute(
                    "UPDATE runtime_sessions SET state = 'closed', updated_at_ms = ?1,
                     restore_kind = NULL, restore_value = NULL",
                    [now],
                )
                .map_err(InstallerHelperError::Database)?;
        } else {
            transaction
                .execute("DELETE FROM runtime_sessions", [])
                .map_err(InstallerHelperError::Database)?;
        }
    }
    transaction.commit().map_err(InstallerHelperError::Database)
}

fn clear_all_session_state(path: &Path) -> Result<(), InstallerHelperError> {
    let mut connection = open_database(path)?;
    let transaction = connection
        .transaction()
        .map_err(InstallerHelperError::Database)?;
    let tables = table_names(&transaction)?;
    for table in [
        "daemon_client_attached_sessions",
        "daemon_sessions",
        "runtime_sessions",
    ] {
        if tables.contains(table) {
            transaction
                .execute(&format!("DELETE FROM {table}"), [])
                .map_err(InstallerHelperError::Database)?;
        }
    }
    transaction.commit().map_err(InstallerHelperError::Database)
}

fn pending_ownership_count(path: &Path) -> Result<u64, InstallerHelperError> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(InstallerHelperError::Database)?;
    if !table_exists(&connection, "session_ownership")? {
        return Ok(0);
    }
    connection
        .query_row(
            "SELECT COUNT(*) FROM session_ownership WHERE phase IN ('preparing', 'cleaning')",
            [],
            |row| row.get(0),
        )
        .map_err(InstallerHelperError::Database)
}

fn terminate_session_supervisors(target_dir: &Path) -> Result<(), InstallerHelperError> {
    let pids = session_supervisor_pids_in(Path::new("/proc"), target_dir)?;
    for pid in &pids {
        signal_process(*pid, libc::SIGTERM)?;
    }
    let mut remaining = wait_for_processes(&pids, Duration::from_secs(5));
    for pid in &remaining {
        signal_process(*pid, libc::SIGKILL)?;
    }
    remaining = wait_for_processes(&remaining, Duration::from_secs(5));
    if remaining.is_empty() {
        Ok(())
    } else {
        Err(InstallerHelperError::SupervisorTimeout)
    }
}

fn session_supervisor_pids_in(
    proc_dir: &Path,
    target_dir: &Path,
) -> Result<Vec<u32>, InstallerHelperError> {
    let target_dir = canonical_or_absolute(target_dir)?;
    let mut matched = Vec::new();
    for entry in fs::read_dir(proc_dir).map_err(InstallerHelperError::Filesystem)? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if proc_dir == Path::new("/proc") && pid == std::process::id() {
            continue;
        }
        let cmdline = match fs::read(entry.path().join("cmdline")) {
            Ok(cmdline) => cmdline,
            Err(_) => continue,
        };
        let args = cmdline
            .split(|byte| *byte == 0)
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if !args.contains(&b"__session-supervisor".as_slice()) {
            continue;
        }
        let Some(socket_index) = args.iter().position(|value| *value == b"--socket-path") else {
            continue;
        };
        let Some(socket_path) = args.get(socket_index + 1) else {
            continue;
        };
        let socket_path = PathBuf::from(OsStr::from_bytes(socket_path));
        let Some(parent) = socket_path.parent() else {
            continue;
        };
        if canonical_or_absolute(parent)? == target_dir {
            matched.push(pid);
        }
    }
    matched.sort_unstable();
    Ok(matched)
}

fn canonical_or_absolute(path: &Path) -> Result<PathBuf, InstallerHelperError> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if path.is_absolute() {
                Ok(path.to_owned())
            } else {
                std::env::current_dir()
                    .map(|current| current.join(path))
                    .map_err(InstallerHelperError::Filesystem)
            }
        }
        Err(error) => Err(InstallerHelperError::Filesystem(error)),
    }
}

fn signal_process(pid: u32, signal: i32) -> Result<(), InstallerHelperError> {
    if unsafe { libc::kill(pid as i32, signal) } == 0 {
        return Ok(());
    }
    let source = io::Error::last_os_error();
    if source.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(InstallerHelperError::Signal { pid, source })
    }
}

fn wait_for_processes(pids: &[u32], timeout: Duration) -> Vec<u32> {
    let deadline = std::time::Instant::now() + timeout;
    let mut remaining = pids
        .iter()
        .copied()
        .filter(|pid| process_is_alive(*pid))
        .collect::<Vec<_>>();
    while !remaining.is_empty() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
        remaining.retain(|pid| process_is_alive(*pid));
    }
    remaining
}

fn process_is_alive(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/status")) {
        Ok(status) => status
            .lines()
            .find(|line| line.starts_with("State:"))
            .and_then(|line| line.split_whitespace().nth(1))
            .is_none_or(|state| !state.starts_with('Z')),
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

fn local_pairing_base_url(listen: &str, scheme: &str) -> Result<String, InstallerHelperError> {
    let (host, port) = if let Some(bracketed) = listen.strip_prefix('[') {
        let (host, port) = bracketed
            .rsplit_once("]:")
            .ok_or(InstallerHelperError::InvalidUrl)?;
        (host, port)
    } else {
        listen
            .rsplit_once(':')
            .ok_or(InstallerHelperError::InvalidUrl)?
    };
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(InstallerHelperError::InvalidUrl)?;
    let host = match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(address)) if address.is_unspecified() => "127.0.0.1".to_owned(),
        Ok(IpAddr::V6(address)) if address.is_unspecified() => "::1".to_owned(),
        _ => host.to_owned(),
    };
    let host = if host.contains(':') && !(host.starts_with('[') && host.ends_with(']')) {
        format!("[{host}]")
    } else {
        host
    };
    Ok(format!("{scheme}://{host}:{port}"))
}

fn local_health_get(endpoint: &str) -> Result<Vec<u8>, InstallerHelperError> {
    let response = Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(InstallerHelperError::HealthRequest)?
        .get(endpoint)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(InstallerHelperError::HealthRequest)?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_HELPER_INPUT_BYTES)
    {
        return Err(InstallerHelperError::InputTooLarge);
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_HELPER_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(InstallerHelperError::Input)?;
    if bytes.len() as u64 > MAX_HELPER_INPUT_BYTES {
        return Err(InstallerHelperError::InputTooLarge);
    }
    serde_json::from_slice::<Value>(&bytes).map_err(InstallerHelperError::InvalidJson)?;
    Ok(bytes)
}

fn relay_api_url(raw: &str, api_path: &str) -> Result<String, InstallerHelperError> {
    if !api_path.starts_with('/') || api_path.contains(['\r', '\n']) {
        return Err(InstallerHelperError::InvalidUrl);
    }
    let mut url = Url::parse(raw).map_err(|_| InstallerHelperError::InvalidUrl)?;
    let scheme = match url.scheme() {
        "wss" => "https",
        "ws" => "http",
        _ => return Err(InstallerHelperError::InvalidUrl),
    };
    url.set_scheme(scheme)
        .map_err(|_| InstallerHelperError::InvalidUrl)?;
    let mut prefix = url.path().trim_end_matches('/').to_owned();
    if prefix.ends_with("/ws") {
        prefix.truncate(prefix.len() - 3);
    }
    url.set_path(&format!("{prefix}{api_path}"));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

#[derive(Debug, Deserialize)]
struct HealthIdentity {
    server_id: String,
    daemon_public_key: String,
}

fn health_identity(input: &[u8]) -> Result<HealthIdentity, InstallerHelperError> {
    let identity: HealthIdentity =
        serde_json::from_slice(input).map_err(InstallerHelperError::InvalidJson)?;
    if identity.server_id.is_empty()
        || uuid::Uuid::parse_str(&identity.server_id).is_err()
        || !safe_line(&identity.daemon_public_key)
    {
        return Err(InstallerHelperError::InvalidIdentity);
    }
    Ok(identity)
}

fn safe_line(value: &str) -> bool {
    !value.is_empty() && !value.contains(['\r', '\n'])
}

fn relay_registration_payload(
    health: &[u8],
    daemon_token_path: &Path,
) -> Result<String, InstallerHelperError> {
    let identity = health_identity(health)?;
    let daemon_token_contents =
        fs::read_to_string(daemon_token_path).map_err(InstallerHelperError::Filesystem)?;
    let daemon_token = daemon_token_contents
        .lines()
        .next()
        .map(str::trim)
        .filter(|token| safe_line(token))
        .ok_or(InstallerHelperError::InvalidDaemonToken)?;
    serde_json::to_string(&json!({
        "server_id": identity.server_id,
        "daemon_token": daemon_token,
        "daemon_public_key": identity.daemon_public_key,
    }))
    .map(|mut payload| {
        payload.push('\n');
        payload
    })
    .map_err(InstallerHelperError::InvalidJson)
}

fn relay_status_payload(health: &[u8]) -> Result<String, InstallerHelperError> {
    let identity = health_identity(health)?;
    serde_json::to_string(&json!({ "server_id": identity.server_id }))
        .map(|mut payload| {
            payload.push('\n');
            payload
        })
        .map_err(InstallerHelperError::InvalidJson)
}

fn relay_status_connected(
    response: &[u8],
    expected_server_id: &str,
) -> Result<bool, InstallerHelperError> {
    let payload: Value =
        serde_json::from_slice(response).map_err(InstallerHelperError::InvalidJson)?;
    Ok(
        payload.get("server_id").and_then(Value::as_str) == Some(expected_server_id)
            && payload.get("connected").and_then(Value::as_bool) == Some(true),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "termd-installer-helper-{name}-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn operation(name: &str, args: &[&Path], input: &[u8]) -> Vec<u8> {
        execute(
            OsStr::new(name),
            &args
                .iter()
                .map(|path| path.as_os_str().to_owned())
                .collect::<Vec<_>>(),
            input,
        )
        .unwrap()
    }

    #[test]
    fn sqlite_meta_and_runtime_detection_operations_round_trip() {
        let root = TestDir::new("meta");
        let database = root.0.join("state.sqlite");
        execute(
            OsStr::new("sqlite-meta-upsert"),
            &[
                database.as_os_str().to_owned(),
                OsString::from("supervisor_version"),
                OsString::from("v2"),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(
            operation(
                "sqlite-meta-read",
                &[&database, Path::new("supervisor_version")],
                &[]
            ),
            b"v2\n"
        );
        assert_eq!(
            operation("sqlite-has-runtime-sessions", &[&database], &[]),
            b"false\n"
        );
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch("CREATE TABLE runtime_sessions (id TEXT); INSERT INTO runtime_sessions VALUES ('s');")
            .unwrap();
        assert_eq!(
            operation("sqlite-has-runtime-sessions", &[&database], &[]),
            b"true\n"
        );
    }

    #[test]
    fn secret_token_operation_returns_fresh_shell_safe_entropy() {
        let first = execute(OsStr::new("generate-secret-token"), &[], &[]).unwrap();
        let second = execute(OsStr::new("generate-secret-token"), &[], &[]).unwrap();
        assert_ne!(first, second);
        for token in [first, second] {
            let token = String::from_utf8(token).unwrap();
            assert_eq!(token.trim_end().len(), 64);
            assert!(
                token
                    .trim_end()
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            );
        }
    }

    #[test]
    fn poisoned_installer_state_repair_requires_empty_known_schema() {
        let root = TestDir::new("repair");
        let database = root.0.join("state.sqlite");
        upsert_meta_value(&database, "supervisor_version", "v1").unwrap();
        operation(
            "sqlite-repair-installer-state",
            &[&database, &root.0.join("supervisors")],
            &[],
        );
        assert_eq!(
            read_meta_value(&database, "supervisor_version").unwrap(),
            None
        );

        upsert_meta_value(&database, "supervisor_version", "v1").unwrap();
        Connection::open(&database)
            .unwrap()
            .execute_batch("CREATE TABLE unknown_state (value TEXT);")
            .unwrap();
        assert!(matches!(
            repair_installer_state(&database, &root.0.join("supervisors")),
            Err(InstallerHelperError::UnsafeState(_))
        ));
    }

    #[test]
    fn runtime_clear_and_full_session_clear_keep_their_distinct_contracts() {
        let root = TestDir::new("clear");
        let database = root.0.join("state.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE daemon_client_attached_sessions (id TEXT);
                 CREATE TABLE daemon_sessions (id TEXT, state TEXT, updated_at_ms INTEGER);
                 CREATE TABLE runtime_sessions (id TEXT, state TEXT, updated_at_ms INTEGER, restore_kind TEXT, restore_value TEXT);
                 INSERT INTO daemon_client_attached_sessions VALUES ('a');
                 INSERT INTO daemon_sessions VALUES ('s', 'running', 0);
                 INSERT INTO runtime_sessions VALUES ('s', 'running', 0, 'socket', '/tmp/sock');",
            )
            .unwrap();
        drop(connection);

        operation("sqlite-clear-runtime-state", &[&database], &[]);
        let connection = Connection::open(&database).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM daemon_sessions", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT state FROM runtime_sessions", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "closed"
        );
        drop(connection);

        operation("sqlite-clear-all-session-state", &[&database], &[]);
        let connection = Connection::open(&database).unwrap();
        for table in [
            "daemon_client_attached_sessions",
            "daemon_sessions",
            "runtime_sessions",
        ] {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0);
        }
    }

    #[test]
    fn ownership_precheck_operation_is_read_only_and_counts_pending_rows() {
        let root = TestDir::new("ownership");
        let database = root.0.join("state.sqlite");
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE session_ownership (phase TEXT);
                 INSERT INTO session_ownership VALUES ('active'), ('preparing'), ('cleaning');",
            )
            .unwrap();
        drop(connection);
        assert_eq!(
            operation("sqlite-pending-ownership-count", &[&database], &[]),
            b"2\n"
        );
    }

    #[test]
    fn supervisor_operation_matches_only_the_requested_socket_directory() {
        let root = TestDir::new("proc");
        let proc_dir = root.0.join("proc");
        let target = root.0.join("supervisors");
        fs::create_dir_all(proc_dir.join("4242")).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(
            proc_dir.join("4242/cmdline"),
            [
                b"termd".as_slice(),
                b"\0__session-supervisor\0--socket-path\0",
                target.join("s.sock").as_os_str().as_bytes(),
                b"\0",
            ]
            .concat(),
        )
        .unwrap();
        assert_eq!(
            session_supervisor_pids_in(&proc_dir, &target).unwrap(),
            [4242]
        );
        terminate_session_supervisors(&root.0.join("no-live-supervisors")).unwrap();
    }

    #[test]
    fn url_operations_handle_wildcard_and_relay_websocket_addresses() {
        assert_eq!(
            execute(
                OsStr::new("local-pairing-base-url"),
                &[OsString::from("http")],
                b"0.0.0.0:8765",
            )
            .unwrap(),
            b"http://127.0.0.1:8765\n"
        );
        assert_eq!(
            execute(
                OsStr::new("relay-api-url"),
                &[OsString::from("/api/relay/daemon/status")],
                b"wss://relay.example/base/ws?secret=ignored",
            )
            .unwrap(),
            b"https://relay.example/base/api/relay/daemon/status\n"
        );
    }

    #[test]
    fn health_and_relay_json_operations_escape_values_and_validate_identity() {
        let root = TestDir::new("json");
        let token_file = root.0.join("token");
        fs::write(&token_file, "token-with-\"-quote\n").unwrap();
        let health = br#"{"server_id":"00000000-0000-0000-0000-000000000001","daemon_public_key":"ed25519-v1:key"}"#;
        assert_eq!(
            execute(OsStr::new("health-identity"), &[], health).unwrap(),
            b"00000000-0000-0000-0000-000000000001\ned25519-v1:key\n"
        );
        let registration = execute(
            OsStr::new("relay-registration-payload"),
            &[token_file.into_os_string()],
            health,
        )
        .unwrap();
        let registration: Value = serde_json::from_slice(&registration).unwrap();
        assert_eq!(registration["daemon_token"], "token-with-\"-quote");
        let status = execute(OsStr::new("relay-status-payload"), &[], health).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&status).unwrap()["server_id"],
            "00000000-0000-0000-0000-000000000001"
        );
        assert_eq!(
            execute(
                OsStr::new("relay-status-connected"),
                &[OsString::from("00000000-0000-0000-0000-000000000001")],
                br#"{"server_id":"00000000-0000-0000-0000-000000000001","connected":true}"#,
            )
            .unwrap(),
            b"true\n"
        );
    }

    #[test]
    fn local_health_operation_fetches_and_validates_json() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}")
                .unwrap();
        });
        let endpoint = format!("http://{address}/healthz");
        assert_eq!(
            execute(OsStr::new("local-health-get"), &[], endpoint.as_bytes()).unwrap(),
            br#"{"ok":true}"#
        );
        server.join().unwrap();
    }

    #[test]
    fn helper_dispatch_rejects_unknown_operations_and_extra_arguments() {
        assert_eq!(
            execute(OsStr::new("self-check"), &[], &[]).unwrap(),
            Vec::<u8>::new()
        );
        assert!(matches!(
            execute(
                OsStr::new("self-check"),
                &[OsString::from("unexpected")],
                &[],
            ),
            Err(InstallerHelperError::InvalidArguments)
        ));
        assert!(matches!(
            execute(OsStr::new("unknown"), &[], &[]),
            Err(InstallerHelperError::UnknownOperation)
        ));
        assert!(matches!(
            execute(
                OsStr::new("sqlite-has-runtime-sessions"),
                &[OsString::from("one"), OsString::from("two")],
                &[],
            ),
            Err(InstallerHelperError::InvalidArguments)
        ));
    }
}
