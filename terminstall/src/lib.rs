use std::error::Error as StdError;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, IsTerminal, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use reqwest::blocking::{Client, Response};
use reqwest::header::{ACCEPT, HeaderValue};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

const TERMD_INSTALLER: &str = include_str!("../../scripts/install-termd.sh");
const TERMRELAY_INSTALLER: &str = include_str!("../../scripts/install-termrelay.sh");
const TERMCTL_INSTALLER: &str = include_str!("../../scripts/install-termctl.sh");
const SELF_INSTALL_MODE: &str = "embedded-v1";
const INTERNAL_HELPER_COMMAND: &str = "__terminstall-helper-v1";
const INTERNAL_HELPER_NONCE_ENV: &str = "TERMD_INSTALL_HELPER_NONCE";
const INTERNAL_HELPER_SELF_BINARY_ENV: &str = "TERMD_INSTALL_HELPER_SELF_BINARY";
const INTERNAL_HELPER_VERIFY_ENV: &str = "TERMD_INSTALL_VERIFY_HELPER";
const INSTALLER_SHELL: &str = "/bin/bash";
const INSTALLER_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";
const INSTALLER_VERIFY_PATH: &str = "/nonexistent/termd-installer-self-check";
const INSTALLER_LOCALE: &str = "C";
const DEFAULT_GITHUB_REPO: &str = "yiiilin/termd";
const GITHUB_API_ACCEPT: &str = "application/vnd.github+json";
const MAX_RELEASE_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 256 * 1024 * 1024;

pub fn supervisor_version() -> &'static str {
    include_str!("../../SUPERVISOR_VERSION").trim()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    Termd,
    Termrelay,
    Termctl,
}

impl Component {
    fn binary_name(self) -> &'static str {
        match self {
            Self::Termd => "termd",
            Self::Termrelay => "termrelay",
            Self::Termctl => "termctl",
        }
    }

    fn installer(self) -> &'static str {
        match self {
            Self::Termd => TERMD_INSTALLER,
            Self::Termrelay => TERMRELAY_INSTALLER,
            Self::Termctl => TERMCTL_INSTALLER,
        }
    }

    fn release_asset(self, architecture: Architecture) -> String {
        format!(
            "{}-linux-{}",
            self.binary_name(),
            architecture.asset_suffix()
        )
    }
}

impl fmt::Display for Component {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.binary_name())
    }
}

#[derive(Debug)]
pub enum InternalHelperError {
    InvalidRequest,
    InspectProcess(io::Error),
}

impl fmt::Display for InternalHelperError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest => {
                formatter.write_str("invalid internal installer helper request")
            }
            Self::InspectProcess(_) => {
                formatter.write_str("failed to validate internal installer helper process")
            }
        }
    }
}

impl StdError for InternalHelperError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::InspectProcess(source) => Some(source),
            Self::InvalidRequest => None,
        }
    }
}

#[derive(Debug)]
pub struct InternalHelperRequest {
    operation: OsString,
    args: Vec<OsString>,
}

impl InternalHelperRequest {
    pub fn operation(&self) -> &OsStr {
        &self.operation
    }

    pub fn args(&self) -> &[OsString] {
        &self.args
    }
}

pub fn internal_helper_request<I, S>(
    args: I,
) -> Result<Option<InternalHelperRequest>, InternalHelperError>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut args = args.into_iter().map(Into::into);
    let _program = args.next();
    if args.next().as_deref() != Some(OsStr::new(INTERNAL_HELPER_COMMAND)) {
        return Ok(None);
    }

    let nonce = std::env::var_os(INTERNAL_HELPER_NONCE_ENV);
    let self_binary = std::env::var_os(INTERNAL_HELPER_SELF_BINARY_ENV);
    // This runs synchronously at process entry before either binary creates its Tokio runtime.
    unsafe {
        std::env::remove_var(INTERNAL_HELPER_NONCE_ENV);
        std::env::remove_var(INTERNAL_HELPER_SELF_BINARY_ENV);
    }
    let nonce = nonce.ok_or(InternalHelperError::InvalidRequest)?;
    let self_binary = self_binary.ok_or(InternalHelperError::InvalidRequest)?;
    validate_internal_helper_context(&nonce, Path::new(&self_binary))?;

    let operation = args.next().ok_or(InternalHelperError::InvalidRequest)?;
    Ok(Some(InternalHelperRequest {
        operation,
        args: args.collect(),
    }))
}

fn validate_internal_helper_context(
    nonce: &OsStr,
    self_binary: &Path,
) -> Result<(), InternalHelperError> {
    if !valid_internal_helper_nonce(nonce) {
        return Err(InternalHelperError::InvalidRequest);
    }
    let installer_pid =
        pinned_self_binary_pid(self_binary).ok_or(InternalHelperError::InvalidRequest)?;
    if !process_is_ancestor(installer_pid)? {
        return Err(InternalHelperError::InvalidRequest);
    }
    let pinned_metadata = fs::metadata(self_binary).map_err(InternalHelperError::InspectProcess)?;
    let current_metadata =
        fs::metadata("/proc/self/exe").map_err(InternalHelperError::InspectProcess)?;
    let installer_metadata = fs::metadata(format!("/proc/{installer_pid}/exe"))
        .map_err(InternalHelperError::InspectProcess)?;
    if pinned_metadata.dev() != current_metadata.dev()
        || pinned_metadata.ino() != current_metadata.ino()
        || pinned_metadata.dev() != installer_metadata.dev()
        || pinned_metadata.ino() != installer_metadata.ino()
    {
        return Err(InternalHelperError::InvalidRequest);
    }
    Ok(())
}

fn valid_internal_helper_nonce(nonce: &OsStr) -> bool {
    let bytes = nonce.as_bytes();
    bytes.len() == 64
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn pinned_self_binary_pid(path: &Path) -> Option<u32> {
    let parts = path
        .as_os_str()
        .as_bytes()
        .split(|byte| *byte == b'/')
        .collect::<Vec<_>>();
    if parts.len() != 5
        || !parts[0].is_empty()
        || parts[1] != b"proc"
        || parts[3] != b"fd"
        || parts[4].is_empty()
        || !parts[4].iter().all(u8::is_ascii_digit)
    {
        return None;
    }
    let pid = std::str::from_utf8(parts[2]).ok()?.parse::<u32>().ok()?;
    (pid > 0).then_some(pid)
}

fn process_is_ancestor(expected_pid: u32) -> Result<bool, InternalHelperError> {
    let mut pid = unsafe { libc::getppid() } as u32;
    for _ in 0..8 {
        if pid == expected_pid {
            return Ok(true);
        }
        if pid <= 1 {
            return Ok(false);
        }
        pid = read_parent_pid(pid)?;
    }
    Ok(false)
}

fn read_parent_pid(pid: u32) -> Result<u32, InternalHelperError> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))
        .map_err(InternalHelperError::InspectProcess)?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:").map(str::trim))
        .and_then(|value| value.parse().ok())
        .ok_or(InternalHelperError::InvalidRequest)
}

pub struct InstallOptions {
    version: &'static str,
    supervisor_version: Option<&'static str>,
    args: Vec<OsString>,
}

impl InstallOptions {
    pub fn new<I, S>(
        version: &'static str,
        supervisor_version: Option<&'static str>,
        args: I,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self {
            version,
            supervisor_version,
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

impl fmt::Debug for InstallOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InstallOptions")
            .field("version", &self.version)
            .field(
                "supervisor_version_configured",
                &self.supervisor_version.is_some(),
            )
            .field("argument_count", &self.args.len())
            .finish()
    }
}

pub struct UninstallOptions {
    version: &'static str,
    args: Vec<OsString>,
}

pub struct UpgradeOptions {
    version: &'static str,
    args: Vec<OsString>,
}

impl UpgradeOptions {
    pub fn new<I, S>(version: &'static str, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self {
            version,
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

impl fmt::Debug for UpgradeOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpgradeOptions")
            .field("version", &self.version)
            .field("argument_count", &self.args.len())
            .finish()
    }
}

impl UninstallOptions {
    pub fn new<I, S>(version: &'static str, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self {
            version,
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

impl fmt::Debug for UninstallOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UninstallOptions")
            .field("version", &self.version)
            .field("argument_count", &self.args.len())
            .finish()
    }
}

pub enum Options {
    Install(InstallOptions),
    Uninstall(UninstallOptions),
    Upgrade(UpgradeOptions),
}

impl Options {
    pub fn from_subcommand<I, S>(
        version: &'static str,
        supervisor_version: Option<&'static str>,
        args: I,
    ) -> Option<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut args = args.into_iter().map(Into::into);
        let command = args.next()?;
        let remaining = args.collect::<Vec<_>>();
        if command == OsStr::new("install") {
            Some(Self::Install(InstallOptions::new(
                version,
                supervisor_version,
                remaining,
            )))
        } else if command == OsStr::new("uninstall") {
            Some(Self::Uninstall(UninstallOptions::new(version, remaining)))
        } else if command == OsStr::new("upgrade") {
            Some(Self::Upgrade(UpgradeOptions::new(version, remaining)))
        } else {
            None
        }
    }
}

impl fmt::Debug for Options {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Install(options) => options.fmt(formatter),
            Self::Uninstall(options) => options.fmt(formatter),
            Self::Upgrade(options) => options.fmt(formatter),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    UnknownArgument(String),
    DuplicateArgument(&'static str),
    ConflictingArguments(&'static str, &'static str),
    MissingValue(&'static str),
    EmptyValue(&'static str),
    PurgeUnsupported,
    NonInteractive,
    RelayTokenRequired,
    EmptyRelayToken,
    InvalidRelayUrl,
    Cancelled,
    OpenSelfExecutable(io::Error),
    InspectExistingInstall(io::Error),
    Randomness(io::Error),
    Input(io::Error),
    Output(io::Error),
    Spawn(io::Error),
    WriteInstaller(io::Error),
    Wait(io::Error),
    ChildFailed {
        component: Component,
        status: String,
    },
    Upgrade(UpgradeError),
    UpgradeThreadPanicked,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownArgument(argument) => write!(formatter, "unknown installer argument: {argument}"),
            Self::DuplicateArgument(flag) => {
                write!(formatter, "{flag} may be specified only once")
            }
            Self::ConflictingArguments(first, second) => {
                write!(formatter, "{first} conflicts with {second}; do not use them together")
            }
            Self::MissingValue(flag) => write!(formatter, "{flag} requires a value"),
            Self::EmptyValue(flag) => write!(formatter, "{flag} requires a non-empty value"),
            Self::PurgeUnsupported => formatter.write_str(
                "termctl has no managed service state; --purge is unsupported and normal uninstall preserves pairing state",
            ),
            Self::NonInteractive => formatter.write_str(
                "installer confirmation requires a TTY; review with --dry-run, then rerun with --yes for non-interactive execution",
            ),
            Self::RelayTokenRequired => formatter.write_str(
                "trusted relay setup token is required for non-interactive installation; pass --relay-token <TOKEN> or --relay-setup-token-file <PATH>",
            ),
            Self::EmptyRelayToken => formatter.write_str("relay setup token cannot be empty"),
            Self::InvalidRelayUrl => formatter.write_str(
                "relay URL must be an absolute ws:// or wss:// URL with a host",
            ),
            Self::Cancelled => formatter.write_str("installation cancelled"),
            Self::OpenSelfExecutable(_) => {
                formatter.write_str("failed to pin the running executable")
            }
            Self::InspectExistingInstall(_) => {
                formatter.write_str("failed to inspect existing managed installation")
            }
            Self::Randomness(_) => formatter.write_str("failed to initialize installer helper authentication"),
            Self::Input(_) => formatter.write_str("failed to read installer confirmation"),
            Self::Output(_) => formatter.write_str("failed to write installer output"),
            Self::Spawn(_) => formatter.write_str("failed to start the embedded bash installer"),
            Self::WriteInstaller(_) => {
                formatter.write_str("failed to write the embedded installer to bash stdin")
            }
            Self::Wait(_) => formatter.write_str("failed to wait for the embedded installer"),
            Self::ChildFailed { component, status } => {
                write!(formatter, "embedded {component} installer exited with {status}")
            }
            Self::Upgrade(source) => source.fmt(formatter),
            Self::UpgradeThreadPanicked => formatter.write_str("upgrade worker terminated unexpectedly"),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::OpenSelfExecutable(source)
            | Self::InspectExistingInstall(source)
            | Self::Randomness(source)
            | Self::Input(source)
            | Self::Output(source)
            | Self::Spawn(source)
            | Self::WriteInstaller(source)
            | Self::Wait(source) => Some(source),
            Self::Upgrade(source) => Some(source),
            _ => None,
        }
    }
}

pub fn run(component: Component, options: Options) -> Result<(), Error> {
    match options {
        Options::Upgrade(options) => std::thread::spawn(move || {
            run_upgrade_with(component, options, &mut SystemUpgradeRuntime).map_err(Error::Upgrade)
        })
        .join()
        .map_err(|_| Error::UpgradeThreadPanicked)?,
        options => run_with(component, options, &mut SystemRuntime),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Install,
    Uninstall,
}

struct ParsedOptions {
    action: Action,
    version: &'static str,
    supervisor_version: Option<&'static str>,
    yes: bool,
    dry_run: bool,
    help: bool,
    purge: bool,
    allow_session_loss: bool,
    relay_explicit: bool,
    prompt_relay_token: bool,
    explicit_install_settings: bool,
    script_args: Vec<OsString>,
    script_env: Vec<(&'static str, OsString)>,
    summary: Vec<String>,
}

impl ParsedOptions {
    fn parse(component: Component, options: Options) -> Result<Self, Error> {
        let (action, version, supervisor_version, args) = match options {
            Options::Install(options) => (
                Action::Install,
                options.version,
                options.supervisor_version,
                options.args,
            ),
            Options::Uninstall(options) => (Action::Uninstall, options.version, None, options.args),
            Options::Upgrade(_) => unreachable!("upgrade options use the dedicated upgrade path"),
        };
        let mut parsed = Self {
            action,
            version,
            supervisor_version,
            yes: false,
            dry_run: false,
            help: false,
            purge: false,
            allow_session_loss: false,
            relay_explicit: false,
            prompt_relay_token: false,
            explicit_install_settings: false,
            script_args: Vec::new(),
            script_env: Vec::new(),
            summary: Vec::new(),
        };
        let mut args = args.into_iter();
        while let Some(argument) = args.next() {
            let raw_argument = argument
                .to_str()
                .ok_or_else(|| Error::UnknownArgument("<non-UTF-8>".to_owned()))?;
            let (flag, inline_value) = match raw_argument.split_once('=') {
                Some((flag, value)) => (flag, Some(OsString::from(value))),
                None => (raw_argument, None),
            };
            match flag {
                "-h" | "--help" if inline_value.is_none() => parsed.help = true,
                "--dry-run" if inline_value.is_none() => parsed.dry_run = true,
                "--yes" if inline_value.is_none() => parsed.yes = true,
                "--allow-session-loss"
                    if inline_value.is_none()
                        && action == Action::Install
                        && component == Component::Termd =>
                {
                    parsed.allow_session_loss = true;
                    parsed
                        .script_args
                        .push(OsString::from("--allow-session-loss"));
                    parsed.summary.push(
                        "allow incompatible supervisor upgrade to remove sessions: yes".to_owned(),
                    );
                }
                "--purge"
                    if inline_value.is_none()
                        && action == Action::Uninstall
                        && component != Component::Termctl =>
                {
                    parsed.purge = true;
                    parsed.script_args.push(OsString::from("--purge"));
                    parsed.summary.push("purge local state: yes".to_owned());
                }
                "--purge" if inline_value.is_none() && action == Action::Uninstall => {
                    return Err(Error::PurgeUnsupported);
                }
                _ if action == Action::Install => {
                    let specification = install_flag(component, flag)
                        .ok_or_else(|| Error::UnknownArgument(safe_unknown_argument(flag)))?;
                    parsed.explicit_install_settings = true;
                    if specification.takes_value {
                        let value = match inline_value {
                            Some(value) => value,
                            None => args.next().ok_or(Error::MissingValue(specification.flag))?,
                        };
                        if value.is_empty() {
                            return Err(Error::EmptyValue(specification.flag));
                        }
                        let summary_value = if let Some(environment_name) = specification.script_env
                        {
                            if environment_name == "TERMD_INSTALL_ARG_RELAY_URL"
                                && parsed
                                    .script_env
                                    .iter()
                                    .any(|(name, _)| *name == environment_name)
                            {
                                return Err(Error::DuplicateArgument("--relay"));
                            }
                            if let Some(flag) = match environment_name {
                                "TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN" => Some("--relay-token"),
                                "TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN_FILE" => {
                                    Some("--relay-setup-token-file")
                                }
                                _ => None,
                            } && parsed
                                .script_env
                                .iter()
                                .any(|(name, _)| *name == environment_name)
                            {
                                return Err(Error::DuplicateArgument(flag));
                            }
                            if environment_name == "TERMD_INSTALL_ARG_RELAY_URL" {
                                parsed.relay_explicit = true;
                            }
                            parsed.script_env.push((environment_name, value));
                            "configured".to_owned()
                        } else {
                            parsed.script_args.push(OsString::from(specification.flag));
                            parsed.script_args.push(value.clone());
                            value.to_string_lossy().into_owned()
                        };
                        parsed
                            .summary
                            .push(format!("{}: {summary_value}", specification.summary));
                    } else {
                        if inline_value.is_some() {
                            return Err(Error::UnknownArgument(safe_unknown_argument(flag)));
                        }
                        parsed.script_args.push(OsString::from(specification.flag));
                        parsed
                            .summary
                            .push(format!("{}: yes", specification.summary));
                    }
                }
                _ => return Err(Error::UnknownArgument(safe_unknown_argument(flag))),
            }
        }
        let has_direct_relay_token = parsed
            .script_env
            .iter()
            .any(|(name, _)| *name == "TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN");
        let has_relay_token_file = parsed
            .script_env
            .iter()
            .any(|(name, _)| *name == "TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN_FILE");
        if has_direct_relay_token && has_relay_token_file {
            return Err(Error::ConflictingArguments(
                "--relay-token",
                "--relay-setup-token-file",
            ));
        }
        if parsed.relay_explicit && !has_direct_relay_token && !has_relay_token_file {
            parsed.prompt_relay_token = true;
            parsed
                .summary
                .push("relay setup token: will prompt securely during installation".to_owned());
        }
        Ok(parsed)
    }
}

fn safe_unknown_argument(argument: &str) -> String {
    if argument.starts_with('-') {
        argument.to_owned()
    } else {
        "<argument>".to_owned()
    }
}

struct FlagSpecification {
    flag: &'static str,
    summary: &'static str,
    takes_value: bool,
    script_env: Option<&'static str>,
}

const fn boolean_flag(flag: &'static str, summary: &'static str) -> FlagSpecification {
    FlagSpecification {
        flag,
        summary,
        takes_value: false,
        script_env: None,
    }
}

const fn value_flag(
    flag: &'static str,
    summary: &'static str,
    script_env: Option<&'static str>,
) -> FlagSpecification {
    FlagSpecification {
        flag,
        summary,
        takes_value: true,
        script_env,
    }
}

fn install_flag(component: Component, flag: &str) -> Option<FlagSpecification> {
    match (component, flag) {
        (Component::Termd, "--web") => Some(boolean_flag("--web", "embedded Web UI enabled")),
        (Component::Termd, "--no-web") => {
            Some(boolean_flag("--no-web", "embedded Web UI disabled"))
        }
        (Component::Termd, "--public") => Some(boolean_flag("--public", "public listen alias")),
        (Component::Termd, "--listen") => Some(value_flag("--listen", "listen address", None)),
        (Component::Termd, "--relay") | (Component::Termd, "--relay-url") => Some(value_flag(
            "--relay",
            "trusted relay",
            Some("TERMD_INSTALL_ARG_RELAY_URL"),
        )),
        (Component::Termd, "--relay-daemon-token-file") => Some(value_flag(
            "--relay-daemon-token-file",
            "relay daemon token file",
            Some("TERMD_INSTALL_ARG_RELAY_DAEMON_TOKEN_FILE"),
        )),
        (Component::Termd, "--relay-setup-token-file") => Some(value_flag(
            "--relay-setup-token-file",
            "relay setup token file",
            Some("TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN_FILE"),
        )),
        (Component::Termd, "--relay-token") => Some(value_flag(
            "--relay-token",
            "relay setup token",
            Some("TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN"),
        )),
        (Component::Termd, "--proxy") | (Component::Termd, "--relay-proxy") => Some(value_flag(
            "--proxy",
            "outbound proxy",
            Some("TERMD_INSTALL_ARG_PROXY"),
        )),
        (Component::Termd, "--tls-cert") => Some(value_flag("--tls-cert", "TLS certificate", None)),
        (Component::Termd, "--tls-key") => Some(value_flag(
            "--tls-key",
            "TLS private key",
            Some("TERMD_INSTALL_ARG_TLS_KEY"),
        )),
        (Component::Termd, "--supervisor-version") => Some(value_flag(
            "--supervisor-version",
            "supervisor compatibility",
            None,
        )),
        (Component::Termd, "--user") => Some(value_flag("--user", "service user", None)),
        (Component::Termrelay, "--web") => Some(boolean_flag("--web", "embedded Web UI enabled")),
        (Component::Termrelay, "--no-web") => {
            Some(boolean_flag("--no-web", "embedded Web UI disabled"))
        }
        (Component::Termrelay, "--public") => Some(boolean_flag("--public", "public listen alias")),
        (Component::Termrelay, "--listen") => Some(value_flag("--listen", "listen address", None)),
        (Component::Termrelay, "--setup-token-file") => Some(value_flag(
            "--setup-token-file",
            "relay setup token file",
            Some("TERMD_INSTALL_ARG_SETUP_TOKEN_FILE"),
        )),
        (Component::Termrelay, "--daemon-registry") => {
            Some(value_flag("--daemon-registry", "daemon registry", None))
        }
        (Component::Termrelay, "--tls-cert") => {
            Some(value_flag("--tls-cert", "TLS certificate", None))
        }
        (Component::Termrelay, "--tls-key") => Some(value_flag(
            "--tls-key",
            "TLS private key",
            Some("TERMD_INSTALL_ARG_TLS_KEY"),
        )),
        _ => None,
    }
}

struct Invocation {
    component: Component,
    script: &'static str,
    args: Vec<OsString>,
    env: Vec<(&'static str, OsString)>,
}

struct ExecutionStatus {
    success: bool,
    description: String,
}

struct SelfBinary {
    display_path: PathBuf,
    installer_path: PathBuf,
    _open_file: Option<File>,
}

trait Runtime {
    fn is_tty(&self) -> bool;
    fn open_self_binary(&mut self) -> Result<SelfBinary, Error>;
    fn install_prefix(&self) -> PathBuf;
    fn managed_install_exists(&self, component: Component) -> Result<bool, Error>;
    fn sudo_user(&self) -> Option<OsString>;
    fn write_output(&mut self, message: &str) -> Result<(), Error>;
    fn read_line(&mut self, prompt: &str) -> Result<String, Error>;
    fn confirm(&mut self, prompt: &str) -> Result<bool, Error>;
    fn read_secret(&mut self, prompt: &str) -> Result<OsString, Error>;
    fn execute(&mut self, invocation: &Invocation) -> Result<ExecutionStatus, Error>;
}

struct SystemRuntime;

impl Runtime for SystemRuntime {
    fn is_tty(&self) -> bool {
        io::stdin().is_terminal() && io::stderr().is_terminal()
    }

    fn open_self_binary(&mut self) -> Result<SelfBinary, Error> {
        let open_file = File::open("/proc/self/exe").map_err(Error::OpenSelfExecutable)?;
        let installer_path = PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            open_file.as_raw_fd()
        ));
        let display_path = std::env::current_exe().unwrap_or_else(|_| installer_path.clone());
        Ok(SelfBinary {
            display_path,
            installer_path,
            _open_file: Some(open_file),
        })
    }

    fn install_prefix(&self) -> PathBuf {
        std::env::var_os("TERMD_INSTALL_PREFIX")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/local"))
    }

    fn managed_install_exists(&self, component: Component) -> Result<bool, Error> {
        let paths = match component {
            Component::Termd => vec![
                PathBuf::from("/etc/termd/termd.env"),
                PathBuf::from("/etc/systemd/system/termd.service"),
            ],
            Component::Termrelay => vec![
                PathBuf::from("/etc/termd/termrelay.env"),
                PathBuf::from("/etc/systemd/system/termrelay.service"),
            ],
            Component::Termctl => vec![self.install_prefix().join("bin/termctl")],
        };
        for path in paths {
            match fs::symlink_metadata(path) {
                Ok(_) => return Ok(true),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(Error::InspectExistingInstall(error)),
            }
        }
        Ok(false)
    }

    fn sudo_user(&self) -> Option<OsString> {
        std::env::var_os("SUDO_USER").filter(|user| !user.is_empty() && user != OsStr::new("root"))
    }

    fn write_output(&mut self, message: &str) -> Result<(), Error> {
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(message.as_bytes())
            .and_then(|_| stdout.flush())
            .map_err(Error::Output)
    }

    fn read_line(&mut self, prompt: &str) -> Result<String, Error> {
        let mut stderr = io::stderr().lock();
        stderr.write_all(prompt.as_bytes()).map_err(Error::Output)?;
        stderr.flush().map_err(Error::Output)?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer).map_err(Error::Input)?;
        Ok(answer.trim().to_owned())
    }

    fn confirm(&mut self, prompt: &str) -> Result<bool, Error> {
        let answer = self.read_line(prompt)?;
        Ok(matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
    }

    fn read_secret(&mut self, prompt: &str) -> Result<OsString, Error> {
        let mut stderr = io::stderr().lock();
        stderr.write_all(prompt.as_bytes()).map_err(Error::Output)?;
        stderr.flush().map_err(Error::Output)?;

        let stdin = io::stdin();
        let fd = stdin.as_raw_fd();
        let mut original = MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(Error::Input(io::Error::last_os_error()));
        }
        let original = unsafe { original.assume_init() };
        let mut hidden = original;
        hidden.c_lflag &= !libc::ECHO;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &hidden) } != 0 {
            return Err(Error::Input(io::Error::last_os_error()));
        }

        struct EchoGuard {
            fd: i32,
            original: libc::termios,
        }
        impl Drop for EchoGuard {
            fn drop(&mut self) {
                unsafe {
                    libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
                }
            }
        }
        let guard = EchoGuard { fd, original };
        let mut value = String::new();
        let read_result = stdin.read_line(&mut value);
        drop(guard);
        stderr.write_all(b"\n").map_err(Error::Output)?;
        stderr.flush().map_err(Error::Output)?;
        read_result.map_err(Error::Input)?;
        let value = value.trim().to_owned();
        if value.is_empty() {
            return Err(Error::EmptyRelayToken);
        }
        Ok(OsString::from(value))
    }

    fn execute(&mut self, invocation: &Invocation) -> Result<ExecutionStatus, Error> {
        let installer_path = if invocation
            .env
            .iter()
            .any(|(name, _)| *name == INTERNAL_HELPER_VERIFY_ENV)
        {
            INSTALLER_VERIFY_PATH
        } else {
            INSTALLER_PATH
        };
        let mut command = Command::new(INSTALLER_SHELL);
        command
            .env_clear()
            .env("PATH", installer_path)
            .env("LANG", INSTALLER_LOCALE)
            .env("LC_ALL", INSTALLER_LOCALE)
            .arg("-s")
            .arg("--")
            .args(&invocation.args)
            .stdin(Stdio::piped());
        for name in ["no_proxy", "NO_PROXY"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        for (name, value) in &invocation.env {
            command.env(name, value);
        }
        let mut child = command.spawn().map_err(Error::Spawn)?;
        let write_result = child
            .stdin
            .take()
            .expect("piped stdin must be available")
            .write_all(invocation.script.as_bytes());
        if let Err(source) = write_result {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::WriteInstaller(source));
        }
        let status = match child.wait() {
            Ok(status) => status,
            Err(source) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Wait(source));
            }
        };
        Ok(ExecutionStatus {
            success: status.success(),
            description: status.to_string(),
        })
    }
}

fn apply_fresh_install_defaults(
    component: Component,
    parsed: &mut ParsedOptions,
    runtime: &mut dyn Runtime,
) -> Result<(), Error> {
    if parsed.action != Action::Install
        || parsed.explicit_install_settings
        || runtime.managed_install_exists(component)?
    {
        return Ok(());
    }

    match component {
        Component::Termrelay => {
            add_script_flag(
                parsed,
                "--web",
                "embedded Web UI enabled by fresh-install default",
            );
            add_script_value(parsed, "--listen", "127.0.0.1:8080", "listen address");
        }
        Component::Termd => {
            add_script_flag(
                parsed,
                "--web",
                "embedded Web UI enabled by fresh-install default",
            );
            add_script_value(parsed, "--listen", "127.0.0.1:8765", "listen address");
            if parsed.dry_run || parsed.yes || !runtime.is_tty() {
                return Ok(());
            }

            let user = match runtime.sudo_user() {
                Some(sudo_user) => {
                    let prompt = format!(
                        "Use sudo login user '{}' for terminal sessions? [Y/n] ",
                        sudo_user.to_string_lossy()
                    );
                    if prompt_yes_no(runtime, &prompt, true)? {
                        Some(sudo_user)
                    } else {
                        optional_session_user(runtime)?
                    }
                }
                None => optional_session_user(runtime)?,
            };
            if let Some(user) = user {
                parsed.script_args.push(OsString::from("--user"));
                parsed.script_args.push(user.clone());
                parsed
                    .summary
                    .push(format!("service user: {}", user.to_string_lossy()));
            }

            if prompt_yes_no(
                runtime,
                "Connect this daemon through a trusted relay? [y/N] ",
                false,
            )? {
                let relay_url = runtime.read_line("Relay WebSocket URL (wss://...): ")?;
                let parsed_url =
                    reqwest::Url::parse(&relay_url).map_err(|_| Error::InvalidRelayUrl)?;
                if !matches!(parsed_url.scheme(), "ws" | "wss") || parsed_url.host().is_none() {
                    return Err(Error::InvalidRelayUrl);
                }
                parsed
                    .script_env
                    .push(("TERMD_INSTALL_ARG_RELAY_URL", OsString::from(relay_url)));
                parsed.relay_explicit = true;
                parsed.prompt_relay_token = true;
                parsed.summary.push("trusted relay: configured".to_owned());
                parsed
                    .summary
                    .push("relay setup token: will prompt securely during installation".to_owned());
            }
        }
        Component::Termctl => {}
    }
    Ok(())
}

fn add_script_flag(parsed: &mut ParsedOptions, flag: &'static str, summary: &'static str) {
    parsed.script_args.push(OsString::from(flag));
    parsed.summary.push(format!("{summary}: yes"));
}

fn add_script_value(
    parsed: &mut ParsedOptions,
    flag: &'static str,
    value: &'static str,
    summary: &'static str,
) {
    parsed.script_args.push(OsString::from(flag));
    parsed.script_args.push(OsString::from(value));
    parsed.summary.push(format!("{summary}: {value}"));
}

fn optional_session_user(runtime: &mut dyn Runtime) -> Result<Option<OsString>, Error> {
    let user =
        runtime.read_line("Session user (leave empty to use the managed termd service user): ")?;
    Ok((!user.is_empty()).then(|| OsString::from(user)))
}

fn prompt_yes_no(runtime: &mut dyn Runtime, prompt: &str, default: bool) -> Result<bool, Error> {
    let answer = runtime.read_line(prompt)?.to_ascii_lowercase();
    if answer.is_empty() {
        return Ok(default);
    }
    Ok(matches!(answer.as_str(), "y" | "yes"))
}

fn run_with(
    component: Component,
    options: Options,
    runtime: &mut dyn Runtime,
) -> Result<(), Error> {
    let mut parsed = ParsedOptions::parse(component, options)?;
    if parsed.help {
        return runtime.write_output(&help_text(component, parsed.action));
    }
    apply_fresh_install_defaults(component, &mut parsed, runtime)?;

    let self_binary = runtime.open_self_binary()?;
    let install_prefix = runtime.install_prefix();
    let plan = render_plan(
        component,
        &parsed,
        &self_binary.display_path,
        &install_prefix,
    );
    runtime.write_output(&plan)?;
    if parsed.dry_run {
        if component != Component::Termctl {
            let invocation = Invocation {
                component,
                script: component.installer(),
                args: Vec::new(),
                env: vec![
                    ("TERMD_INSTALL_SELF_MODE", OsString::from(SELF_INSTALL_MODE)),
                    (
                        "TERMD_INSTALL_SELF_BINARY",
                        self_binary.installer_path.as_os_str().to_owned(),
                    ),
                    ("TERMD_VERSION", OsString::from(parsed.version)),
                    (INTERNAL_HELPER_NONCE_ENV, generate_internal_helper_nonce()?),
                    (INTERNAL_HELPER_VERIFY_ENV, OsString::from("1")),
                ],
            };
            let status = runtime.execute(&invocation)?;
            if !status.success {
                return Err(Error::ChildFailed {
                    component: invocation.component,
                    status: status.description,
                });
            }
        }
        return Ok(());
    }
    if parsed.prompt_relay_token && !runtime.is_tty() {
        return Err(Error::RelayTokenRequired);
    }
    if !parsed.yes {
        if !runtime.is_tty() {
            return Err(Error::NonInteractive);
        }
        if !runtime.confirm("Continue? [y/N] ")? {
            return Err(Error::Cancelled);
        }
    }

    let prompted_relay_token = if parsed.prompt_relay_token {
        Some(runtime.read_secret("Relay setup token (input hidden): ")?)
    } else {
        None
    };
    let mut script_args = parsed.script_args;
    if parsed.action == Action::Uninstall {
        script_args.insert(0, OsString::from("--uninstall"));
    }
    let mut env = vec![
        ("TERMD_INSTALL_SELF_MODE", OsString::from(SELF_INSTALL_MODE)),
        (
            "TERMD_INSTALL_SELF_BINARY",
            self_binary.installer_path.as_os_str().to_owned(),
        ),
        ("TERMD_VERSION", OsString::from(parsed.version)),
        (
            "TERMD_INSTALL_PREFIX",
            install_prefix.as_os_str().to_owned(),
        ),
    ];
    if component != Component::Termctl {
        env.push((INTERNAL_HELPER_NONCE_ENV, generate_internal_helper_nonce()?));
    }
    env.extend(parsed.script_env);
    if let Some(token) = prompted_relay_token {
        env.push(("TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN", token));
    }
    if component == Component::Termd
        && let Some(supervisor_version) = parsed.supervisor_version
    {
        env.push((
            "TERMD_REQUIRED_SUPERVISOR_VERSION",
            OsString::from(supervisor_version),
        ));
    }
    let invocation = Invocation {
        component,
        script: component.installer(),
        args: script_args,
        env,
    };
    let status = runtime.execute(&invocation)?;
    drop(self_binary);
    if !status.success {
        return Err(Error::ChildFailed {
            component: invocation.component,
            status: status.description,
        });
    }
    Ok(())
}

fn generate_internal_helper_nonce() -> Result<OsString, Error> {
    let mut random = [0_u8; 32];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut random))
        .map_err(Error::Randomness)?;
    let mut encoded = String::with_capacity(random.len() * 2);
    for byte in random {
        use fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(OsString::from(encoded))
}

fn render_plan(
    component: Component,
    parsed: &ParsedOptions,
    current_exe: &Path,
    install_prefix: &Path,
) -> String {
    let action = match parsed.action {
        Action::Install => "install or upgrade",
        Action::Uninstall => "uninstall",
    };
    let mut output = String::new();
    if parsed.dry_run {
        output.push_str("Dry run: no files or services will be changed.\n");
    }
    output.push_str(&format!(
        "[1/3] Component\n  action: {action}\n  component: {component} {}\n  binary: {}\n  target: {}\n",
        parsed.version,
        current_exe.display(),
        install_prefix.join("bin").join(component.binary_name()).display(),
    ));
    output.push_str("[2/3] Configuration\n");
    if parsed.summary.is_empty() {
        output
            .push_str("  existing configuration is preserved; fresh installs use safe defaults\n");
    } else {
        for summary in &parsed.summary {
            output.push_str("  ");
            output.push_str(summary);
            output.push('\n');
        }
    }
    output.push_str(
        "[3/3] Safety\n  the embedded installer keeps existing rollback and service checks\n",
    );
    if parsed.purge {
        output.push_str("  WARNING: purge removes component state and cannot be undone\n");
    } else if parsed.allow_session_loss {
        output.push_str(
            "  WARNING: session loss is authorized if supervisor compatibility changes\n",
        );
    } else if parsed.action == Action::Uninstall {
        output.push_str("  local state is preserved\n");
    } else {
        output.push_str("  network installer download and source-build fallback are bypassed\n");
    }
    output
}

fn help_text(component: Component, action: Action) -> String {
    let mut output = format!(
        "Usage: {component} {} [OPTIONS]\n\nCommon options:\n  --dry-run  Show the non-sensitive execution plan without requiring root\n  --yes      Run non-interactively after reviewing the plan\n  -h, --help Show this help\n",
        match action {
            Action::Install => "install",
            Action::Uninstall => "uninstall",
        }
    );
    if action == Action::Uninstall {
        if component == Component::Termctl {
            output.push_str(
                "\ntermctl uninstall removes only the installed binary; --purge is unsupported.\n",
            );
        } else {
            output
                .push_str("  --purge    Also remove component state and the managed system user\n");
        }
        return output;
    }
    match component {
        Component::Termd => output.push_str(
            "\ntermd options:\n  --web | --no-web\n  --listen <HOST:PORT> | --public\n  --relay <WS_URL>\n  --relay-token <TOKEN>\n  --relay-daemon-token-file <PATH>\n  --relay-setup-token-file <PATH>\n  --proxy <URL>\n  --tls-cert <PATH> --tls-key <PATH>\n  --user <USER>\n  --supervisor-version <VERSION>\n  --allow-session-loss  Permit an incompatible supervisor upgrade to remove sessions\n",
        ),
        Component::Termrelay => output.push_str(
            "\ntermrelay options:\n  --web | --no-web\n  --listen <HOST:PORT> | --public\n  --setup-token-file <PATH>\n  --daemon-registry <PATH>\n  --tls-cert <PATH> --tls-key <PATH>\n",
        ),
        Component::Termctl => {}
    }
    output
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Architecture {
    Amd64,
    Arm64,
}

impl Architecture {
    fn detect(os: &str, architecture: &str) -> Result<Self, UpgradeError> {
        match (os, architecture) {
            ("linux", "x86_64") | ("linux", "amd64") => Ok(Self::Amd64),
            ("linux", "aarch64") | ("linux", "arm64") => Ok(Self::Arm64),
            _ => Err(UpgradeError::UnsupportedPlatform {
                os: os.to_owned(),
                architecture: architecture.to_owned(),
            }),
        }
    }

    const fn asset_suffix(self) -> &'static str {
        match self {
            Self::Amd64 => "amd64",
            Self::Arm64 => "arm64",
        }
    }
}

#[derive(Debug)]
pub enum UpgradeError {
    UnknownArgument(String),
    NonInteractive,
    UnsupportedPlatform { os: String, architecture: String },
    InvalidRepository,
    InvalidCurrentVersion(String),
    InvalidLatestVersion(String),
    HttpClient(reqwest::Error),
    LatestRequest(reqwest::Error),
    LatestHttpStatus(u16),
    LatestResponseTooLarge,
    LatestRead(io::Error),
    LatestDecode(serde_json::Error),
    MissingAsset(String),
    DuplicateAsset(String),
    InvalidAssetUrl(String),
    InvalidAssetDigest(String),
    AssetTooLarge(String),
    RootRequired(String),
    DownloadRequest(reqwest::Error),
    DownloadHttpStatus(u16),
    DownloadRead(io::Error),
    DownloadTooLarge(String),
    TemporaryFile(io::Error),
    DigestMismatch(String),
    CandidatePermissions(io::Error),
    CandidateSpawn(io::Error),
    CandidateVersionFailed(String),
    CandidateIdentity { expected: String, actual: String },
    InstallerSpawn(io::Error),
    InstallerFailed(String),
    Input(io::Error),
    Output(io::Error),
}

impl fmt::Display for UpgradeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownArgument(argument) => write!(formatter, "unknown upgrade argument: {argument}"),
            Self::NonInteractive => formatter.write_str(
                "upgrade confirmation requires a TTY; rerun with --yes for non-interactive execution",
            ),
            Self::UnsupportedPlatform { os, architecture } => write!(
                formatter,
                "upgrade supports only Linux x86_64/amd64 and aarch64/arm64; detected {os}/{architecture}"
            ),
            Self::InvalidRepository => formatter.write_str(
                "TERMD_GITHUB_REPO must be in owner/repository form using letters, digits, '.', '_' or '-'",
            ),
            Self::InvalidCurrentVersion(version) => {
                write!(formatter, "current version is not valid semver: {version}")
            }
            Self::InvalidLatestVersion(version) => {
                write!(formatter, "latest release tag is not valid semver: {version}")
            }
            Self::HttpClient(source) => write!(
                formatter,
                "failed to initialize the HTTPS upgrade client: {}",
                reqwest_error_reason(source)
            ),
            Self::LatestRequest(source) => write!(
                formatter,
                "failed to query the latest GitHub release: {}",
                reqwest_error_reason(source)
            ),
            Self::LatestHttpStatus(status) => {
                write!(formatter, "GitHub latest release request returned HTTP {status}")
            }
            Self::LatestResponseTooLarge => {
                formatter.write_str("GitHub latest release response exceeded the safety limit")
            }
            Self::LatestRead(source) => write!(
                formatter,
                "failed to read the latest GitHub release response: {source}"
            ),
            Self::LatestDecode(source) => write!(
                formatter,
                "GitHub latest release response was invalid JSON: {source}"
            ),
            Self::MissingAsset(asset) => write!(formatter, "latest release is missing required asset {asset}"),
            Self::DuplicateAsset(asset) => {
                write!(formatter, "latest release contains duplicate asset {asset}")
            }
            Self::InvalidAssetUrl(asset) => write!(
                formatter,
                "latest release asset {asset} does not have a valid HTTPS download URL"
            ),
            Self::InvalidAssetDigest(asset) => write!(
                formatter,
                "latest release asset {asset} must provide digest sha256:<64 hex characters>"
            ),
            Self::AssetTooLarge(asset) => {
                write!(formatter, "latest release asset {asset} exceeds the safety limit")
            }
            Self::RootRequired(command) => write!(
                formatter,
                "upgrade requires root to replace the installed binary and manage any service; rerun: {command}"
            ),
            Self::DownloadRequest(source) => write!(
                formatter,
                "failed to download the release candidate: {}",
                reqwest_error_reason(source)
            ),
            Self::DownloadHttpStatus(status) => {
                write!(formatter, "release candidate download returned HTTP {status}")
            }
            Self::DownloadRead(source) => {
                write!(formatter, "failed while streaming the release candidate: {source}")
            }
            Self::DownloadTooLarge(asset) => {
                write!(formatter, "downloaded release asset {asset} exceeded the safety limit")
            }
            Self::TemporaryFile(source) => write!(
                formatter,
                "failed to create or write the protected upgrade file: {source}"
            ),
            Self::DigestMismatch(asset) => {
                write!(formatter, "SHA-256 verification failed for release asset {asset}")
            }
            Self::CandidatePermissions(source) => write!(
                formatter,
                "failed to make the verified release candidate executable: {source}"
            ),
            Self::CandidateSpawn(source) => {
                write!(formatter, "failed to run the verified release candidate: {source}")
            }
            Self::CandidateVersionFailed(status) => write!(
                formatter,
                "verified release candidate --version failed with {status}"
            ),
            Self::CandidateIdentity { expected, actual } => write!(
                formatter,
                "release candidate identity mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::InstallerSpawn(source) => write!(
                formatter,
                "failed to start the verified candidate installer: {source}"
            ),
            Self::InstallerFailed(status) => write!(
                formatter,
                "verified candidate installer exited with {status}; see the managed installer output above"
            ),
            Self::Input(source) => write!(formatter, "failed to read upgrade confirmation: {source}"),
            Self::Output(source) => write!(formatter, "failed to write upgrade output: {source}"),
        }
    }
}

impl StdError for UpgradeError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::HttpClient(source)
            | Self::LatestRequest(source)
            | Self::DownloadRequest(source) => Some(source),
            Self::LatestRead(source)
            | Self::DownloadRead(source)
            | Self::TemporaryFile(source)
            | Self::CandidatePermissions(source)
            | Self::CandidateSpawn(source)
            | Self::InstallerSpawn(source)
            | Self::Input(source)
            | Self::Output(source) => Some(source),
            Self::LatestDecode(source) => Some(source),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GithubRelease {
    tag_name: String,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
    #[serde(default)]
    size: u64,
}

#[derive(Debug, Clone)]
struct ReleaseAsset {
    name: String,
    download_url: String,
    digest: [u8; 32],
    size: u64,
}

struct UpgradeCandidate {
    path: PathBuf,
    _temporary_file: Option<NamedTempFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CandidateInvocation {
    path: PathBuf,
    args: Vec<OsString>,
    install_prefix: PathBuf,
}

#[derive(Debug, Clone)]
struct UpgradeExecutionStatus {
    success: bool,
    description: String,
}

trait UpgradeRuntime {
    fn is_tty(&self) -> bool;
    fn is_root(&self) -> bool;
    fn platform(&self) -> (&str, &str);
    fn repository(&self) -> Result<String, UpgradeError>;
    fn current_executable(&self) -> PathBuf;
    fn configured_install_prefix(&self) -> Option<PathBuf>;
    fn write_output(&mut self, message: &str) -> Result<(), UpgradeError>;
    fn confirm(&mut self, prompt: &str) -> Result<bool, UpgradeError>;
    fn latest_release(
        &mut self,
        repository: &str,
        component: Component,
        current_version: &str,
    ) -> Result<GithubRelease, UpgradeError>;
    fn download(
        &mut self,
        component: Component,
        current_version: &str,
        asset: &ReleaseAsset,
    ) -> Result<UpgradeCandidate, UpgradeError>;
    fn candidate_version(
        &mut self,
        candidate: &UpgradeCandidate,
    ) -> Result<UpgradeExecutionStatus, UpgradeError>;
    fn install_candidate(
        &mut self,
        invocation: &CandidateInvocation,
    ) -> Result<UpgradeExecutionStatus, UpgradeError>;
}

struct SystemUpgradeRuntime;

impl UpgradeRuntime for SystemUpgradeRuntime {
    fn is_tty(&self) -> bool {
        io::stdin().is_terminal() && io::stderr().is_terminal()
    }

    fn is_root(&self) -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    fn platform(&self) -> (&str, &str) {
        (std::env::consts::OS, std::env::consts::ARCH)
    }

    fn repository(&self) -> Result<String, UpgradeError> {
        match std::env::var("TERMD_GITHUB_REPO") {
            Ok(repository) => Ok(repository),
            Err(std::env::VarError::NotPresent) => Ok(DEFAULT_GITHUB_REPO.to_owned()),
            Err(std::env::VarError::NotUnicode(_)) => Err(UpgradeError::InvalidRepository),
        }
    }

    fn current_executable(&self) -> PathBuf {
        std::env::current_exe().unwrap_or_default()
    }

    fn configured_install_prefix(&self) -> Option<PathBuf> {
        std::env::var_os("TERMD_INSTALL_PREFIX")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }

    fn write_output(&mut self, message: &str) -> Result<(), UpgradeError> {
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(message.as_bytes())
            .and_then(|_| stdout.flush())
            .map_err(UpgradeError::Output)
    }

    fn confirm(&mut self, prompt: &str) -> Result<bool, UpgradeError> {
        let mut stderr = io::stderr().lock();
        stderr
            .write_all(prompt.as_bytes())
            .map_err(UpgradeError::Output)?;
        stderr.flush().map_err(UpgradeError::Output)?;
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .map_err(UpgradeError::Input)?;
        Ok(matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    }

    fn latest_release(
        &mut self,
        repository: &str,
        component: Component,
        current_version: &str,
    ) -> Result<GithubRelease, UpgradeError> {
        let client = upgrade_http_client(component, current_version)?;
        let url = format!("https://api.github.com/repos/{repository}/releases/latest");
        let response = client
            .get(url)
            .header(ACCEPT, GITHUB_API_ACCEPT)
            .send()
            .map_err(UpgradeError::LatestRequest)?;
        read_latest_release(response)
    }

    fn download(
        &mut self,
        component: Component,
        current_version: &str,
        asset: &ReleaseAsset,
    ) -> Result<UpgradeCandidate, UpgradeError> {
        let client = upgrade_http_client(component, current_version)?;
        let response = client
            .get(&asset.download_url)
            .send()
            .map_err(UpgradeError::DownloadRequest)?;
        download_candidate(response, asset)
    }

    fn candidate_version(
        &mut self,
        candidate: &UpgradeCandidate,
    ) -> Result<UpgradeExecutionStatus, UpgradeError> {
        let output = Command::new(&candidate.path)
            .arg("--version")
            .output()
            .map_err(UpgradeError::CandidateSpawn)?;
        Ok(UpgradeExecutionStatus {
            success: output.status.success(),
            description: if output.status.success() {
                String::from_utf8_lossy(&output.stdout).trim().to_owned()
            } else {
                output.status.to_string()
            },
        })
    }

    fn install_candidate(
        &mut self,
        invocation: &CandidateInvocation,
    ) -> Result<UpgradeExecutionStatus, UpgradeError> {
        let status = Command::new(&invocation.path)
            .args(&invocation.args)
            .env("TERMD_INSTALL_PREFIX", &invocation.install_prefix)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(UpgradeError::InstallerSpawn)?;
        Ok(UpgradeExecutionStatus {
            success: status.success(),
            description: status.to_string(),
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct ParsedUpgradeOptions {
    yes: bool,
    help: bool,
    allow_session_loss: bool,
}

impl ParsedUpgradeOptions {
    fn parse(component: Component, args: Vec<OsString>) -> Result<Self, UpgradeError> {
        let mut parsed = Self {
            yes: false,
            help: false,
            allow_session_loss: false,
        };
        for argument in args {
            let raw_argument = argument
                .to_str()
                .ok_or_else(|| UpgradeError::UnknownArgument("<non-UTF-8>".to_owned()))?;
            let (flag, has_inline_value) = match raw_argument.split_once('=') {
                Some((flag, _)) => (flag, true),
                None => (raw_argument, false),
            };
            match flag {
                "-h" | "--help" if !has_inline_value => parsed.help = true,
                "--yes" if !has_inline_value => parsed.yes = true,
                "--allow-session-loss" if !has_inline_value && component == Component::Termd => {
                    parsed.allow_session_loss = true;
                }
                _ => {
                    return Err(UpgradeError::UnknownArgument(safe_unknown_argument(flag)));
                }
            }
        }
        Ok(parsed)
    }
}

fn run_upgrade_with(
    component: Component,
    options: UpgradeOptions,
    runtime: &mut dyn UpgradeRuntime,
) -> Result<(), UpgradeError> {
    let parsed = ParsedUpgradeOptions::parse(component, options.args)?;
    if parsed.help {
        return runtime.write_output(&upgrade_help_text(component));
    }

    let (os, platform_architecture) = runtime.platform();
    let architecture = Architecture::detect(os, platform_architecture)?;
    let current_version = Version::parse(options.version)
        .map_err(|_| UpgradeError::InvalidCurrentVersion(options.version.to_owned()))?;
    let repository = runtime.repository()?;
    validate_repository(&repository)?;
    runtime.write_output(&format!(
        "Checking GitHub {repository} for the latest {component} release...\n"
    ))?;
    let release = runtime.latest_release(&repository, component, options.version)?;
    let latest_version = parse_release_version(&release.tag_name)?;
    if latest_version <= current_version {
        return runtime.write_output(&format!(
            "{component} is up to date (current {}, latest {}).\n",
            current_version, latest_version
        ));
    }

    let asset_name = component.release_asset(architecture);
    let asset = select_release_asset(&release, &asset_name)?;
    runtime.write_output(&format!(
        "Upgrade available:\n  component: {component}\n  current: {current_version}\n  latest: {latest_version}\n  asset: {}\n",
        asset.name
    ))?;

    if !parsed.yes {
        if !runtime.is_tty() {
            return Err(UpgradeError::NonInteractive);
        }
        let prompt = if component == Component::Termctl {
            "Download, verify and install now? [y/N] "
        } else {
            "Download, verify, install and restart now? [y/N] "
        };
        if !runtime.confirm(prompt)? {
            return runtime.write_output("Upgrade cancelled; no files or services were changed.\n");
        }
    }

    if !runtime.is_root() {
        let mut command = format!("sudo {component} upgrade");
        if parsed.yes {
            command.push_str(" --yes");
        }
        if parsed.allow_session_loss {
            command.push_str(" --allow-session-loss");
        }
        return Err(UpgradeError::RootRequired(command));
    }

    runtime.write_output(&format!("Downloading and verifying {}...\n", asset.name))?;
    let candidate = runtime.download(component, options.version, &asset)?;
    let expected_identity = format!("{component} {latest_version}");
    let candidate_status = runtime.candidate_version(&candidate)?;
    if !candidate_status.success {
        return Err(UpgradeError::CandidateVersionFailed(
            candidate_status.description,
        ));
    }
    if candidate_status.description != expected_identity {
        return Err(UpgradeError::CandidateIdentity {
            expected: expected_identity,
            actual: candidate_status.description,
        });
    }

    let install_prefix = resolve_upgrade_prefix(
        runtime.configured_install_prefix(),
        &runtime.current_executable(),
        component,
    );
    let mut args = vec![OsString::from("install"), OsString::from("--yes")];
    if parsed.allow_session_loss {
        args.push(OsString::from("--allow-session-loss"));
    }
    let invocation = CandidateInvocation {
        path: candidate.path.clone(),
        args,
        install_prefix,
    };
    let status = runtime.install_candidate(&invocation)?;
    if !status.success {
        return Err(UpgradeError::InstallerFailed(status.description));
    }
    runtime.write_output(&format!("Upgraded {component} to {latest_version}.\n"))
}

fn upgrade_help_text(component: Component) -> String {
    let mut output = format!(
        "Usage: {component} upgrade [OPTIONS]\n\nChecks the latest yiiilin/termd GitHub release, verifies the selected Linux binary\nwith its release asset SHA-256 digest, then uses that binary's managed installer.\nExisting configuration and state are preserved."
    );
    if component == Component::Termctl {
        output.push_str(" termctl has no managed service to restart.\n");
    } else {
        output.push_str(" The managed service is restarted after replacement.\n");
    }
    output.push_str(
        "\nOptions:\n  --yes      Confirm the normal upgrade without prompting\n  -h, --help Show this help\n",
    );
    if component == Component::Termd {
        output.push_str(
            "  --allow-session-loss  Separately authorize deleting sessions only if supervisor compatibility changed\n\nWithout --allow-session-loss, an incompatible supervisor upgrade still requires a\nsecond interactive confirmation even when --yes is present.\n",
        );
    }
    output.push_str(
        "\nEnvironment:\n  TERMD_GITHUB_REPO    Override owner/repository for forks or tests\n  TERMD_INSTALL_PREFIX Override the managed installation prefix\n  http_proxy, https_proxy, all_proxy, no_proxy and uppercase variants are honored\n",
    );
    output
}

fn validate_repository(repository: &str) -> Result<(), UpgradeError> {
    let mut components = repository.split('/');
    let owner = components.next().unwrap_or_default();
    let name = components.next().unwrap_or_default();
    if components.next().is_some()
        || owner.is_empty()
        || name.is_empty()
        || !owner.chars().all(valid_repository_character)
        || !name.chars().all(valid_repository_character)
    {
        return Err(UpgradeError::InvalidRepository);
    }
    Ok(())
}

fn valid_repository_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
}

fn parse_release_version(tag: &str) -> Result<Version, UpgradeError> {
    let version = tag.strip_prefix('v').unwrap_or(tag);
    Version::parse(version).map_err(|_| UpgradeError::InvalidLatestVersion(tag.to_owned()))
}

fn select_release_asset(
    release: &GithubRelease,
    expected_name: &str,
) -> Result<ReleaseAsset, UpgradeError> {
    let matches = release
        .assets
        .iter()
        .filter(|asset| asset.name == expected_name)
        .collect::<Vec<_>>();
    let asset = match matches.as_slice() {
        [] => return Err(UpgradeError::MissingAsset(expected_name.to_owned())),
        [asset] => *asset,
        _ => return Err(UpgradeError::DuplicateAsset(expected_name.to_owned())),
    };
    let url = reqwest::Url::parse(&asset.browser_download_url)
        .map_err(|_| UpgradeError::InvalidAssetUrl(expected_name.to_owned()))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none()
    {
        return Err(UpgradeError::InvalidAssetUrl(expected_name.to_owned()));
    }
    if asset.size > MAX_BINARY_BYTES {
        return Err(UpgradeError::AssetTooLarge(expected_name.to_owned()));
    }
    let digest = parse_sha256_digest(asset.digest.as_deref())
        .ok_or_else(|| UpgradeError::InvalidAssetDigest(expected_name.to_owned()))?;
    Ok(ReleaseAsset {
        name: asset.name.clone(),
        download_url: asset.browser_download_url.clone(),
        digest,
        size: asset.size,
    })
}

fn parse_sha256_digest(digest: Option<&str>) -> Option<[u8; 32]> {
    let hexadecimal = digest?.strip_prefix("sha256:")?;
    if hexadecimal.len() != 64 || !hexadecimal.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut output = [0_u8; 32];
    for (index, pair) in hexadecimal.as_bytes().chunks_exact(2).enumerate() {
        let pair = std::str::from_utf8(pair).ok()?;
        output[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(output)
}

fn reqwest_error_reason(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "request timed out"
    } else if error.is_connect() {
        "connection failed"
    } else if error.is_redirect() {
        "redirect was rejected"
    } else if error.is_decode() {
        "response decoding failed"
    } else if error.is_body() {
        "response body failed"
    } else if error.is_builder() {
        "request configuration was invalid"
    } else {
        "request failed"
    }
}

fn upgrade_http_client(
    component: Component,
    current_version: &str,
) -> Result<Client, UpgradeError> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let user_agent = HeaderValue::from_str(&format!("{component}/{current_version}"))
        .unwrap_or_else(|_| HeaderValue::from_static("termd-upgrade"));
    Client::builder()
        .user_agent(user_agent)
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(600))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() < 10
                && attempt.url().scheme() == "https"
                && attempt.url().host_str().is_some()
                && attempt.url().username().is_empty()
                && attempt.url().password().is_none()
            {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .map_err(UpgradeError::HttpClient)
}

fn read_latest_release(response: Response) -> Result<GithubRelease, UpgradeError> {
    let status = response.status();
    if !status.is_success() {
        return Err(UpgradeError::LatestHttpStatus(status.as_u16()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RELEASE_RESPONSE_BYTES)
    {
        return Err(UpgradeError::LatestResponseTooLarge);
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_RELEASE_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(UpgradeError::LatestRead)?;
    if bytes.len() as u64 > MAX_RELEASE_RESPONSE_BYTES {
        return Err(UpgradeError::LatestResponseTooLarge);
    }
    serde_json::from_slice(&bytes).map_err(UpgradeError::LatestDecode)
}

fn download_candidate(
    response: Response,
    asset: &ReleaseAsset,
) -> Result<UpgradeCandidate, UpgradeError> {
    let status = response.status();
    if !status.is_success() {
        return Err(UpgradeError::DownloadHttpStatus(status.as_u16()));
    }
    let content_length = response.content_length();
    if content_length.is_some_and(|length| length > MAX_BINARY_BYTES) {
        return Err(UpgradeError::DownloadTooLarge(asset.name.clone()));
    }
    stage_candidate(response, content_length, asset)
}

fn stage_candidate(
    mut reader: impl Read,
    content_length: Option<u64>,
    asset: &ReleaseAsset,
) -> Result<UpgradeCandidate, UpgradeError> {
    if content_length.is_some_and(|length| length > MAX_BINARY_BYTES) {
        return Err(UpgradeError::DownloadTooLarge(asset.name.clone()));
    }
    let mut temporary_file = NamedTempFile::new().map_err(UpgradeError::TemporaryFile)?;
    temporary_file
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(UpgradeError::TemporaryFile)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(UpgradeError::DownloadRead)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| UpgradeError::DownloadTooLarge(asset.name.clone()))?;
        if total > MAX_BINARY_BYTES {
            return Err(UpgradeError::DownloadTooLarge(asset.name.clone()));
        }
        digest.update(&buffer[..read]);
        temporary_file
            .write_all(&buffer[..read])
            .map_err(UpgradeError::TemporaryFile)?;
    }
    if asset.size != 0 && total != asset.size {
        return Err(UpgradeError::DigestMismatch(asset.name.clone()));
    }
    let actual: [u8; 32] = digest.finalize().into();
    if actual != asset.digest {
        return Err(UpgradeError::DigestMismatch(asset.name.clone()));
    }
    temporary_file
        .flush()
        .and_then(|_| temporary_file.as_file().sync_all())
        .map_err(UpgradeError::TemporaryFile)?;
    temporary_file
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o700))
        .map_err(UpgradeError::CandidatePermissions)?;
    Ok(UpgradeCandidate {
        path: temporary_file.path().to_owned(),
        _temporary_file: Some(temporary_file),
    })
}

fn resolve_upgrade_prefix(
    configured_prefix: Option<PathBuf>,
    current_executable: &Path,
    component: Component,
) -> PathBuf {
    if let Some(prefix) = configured_prefix {
        return prefix;
    }
    if current_executable.file_name() == Some(OsStr::new(component.binary_name()))
        && current_executable.parent().and_then(Path::file_name) == Some(OsStr::new("bin"))
        && let Some(prefix) = current_executable.parent().and_then(Path::parent)
    {
        return prefix.to_owned();
    }
    PathBuf::from("/usr/local")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct FakeRuntime {
        tty: bool,
        confirmation: bool,
        output: String,
        execute_calls: usize,
        invocation_args: Vec<OsString>,
        invocation_env: Vec<(&'static str, OsString)>,
        secret_input: OsString,
        secret_reads: usize,
        managed_install_exists: bool,
        sudo_user: Option<OsString>,
        input_lines: VecDeque<String>,
        line_reads: usize,
        status: ExecutionStatus,
    }

    impl Default for FakeRuntime {
        fn default() -> Self {
            Self {
                tty: true,
                confirmation: true,
                output: String::new(),
                execute_calls: 0,
                invocation_args: Vec::new(),
                invocation_env: Vec::new(),
                secret_input: OsString::from("prompted-relay-setup-token"),
                secret_reads: 0,
                managed_install_exists: true,
                sudo_user: None,
                input_lines: VecDeque::new(),
                line_reads: 0,
                status: ExecutionStatus {
                    success: true,
                    description: "exit status: 0".to_owned(),
                },
            }
        }
    }

    impl Runtime for FakeRuntime {
        fn is_tty(&self) -> bool {
            self.tty
        }

        fn open_self_binary(&mut self) -> Result<SelfBinary, Error> {
            Ok(SelfBinary {
                display_path: PathBuf::from("/tmp/release/termd-linux-amd64"),
                installer_path: PathBuf::from("/proc/4242/fd/9"),
                _open_file: None,
            })
        }

        fn install_prefix(&self) -> PathBuf {
            PathBuf::from("/tmp/prefix")
        }

        fn managed_install_exists(&self, _component: Component) -> Result<bool, Error> {
            Ok(self.managed_install_exists)
        }

        fn sudo_user(&self) -> Option<OsString> {
            self.sudo_user.clone()
        }

        fn write_output(&mut self, message: &str) -> Result<(), Error> {
            self.output.push_str(message);
            Ok(())
        }

        fn read_line(&mut self, _prompt: &str) -> Result<String, Error> {
            self.line_reads += 1;
            Ok(self.input_lines.pop_front().unwrap_or_default())
        }

        fn confirm(&mut self, _prompt: &str) -> Result<bool, Error> {
            Ok(self.confirmation)
        }

        fn read_secret(&mut self, _prompt: &str) -> Result<OsString, Error> {
            self.secret_reads += 1;
            Ok(self.secret_input.clone())
        }

        fn execute(&mut self, invocation: &Invocation) -> Result<ExecutionStatus, Error> {
            self.execute_calls += 1;
            self.invocation_args = invocation.args.clone();
            self.invocation_env = invocation.env.clone();
            Ok(ExecutionStatus {
                success: self.status.success,
                description: self.status.description.clone(),
            })
        }
    }

    #[test]
    fn parses_install_argv_and_builds_embedded_invocation() {
        let options = Options::from_subcommand(
            "1.2.3",
            Some("supervisor-v1"),
            ["install", "--web", "--listen", "127.0.0.1:8765", "--yes"],
        )
        .unwrap();
        let mut runtime = FakeRuntime::default();

        run_with(Component::Termd, options, &mut runtime).unwrap();

        assert_eq!(
            runtime.invocation_args,
            ["--web", "--listen", "127.0.0.1:8765"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        assert!(runtime.invocation_env.contains(&(
            "TERMD_INSTALL_SELF_BINARY",
            OsString::from("/proc/4242/fd/9")
        )));
        assert!(runtime.invocation_env.contains(&(
            "TERMD_REQUIRED_SUPERVISOR_VERSION",
            OsString::from("supervisor-v1")
        )));
        assert!(
            !runtime
                .invocation_env
                .iter()
                .any(|(name, _)| *name == "TERMD_INSTALL_ASSUME_YES")
        );
        assert!(
            runtime
                .invocation_env
                .contains(&("TERMD_INSTALL_PREFIX", OsString::from("/tmp/prefix")))
        );
        let helper_nonce = runtime
            .invocation_env
            .iter()
            .find_map(|(name, value)| (*name == INTERNAL_HELPER_NONCE_ENV).then_some(value))
            .expect("service installers must receive a helper nonce");
        assert!(valid_internal_helper_nonce(helper_nonce));
    }

    #[test]
    fn internal_helper_protocol_rejects_malformed_capabilities() {
        assert!(valid_internal_helper_nonce(OsStr::new(&"a".repeat(64))));
        for invalid in [
            "a".repeat(63),
            "A".repeat(64),
            "g".repeat(64),
            "0".repeat(65),
        ] {
            assert!(!valid_internal_helper_nonce(OsStr::new(&invalid)));
        }
        assert_eq!(
            pinned_self_binary_pid(Path::new("/proc/4242/fd/9")),
            Some(4242)
        );
        for invalid in [
            "/proc/0/fd/9",
            "/proc/4242/fd/",
            "/proc/4242/fd/9/extra",
            "/tmp/4242/fd/9",
        ] {
            assert_eq!(pinned_self_binary_pid(Path::new(invalid)), None);
        }
        assert!(process_is_ancestor(unsafe { libc::getppid() } as u32).unwrap());
    }

    #[test]
    fn termctl_does_not_receive_unused_helper_capability() {
        let options = Options::Install(InstallOptions::new("1.2.3", None, ["--yes"]));
        let mut runtime = FakeRuntime::default();

        run_with(Component::Termctl, options, &mut runtime).unwrap();

        assert!(
            runtime
                .invocation_env
                .iter()
                .all(|(name, _)| *name != INTERNAL_HELPER_NONCE_ENV)
        );
    }

    #[test]
    fn fresh_termrelay_without_settings_enables_loopback_web_defaults() {
        let options = Options::Install(InstallOptions::new("1.2.3", None, ["--yes"]));
        let mut runtime = FakeRuntime {
            managed_install_exists: false,
            ..FakeRuntime::default()
        };

        run_with(Component::Termrelay, options, &mut runtime).unwrap();

        assert_eq!(
            runtime.invocation_args,
            ["--web", "--listen", "127.0.0.1:8080"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn existing_service_install_without_settings_preserves_configuration() {
        for component in [Component::Termd, Component::Termrelay] {
            let options = Options::Install(InstallOptions::new("1.2.3", None, ["--yes"]));
            let mut runtime = FakeRuntime {
                managed_install_exists: true,
                ..FakeRuntime::default()
            };

            run_with(component, options, &mut runtime).unwrap();

            assert!(runtime.invocation_args.is_empty());
            assert_eq!(runtime.line_reads, 0);
        }
    }

    #[test]
    fn fresh_termd_guide_uses_confirmed_sudo_user_and_local_mode() {
        let options = Options::Install(InstallOptions::new("1.2.3", None, [] as [&str; 0]));
        let mut runtime = FakeRuntime {
            managed_install_exists: false,
            sudo_user: Some(OsString::from("alice")),
            input_lines: VecDeque::from([String::new(), "n".to_owned()]),
            ..FakeRuntime::default()
        };

        run_with(Component::Termd, options, &mut runtime).unwrap();

        assert_eq!(
            runtime.invocation_args,
            ["--web", "--listen", "127.0.0.1:8765", "--user", "alice",]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(runtime.secret_reads, 0);
        assert_eq!(runtime.line_reads, 2);
    }

    #[test]
    fn fresh_termd_guide_accepts_relay_url_and_reads_token_securely() {
        let options = Options::Install(InstallOptions::new("1.2.3", None, [] as [&str; 0]));
        let mut runtime = FakeRuntime {
            managed_install_exists: false,
            input_lines: VecDeque::from([
                "bob".to_owned(),
                "y".to_owned(),
                "wss://relay.example/ws".to_owned(),
            ]),
            ..FakeRuntime::default()
        };

        run_with(Component::Termd, options, &mut runtime).unwrap();

        assert!(
            runtime
                .invocation_args
                .ends_with(&[OsString::from("--user"), OsString::from("bob")])
        );
        assert!(runtime.invocation_env.contains(&(
            "TERMD_INSTALL_ARG_RELAY_URL",
            OsString::from("wss://relay.example/ws")
        )));
        assert_eq!(runtime.secret_reads, 1);
        assert!(
            !runtime
                .invocation_args
                .contains(&OsString::from("prompted-relay-setup-token"))
        );
    }

    #[test]
    fn sensitive_arguments_use_controlled_environment_not_child_argv() {
        let relay = "wss://relay-user:relay-password@relay.example/ws";
        let daemon_token = "/tmp/private/daemon-token";
        let setup_token = "/tmp/private/setup-token";
        let proxy = "http://proxy-user:proxy-password@proxy.example";
        let tls_key = "/tmp/private/tls-key.pem";
        let options = Options::Install(InstallOptions::new(
            "1.2.3",
            None,
            [
                "--relay",
                relay,
                "--relay-daemon-token-file",
                daemon_token,
                "--relay-setup-token-file",
                setup_token,
                "--proxy",
                proxy,
                "--tls-key",
                tls_key,
                "--listen",
                "127.0.0.1:8765",
                "--yes",
            ],
        ));
        let mut runtime = FakeRuntime::default();

        run_with(Component::Termd, options, &mut runtime).unwrap();

        assert_eq!(
            runtime.invocation_args,
            ["--listen", "127.0.0.1:8765"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        for (name, value) in [
            ("TERMD_INSTALL_ARG_RELAY_URL", relay),
            ("TERMD_INSTALL_ARG_RELAY_DAEMON_TOKEN_FILE", daemon_token),
            ("TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN_FILE", setup_token),
            ("TERMD_INSTALL_ARG_PROXY", proxy),
            ("TERMD_INSTALL_ARG_TLS_KEY", tls_key),
        ] {
            assert!(
                runtime
                    .invocation_env
                    .contains(&(name, OsString::from(value)))
            );
            assert!(!runtime.output.contains(value));
        }
    }

    #[test]
    fn inline_sensitive_values_are_supported_without_disclosure() {
        let relay = "wss://inline-user:inline-relay-secret@relay.example/ws";
        let proxy = "http://inline-user:inline-proxy-secret@proxy.example";
        let setup_token = "/tmp/inline-private/setup-token";
        let tls_key = "/tmp/inline-private/tls-key.pem";
        let options = Options::Install(InstallOptions::new(
            "1.2.3",
            None,
            [
                format!("--relay={relay}"),
                format!("--proxy={proxy}"),
                format!("--relay-setup-token-file={setup_token}"),
                format!("--tls-key={tls_key}"),
                "--yes".to_owned(),
            ],
        ));
        let mut runtime = FakeRuntime::default();

        run_with(Component::Termd, options, &mut runtime).unwrap();

        assert!(runtime.invocation_args.is_empty());
        for (name, value) in [
            ("TERMD_INSTALL_ARG_RELAY_URL", relay),
            ("TERMD_INSTALL_ARG_PROXY", proxy),
            ("TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN_FILE", setup_token),
            ("TERMD_INSTALL_ARG_TLS_KEY", tls_key),
        ] {
            assert!(
                runtime
                    .invocation_env
                    .contains(&(name, OsString::from(value)))
            );
            assert!(!runtime.output.contains(value));
        }
    }

    #[test]
    fn direct_relay_token_uses_environment_without_disclosure() {
        let relay_token = "direct-relay-setup-secret";
        let options = Options::Install(InstallOptions::new(
            "1.2.3",
            None,
            [
                "--relay",
                "wss://relay.example",
                "--relay-token",
                relay_token,
                "--yes",
            ],
        ));
        let mut runtime = FakeRuntime::default();

        run_with(Component::Termd, options, &mut runtime).unwrap();

        assert_eq!(runtime.secret_reads, 0);
        assert!(runtime.invocation_args.is_empty());
        assert!(runtime.invocation_env.contains(&(
            "TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN",
            OsString::from(relay_token)
        )));
        assert!(!runtime.output.contains(relay_token));
    }

    #[test]
    fn relay_token_sources_conflict_without_disclosing_secret() {
        let relay_token = "conflicting-relay-setup-secret";
        let options = Options::Install(InstallOptions::new(
            "1.2.3",
            None,
            [
                "--relay-token",
                relay_token,
                "--relay-setup-token-file",
                "/tmp/relay-setup-token",
            ],
        ));

        let error = match ParsedOptions::parse(Component::Termd, options) {
            Ok(_) => panic!("conflicting relay token sources were accepted"),
            Err(error) => error,
        };
        let rendered = format!("{error:?}\n{error}");

        assert!(matches!(
            error,
            Error::ConflictingArguments("--relay-token", "--relay-setup-token-file")
        ));
        assert!(!rendered.contains(relay_token));
    }

    #[test]
    fn explicit_relay_prompts_securely_only_for_live_tty_install() {
        let dry_run_options = Options::Install(InstallOptions::new(
            "1.2.3",
            None,
            ["--relay", "wss://relay.example", "--dry-run"],
        ));
        let mut dry_run = FakeRuntime {
            tty: false,
            ..FakeRuntime::default()
        };
        run_with(Component::Termd, dry_run_options, &mut dry_run).unwrap();
        assert_eq!(dry_run.secret_reads, 0);
        assert_eq!(dry_run.execute_calls, 1);
        assert!(
            dry_run
                .output
                .contains("relay setup token: will prompt securely during installation")
        );

        let live_options = Options::Install(InstallOptions::new(
            "1.2.3",
            None,
            ["--relay", "wss://relay.example", "--yes"],
        ));
        let mut live = FakeRuntime::default();
        run_with(Component::Termd, live_options, &mut live).unwrap();
        assert_eq!(live.secret_reads, 1);
        assert_eq!(
            live.invocation_env
                .iter()
                .filter(|(name, value)| {
                    *name == "TERMD_INSTALL_ARG_RELAY_SETUP_TOKEN"
                        && value == OsStr::new("prompted-relay-setup-token")
                })
                .count(),
            1
        );
        assert!(!live.output.contains("prompted-relay-setup-token"));
    }

    #[test]
    fn explicit_relay_without_token_has_actionable_noninteractive_error() {
        let options = Options::Install(InstallOptions::new(
            "1.2.3",
            None,
            ["--relay", "wss://relay.example", "--yes"],
        ));
        let mut runtime = FakeRuntime {
            tty: false,
            ..FakeRuntime::default()
        };

        let error = run_with(Component::Termd, options, &mut runtime).unwrap_err();

        assert!(matches!(error, Error::RelayTokenRequired));
        assert!(format!("{error}").contains("--relay-token"));
        assert!(format!("{error}").contains("--relay-setup-token-file"));
        assert_eq!(runtime.secret_reads, 0);
        assert_eq!(runtime.execute_calls, 0);
    }

    #[test]
    fn upgrade_without_explicit_relay_does_not_require_setup_token() {
        let options = Options::Install(InstallOptions::new("1.2.3", None, ["--yes"]));
        let mut runtime = FakeRuntime {
            tty: false,
            ..FakeRuntime::default()
        };

        run_with(Component::Termd, options, &mut runtime).unwrap();

        assert_eq!(runtime.secret_reads, 0);
        assert_eq!(runtime.execute_calls, 1);
    }

    #[test]
    fn removed_open_relay_flag_is_unknown_for_self_installers() {
        for component in [Component::Termd, Component::Termrelay] {
            let options =
                Options::Install(InstallOptions::new("1.2.3", None, ["--allow-open-relay"]));
            assert!(matches!(
                ParsedOptions::parse(component, options),
                Err(Error::UnknownArgument(argument)) if argument == "--allow-open-relay"
            ));
            assert!(!help_text(component, Action::Install).contains("--allow-open-relay"));
        }
    }

    #[test]
    fn duplicate_relay_aliases_are_rejected_without_disclosing_values() {
        let first_secret = "wss://first-secret@relay.example/ws";
        let second_secret = "wss://second-secret@relay.example/ws";
        let options = Options::Install(InstallOptions::new(
            "1.2.3",
            None,
            [
                format!("--relay={first_secret}"),
                format!("--relay-url={second_secret}"),
            ],
        ));

        let error = match ParsedOptions::parse(Component::Termd, options) {
            Err(error) => error,
            Ok(_) => panic!("duplicate relay arguments were accepted"),
        };
        let rendered = format!("{error:?}\n{error}");

        assert!(matches!(error, Error::DuplicateArgument("--relay")));
        assert!(!rendered.contains(first_secret));
        assert!(!rendered.contains(second_secret));
    }

    #[test]
    fn unknown_inline_and_bare_arguments_are_fully_redacted() {
        let inline_secret = "reviewer-inline-secret";
        let bare_secret = "reviewer-bare-secret";
        let inline = Options::Install(InstallOptions::new(
            "1.2.3",
            None,
            [format!("--unknown={inline_secret}")],
        ));
        let bare = Options::Install(InstallOptions::new("1.2.3", None, [bare_secret.to_owned()]));

        let inline_error = match ParsedOptions::parse(Component::Termd, inline) {
            Err(error) => error,
            Ok(_) => panic!("unknown inline argument was accepted"),
        };
        let bare_error = match ParsedOptions::parse(Component::Termd, bare) {
            Err(error) => error,
            Ok(_) => panic!("unknown bare argument was accepted"),
        };
        let rendered = format!("{inline_error:?}\n{inline_error}\n{bare_error:?}\n{bare_error}");

        assert!(matches!(
            inline_error,
            Error::UnknownArgument(ref argument) if argument == "--unknown"
        ));
        assert!(matches!(
            bare_error,
            Error::UnknownArgument(ref argument) if argument == "<argument>"
        ));
        assert!(!rendered.contains(inline_secret));
        assert!(!rendered.contains(bare_secret));
    }

    #[test]
    fn relay_sensitive_arguments_use_their_controlled_environment() {
        let options = Options::Install(InstallOptions::new(
            "1.2.3",
            None,
            [
                "--setup-token-file",
                "/tmp/private/relay-setup-token",
                "--tls-key",
                "/tmp/private/relay-key.pem",
                "--yes",
            ],
        ));
        let mut runtime = FakeRuntime::default();

        run_with(Component::Termrelay, options, &mut runtime).unwrap();

        assert!(runtime.invocation_args.is_empty());
        assert!(runtime.invocation_env.contains(&(
            "TERMD_INSTALL_ARG_SETUP_TOKEN_FILE",
            OsString::from("/tmp/private/relay-setup-token")
        )));
        assert!(runtime.invocation_env.contains(&(
            "TERMD_INSTALL_ARG_TLS_KEY",
            OsString::from("/tmp/private/relay-key.pem")
        )));
        assert!(
            !runtime
                .invocation_args
                .contains(&OsString::from("--allow-session-loss"))
        );
    }

    #[test]
    fn yes_does_not_enable_daemon_session_loss_bypass() {
        let options = Options::Install(InstallOptions::new("1.2.3", None, ["--yes"]));
        let mut runtime = FakeRuntime::default();

        run_with(Component::Termd, options, &mut runtime).unwrap();

        assert!(
            !runtime
                .invocation_args
                .contains(&OsString::from("--allow-session-loss"))
        );
        assert!(!runtime.output.contains("session loss is authorized"));
    }

    #[test]
    fn explicit_session_loss_flag_is_forwarded_and_warned() {
        let options = Options::Install(InstallOptions::new(
            "1.2.3",
            None,
            ["--yes", "--allow-session-loss"],
        ));
        let mut runtime = FakeRuntime::default();

        run_with(Component::Termd, options, &mut runtime).unwrap();

        assert!(
            runtime
                .invocation_args
                .contains(&OsString::from("--allow-session-loss"))
        );
        assert!(
            runtime.output.contains(
                "WARNING: session loss is authorized if supervisor compatibility changes"
            )
        );
    }

    #[test]
    fn system_runtime_clears_hostile_shell_environment() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "terminstall-hostile-env-{}-{unique}",
            std::process::id()
        ));
        let malicious_bin = root.join("bin");
        let marker = root.join("marker");
        let bash_env = root.join("bash-env");
        let fake_uname = malicious_bin.join("uname");
        std::fs::create_dir_all(&malicious_bin).unwrap();
        std::fs::write(
            &bash_env,
            format!(
                "printf 'BASH_ENV_RAN\\n' >>'{}'\nprintf '%s\\n' \"$TERMD_HOSTILE_SECRET\" >&2\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::write(
            &fake_uname,
            "#!/bin/bash\nprintf 'MALICIOUS_PATH_RAN\\n' >>\"$TERMD_TEST_MARKER\"\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake_uname).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_uname, permissions).unwrap();

        let secret = "hostile-environment-secret-must-not-leak";
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "tests::system_runtime_hostile_environment_helper",
                "--nocapture",
            ])
            .env("TERMD_RUNTIME_HELPER", "1")
            .env("TERMD_RUNTIME_MARKER", &marker)
            .env("TERMD_HOSTILE_SECRET", secret)
            .env("BASH_ENV", &bash_env)
            .env("PATH", &malicious_bin)
            .env("SHELLOPTS", "xtrace")
            .output()
            .unwrap();

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "hostile environment helper failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert!(
            !stderr.contains(secret),
            "secret leaked to stderr: {stderr}"
        );
        let marker_contents = std::fs::read_to_string(&marker).unwrap();
        assert!(marker_contents.contains(&format!("PATH={INSTALLER_PATH}")));
        assert!(marker_contents.contains("LANG=C"));
        assert!(marker_contents.contains("LC_ALL=C"));
        assert!(!marker_contents.contains("BASH_ENV_RAN"));
        assert!(!marker_contents.contains("MALICIOUS_PATH_RAN"));
        let shell_flags = marker_contents
            .lines()
            .find_map(|line| line.strip_prefix("FLAGS="))
            .unwrap();
        assert!(
            !shell_flags.contains('x'),
            "xtrace was enabled: {shell_flags}"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "invoked by system_runtime_clears_hostile_shell_environment"]
    fn system_runtime_hostile_environment_helper() {
        if std::env::var_os("TERMD_RUNTIME_HELPER").is_none() {
            return;
        }
        let marker = std::env::var_os("TERMD_RUNTIME_MARKER").unwrap();
        let invocation = Invocation {
            component: Component::Termctl,
            script: r#"printf 'PATH=%s\nLANG=%s\nLC_ALL=%s\nFLAGS=%s\n' "$PATH" "$LANG" "$LC_ALL" "$-" >"$TERMD_TEST_MARKER"
uname -m >/dev/null
"#,
            args: Vec::new(),
            env: vec![("TERMD_TEST_MARKER", marker)],
        };

        let status = SystemRuntime.execute(&invocation).unwrap();
        assert!(
            status.success,
            "embedded bash failed: {}",
            status.description
        );
    }

    #[test]
    fn non_tty_requires_yes() {
        let options = Options::Install(InstallOptions::new("1.2.3", None, ["--web"]));
        let mut runtime = FakeRuntime {
            tty: false,
            ..FakeRuntime::default()
        };

        let error = run_with(Component::Termd, options, &mut runtime).unwrap_err();

        assert!(matches!(error, Error::NonInteractive));
        assert_eq!(runtime.execute_calls, 0);
    }

    #[test]
    fn dry_run_runs_only_helper_self_check_and_redacts_secret_files() {
        let secret_path = "/tmp/do-not-print/setup-token";
        let options = Options::Install(InstallOptions::new(
            "1.2.3",
            None,
            [
                "--relay-setup-token-file",
                secret_path,
                "--proxy",
                "http://user:password@proxy.example",
                "--dry-run",
            ],
        ));
        let mut runtime = FakeRuntime {
            tty: false,
            ..FakeRuntime::default()
        };

        run_with(Component::Termd, options, &mut runtime).unwrap();

        assert_eq!(runtime.execute_calls, 1);
        assert!(runtime.invocation_args.is_empty());
        assert!(
            runtime.invocation_env.iter().any(
                |(name, value)| *name == INTERNAL_HELPER_VERIFY_ENV && value == OsStr::new("1")
            )
        );
        assert!(
            !runtime
                .invocation_env
                .iter()
                .any(|(_, value)| value == OsStr::new(secret_path)
                    || value.to_string_lossy().contains("password"))
        );
        assert!(runtime.output.contains("Dry run"));
        assert!(
            runtime
                .output
                .contains("relay setup token file: configured")
        );
        assert!(runtime.output.contains("outbound proxy: configured"));
        assert!(!runtime.output.contains(secret_path));
        assert!(!runtime.output.contains("password"));
    }

    #[test]
    fn debug_output_does_not_disclose_secret_arguments() {
        let options = InstallOptions::new(
            "1.2.3",
            None,
            ["--relay-setup-token-file", "/tmp/secret-token"],
        );

        let debug = format!("{options:?}");

        assert!(!debug.contains("secret-token"));
        assert!(debug.contains("argument_count"));
    }

    #[test]
    fn child_failure_keeps_component_and_exit_status() {
        let options = Options::Install(InstallOptions::new("1.2.3", None, ["--yes"]));
        let mut runtime = FakeRuntime {
            status: ExecutionStatus {
                success: false,
                description: "exit status: 23".to_owned(),
            },
            ..FakeRuntime::default()
        };

        let error = run_with(Component::Termrelay, options, &mut runtime).unwrap_err();

        assert!(matches!(
            error,
            Error::ChildFailed {
                component: Component::Termrelay,
                ref status,
            } if status == "exit status: 23"
        ));
    }

    #[test]
    fn rejects_unknown_and_missing_component_arguments() {
        let unknown = Options::Install(InstallOptions::new("1.2.3", None, ["--bogus"]));
        let missing = Options::Install(InstallOptions::new("1.2.3", None, ["--listen"]));

        assert!(matches!(
            ParsedOptions::parse(Component::Termd, unknown),
            Err(Error::UnknownArgument(argument)) if argument == "--bogus"
        ));
        assert!(matches!(
            ParsedOptions::parse(Component::Termd, missing),
            Err(Error::MissingValue("--listen"))
        ));
    }

    #[test]
    fn termctl_purge_is_explicitly_rejected() {
        let options = Options::Uninstall(UninstallOptions::new("1.2.3", ["--purge"]));

        assert!(matches!(
            ParsedOptions::parse(Component::Termctl, options),
            Err(Error::PurgeUnsupported)
        ));
    }

    #[test]
    fn embedded_supervisor_version_is_non_empty() {
        assert!(!supervisor_version().is_empty());
        assert!(!supervisor_version().contains('\n'));
    }

    struct FakeUpgradeRuntime {
        tty: bool,
        root: bool,
        os: &'static str,
        architecture: &'static str,
        repository: String,
        current_executable: PathBuf,
        configured_prefix: Option<PathBuf>,
        confirmation: bool,
        output: String,
        latest_calls: usize,
        download_calls: usize,
        version_calls: usize,
        install_calls: usize,
        release: GithubRelease,
        download_fails_digest: bool,
        candidate_status: UpgradeExecutionStatus,
        installer_status: UpgradeExecutionStatus,
        invocation: Option<CandidateInvocation>,
    }

    impl Default for FakeUpgradeRuntime {
        fn default() -> Self {
            Self {
                tty: true,
                root: true,
                os: "linux",
                architecture: "x86_64",
                repository: DEFAULT_GITHUB_REPO.to_owned(),
                current_executable: PathBuf::from("/opt/termd/bin/termd"),
                configured_prefix: None,
                confirmation: true,
                output: String::new(),
                latest_calls: 0,
                download_calls: 0,
                version_calls: 0,
                install_calls: 0,
                release: fake_release("1.3.0", "termd-linux-amd64"),
                download_fails_digest: false,
                candidate_status: UpgradeExecutionStatus {
                    success: true,
                    description: "termd 1.3.0".to_owned(),
                },
                installer_status: UpgradeExecutionStatus {
                    success: true,
                    description: "exit status: 0".to_owned(),
                },
                invocation: None,
            }
        }
    }

    impl UpgradeRuntime for FakeUpgradeRuntime {
        fn is_tty(&self) -> bool {
            self.tty
        }

        fn is_root(&self) -> bool {
            self.root
        }

        fn platform(&self) -> (&str, &str) {
            (self.os, self.architecture)
        }

        fn repository(&self) -> Result<String, UpgradeError> {
            Ok(self.repository.clone())
        }

        fn current_executable(&self) -> PathBuf {
            self.current_executable.clone()
        }

        fn configured_install_prefix(&self) -> Option<PathBuf> {
            self.configured_prefix.clone()
        }

        fn write_output(&mut self, message: &str) -> Result<(), UpgradeError> {
            self.output.push_str(message);
            Ok(())
        }

        fn confirm(&mut self, _prompt: &str) -> Result<bool, UpgradeError> {
            Ok(self.confirmation)
        }

        fn latest_release(
            &mut self,
            _repository: &str,
            _component: Component,
            _current_version: &str,
        ) -> Result<GithubRelease, UpgradeError> {
            self.latest_calls += 1;
            Ok(self.release.clone())
        }

        fn download(
            &mut self,
            _component: Component,
            _current_version: &str,
            asset: &ReleaseAsset,
        ) -> Result<UpgradeCandidate, UpgradeError> {
            self.download_calls += 1;
            if self.download_fails_digest {
                return Err(UpgradeError::DigestMismatch(asset.name.clone()));
            }
            Ok(UpgradeCandidate {
                path: PathBuf::from("/tmp/verified-upgrade-candidate"),
                _temporary_file: None,
            })
        }

        fn candidate_version(
            &mut self,
            _candidate: &UpgradeCandidate,
        ) -> Result<UpgradeExecutionStatus, UpgradeError> {
            self.version_calls += 1;
            Ok(self.candidate_status.clone())
        }

        fn install_candidate(
            &mut self,
            invocation: &CandidateInvocation,
        ) -> Result<UpgradeExecutionStatus, UpgradeError> {
            self.install_calls += 1;
            self.invocation = Some(invocation.clone());
            Ok(self.installer_status.clone())
        }
    }

    fn fake_release(version: &str, asset_name: &str) -> GithubRelease {
        GithubRelease {
            tag_name: version.to_owned(),
            assets: vec![GithubAsset {
                name: asset_name.to_owned(),
                browser_download_url: format!(
                    "https://github.com/yiiilin/termd/releases/download/{version}/{asset_name}"
                ),
                digest: Some(format!("sha256:{}", "ab".repeat(32))),
                size: 123,
            }],
        }
    }

    fn upgrade_options(version: &'static str, args: &[&str]) -> UpgradeOptions {
        UpgradeOptions::new(version, args.iter().copied())
    }

    #[test]
    fn recognizes_upgrade_for_all_components() {
        for component in [Component::Termd, Component::Termctl, Component::Termrelay] {
            assert!(matches!(
                Options::from_subcommand("1.2.3", None, ["upgrade", "--help"]),
                Some(Options::Upgrade(_))
            ));
            assert!(ParsedUpgradeOptions::parse(component, vec![OsString::from("--help")]).is_ok());
        }
    }

    #[test]
    fn upgrade_help_has_no_network_or_install_side_effects() {
        let mut runtime = FakeUpgradeRuntime::default();

        run_upgrade_with(
            Component::Termd,
            upgrade_options("1.2.3", &["--help"]),
            &mut runtime,
        )
        .unwrap();

        assert!(runtime.output.contains("Usage: termd upgrade"));
        assert!(runtime.output.contains("--allow-session-loss"));
        assert_eq!(runtime.latest_calls, 0);
        assert_eq!(runtime.download_calls, 0);
    }

    #[test]
    fn no_newer_release_exits_without_download() {
        let mut runtime = FakeUpgradeRuntime {
            release: fake_release("1.2.3", "termd-linux-amd64"),
            ..FakeUpgradeRuntime::default()
        };

        run_upgrade_with(
            Component::Termd,
            upgrade_options("1.2.3", &[]),
            &mut runtime,
        )
        .unwrap();

        assert!(runtime.output.contains("is up to date"));
        assert_eq!(runtime.download_calls, 0);
        assert_eq!(runtime.install_calls, 0);
    }

    #[test]
    fn declined_upgrade_does_not_download_or_modify() {
        let mut runtime = FakeUpgradeRuntime {
            confirmation: false,
            ..FakeUpgradeRuntime::default()
        };

        run_upgrade_with(
            Component::Termd,
            upgrade_options("1.2.3", &[]),
            &mut runtime,
        )
        .unwrap();

        assert!(runtime.output.contains("Upgrade cancelled"));
        assert_eq!(runtime.download_calls, 0);
        assert_eq!(runtime.install_calls, 0);
    }

    #[test]
    fn non_tty_upgrade_requires_yes_before_download() {
        let mut runtime = FakeUpgradeRuntime {
            tty: false,
            ..FakeUpgradeRuntime::default()
        };

        let error = run_upgrade_with(
            Component::Termd,
            upgrade_options("1.2.3", &[]),
            &mut runtime,
        )
        .unwrap_err();

        assert!(matches!(error, UpgradeError::NonInteractive));
        assert_eq!(runtime.download_calls, 0);
    }

    #[test]
    fn unsupported_platform_fails_before_network() {
        let mut runtime = FakeUpgradeRuntime {
            os: "macos",
            architecture: "aarch64",
            ..FakeUpgradeRuntime::default()
        };

        let error = run_upgrade_with(
            Component::Termd,
            upgrade_options("1.2.3", &["--yes"]),
            &mut runtime,
        )
        .unwrap_err();

        assert!(matches!(error, UpgradeError::UnsupportedPlatform { .. }));
        assert_eq!(runtime.latest_calls, 0);
    }

    #[test]
    fn release_requires_exactly_one_asset_with_sha256_digest() {
        let missing = GithubRelease {
            tag_name: "1.3.0".to_owned(),
            assets: Vec::new(),
        };
        assert!(matches!(
            select_release_asset(&missing, "termd-linux-amd64"),
            Err(UpgradeError::MissingAsset(_))
        ));

        let mut duplicate = fake_release("1.3.0", "termd-linux-amd64");
        duplicate.assets.push(duplicate.assets[0].clone());
        assert!(matches!(
            select_release_asset(&duplicate, "termd-linux-amd64"),
            Err(UpgradeError::DuplicateAsset(_))
        ));

        for digest in [
            None,
            Some("sha256:1234".to_owned()),
            Some("md5:bad".to_owned()),
        ] {
            let mut release = fake_release("1.3.0", "termd-linux-amd64");
            release.assets[0].digest = digest;
            assert!(matches!(
                select_release_asset(&release, "termd-linux-amd64"),
                Err(UpgradeError::InvalidAssetDigest(_))
            ));
        }
    }

    #[test]
    fn digest_mismatch_stops_before_candidate_execution() {
        let mut runtime = FakeUpgradeRuntime {
            download_fails_digest: true,
            ..FakeUpgradeRuntime::default()
        };

        let error = run_upgrade_with(
            Component::Termd,
            upgrade_options("1.2.3", &["--yes"]),
            &mut runtime,
        )
        .unwrap_err();

        assert!(matches!(error, UpgradeError::DigestMismatch(_)));
        assert_eq!(runtime.version_calls, 0);
        assert_eq!(runtime.install_calls, 0);
    }

    #[test]
    fn staged_candidate_rejects_mismatched_sha256() {
        let bytes = b"not the authenticated release binary";
        let asset = ReleaseAsset {
            name: "termd-linux-amd64".to_owned(),
            download_url: "https://github.com/example/release".to_owned(),
            digest: [0_u8; 32],
            size: bytes.len() as u64,
        };

        let error = match stage_candidate(io::Cursor::new(bytes), Some(bytes.len() as u64), &asset)
        {
            Ok(_) => panic!("mismatched candidate digest was accepted"),
            Err(error) => error,
        };

        assert!(matches!(error, UpgradeError::DigestMismatch(name) if name == asset.name));
    }

    #[test]
    fn candidate_version_must_match_component_and_latest_release() {
        let mut runtime = FakeUpgradeRuntime {
            candidate_status: UpgradeExecutionStatus {
                success: true,
                description: "termrelay 1.3.0".to_owned(),
            },
            ..FakeUpgradeRuntime::default()
        };

        let error = run_upgrade_with(
            Component::Termd,
            upgrade_options("1.2.3", &["--yes"]),
            &mut runtime,
        )
        .unwrap_err();

        assert!(matches!(error, UpgradeError::CandidateIdentity { .. }));
        assert_eq!(runtime.install_calls, 0);
    }

    #[test]
    fn successful_upgrade_runs_candidate_installer_with_derived_prefix() {
        let mut runtime = FakeUpgradeRuntime::default();

        run_upgrade_with(
            Component::Termd,
            upgrade_options("1.2.3", &["--yes"]),
            &mut runtime,
        )
        .unwrap();

        let invocation = runtime.invocation.unwrap();
        assert_eq!(
            invocation.path,
            PathBuf::from("/tmp/verified-upgrade-candidate")
        );
        assert_eq!(invocation.install_prefix, PathBuf::from("/opt/termd"));
        assert_eq!(
            invocation.args,
            ["install", "--yes"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn explicit_prefix_overrides_installed_binary_path() {
        let mut runtime = FakeUpgradeRuntime {
            configured_prefix: Some(PathBuf::from("/srv/termd")),
            ..FakeUpgradeRuntime::default()
        };

        run_upgrade_with(
            Component::Termd,
            upgrade_options("1.2.3", &["--yes"]),
            &mut runtime,
        )
        .unwrap();

        assert_eq!(
            runtime.invocation.unwrap().install_prefix,
            PathBuf::from("/srv/termd")
        );
    }

    #[test]
    fn termctl_rejects_session_loss_flag() {
        let error = ParsedUpgradeOptions::parse(
            Component::Termctl,
            vec![OsString::from("--allow-session-loss")],
        )
        .unwrap_err();

        assert!(matches!(error, UpgradeError::UnknownArgument(_)));
        assert!(!upgrade_help_text(Component::Termctl).contains("--allow-session-loss"));
    }

    #[test]
    fn unknown_upgrade_arguments_never_disclose_values() {
        let inline_secret = "http://proxy-user:proxy-password@proxy.example";
        let options = UpgradeOptions::new(
            "1.2.3",
            [
                format!("--proxy={inline_secret}"),
                "bare-token-secret".to_owned(),
            ],
        );
        let options_debug = format!("{options:?}");
        assert!(!options_debug.contains(inline_secret));
        assert!(!options_debug.contains("bare-token-secret"));

        let error = ParsedUpgradeOptions::parse(
            Component::Termd,
            vec![OsString::from(format!("--proxy={inline_secret}"))],
        )
        .unwrap_err();
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert_eq!(display, "unknown upgrade argument: --proxy");
        assert!(!display.contains(inline_secret));
        assert!(!debug.contains(inline_secret));

        let error = ParsedUpgradeOptions::parse(
            Component::Termd,
            vec![OsString::from("bare-token-secret")],
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "unknown upgrade argument: <argument>");
        assert!(!format!("{error:?}").contains("bare-token-secret"));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_upgrade_argument_uses_safe_placeholder() {
        use std::os::unix::ffi::OsStringExt;

        let error = ParsedUpgradeOptions::parse(
            Component::Termd,
            vec![OsString::from_vec(vec![
                b'-', b'-', 0xff, b'=', b's', b'e', b'c',
            ])],
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "unknown upgrade argument: <non-UTF-8>");
    }

    #[test]
    fn yes_never_implies_session_loss_authorization() {
        let mut runtime = FakeUpgradeRuntime::default();

        run_upgrade_with(
            Component::Termd,
            upgrade_options("1.2.3", &["--yes"]),
            &mut runtime,
        )
        .unwrap();

        assert!(
            !runtime
                .invocation
                .unwrap()
                .args
                .contains(&OsString::from("--allow-session-loss"))
        );
    }

    #[test]
    fn explicit_termd_session_loss_authorization_is_forwarded() {
        let mut runtime = FakeUpgradeRuntime::default();

        run_upgrade_with(
            Component::Termd,
            upgrade_options("1.2.3", &["--yes", "--allow-session-loss"]),
            &mut runtime,
        )
        .unwrap();

        assert!(
            runtime
                .invocation
                .unwrap()
                .args
                .contains(&OsString::from("--allow-session-loss"))
        );
    }

    #[test]
    fn non_root_upgrade_stops_before_download_with_actionable_command() {
        let mut runtime = FakeUpgradeRuntime {
            root: false,
            ..FakeUpgradeRuntime::default()
        };

        let error = run_upgrade_with(
            Component::Termd,
            upgrade_options("1.2.3", &["--yes"]),
            &mut runtime,
        )
        .unwrap_err();

        assert!(
            matches!(error, UpgradeError::RootRequired(command) if command == "sudo termd upgrade --yes")
        );
        assert_eq!(runtime.download_calls, 0);
    }
}
