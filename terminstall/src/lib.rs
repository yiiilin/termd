use std::error::Error as StdError;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const TERMD_INSTALLER: &str = include_str!("../../scripts/install-termd.sh");
const TERMRELAY_INSTALLER: &str = include_str!("../../scripts/install-termrelay.sh");
const TERMCTL_INSTALLER: &str = include_str!("../../scripts/install-termctl.sh");
const SELF_INSTALL_MODE: &str = "embedded-v1";
const INSTALLER_SHELL: &str = "/bin/bash";
const INSTALLER_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";
const INSTALLER_LOCALE: &str = "C";

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
}

impl fmt::Display for Component {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.binary_name())
    }
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
        }
    }
}

#[derive(Debug)]
pub enum Error {
    UnknownArgument(String),
    DuplicateArgument(&'static str),
    MissingValue(&'static str),
    EmptyValue(&'static str),
    PurgeUnsupported,
    NonInteractive,
    Cancelled,
    OpenSelfExecutable(io::Error),
    Input(io::Error),
    Output(io::Error),
    Spawn(io::Error),
    WriteInstaller(io::Error),
    Wait(io::Error),
    ChildFailed {
        component: Component,
        status: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownArgument(argument) => write!(formatter, "unknown installer argument: {argument}"),
            Self::DuplicateArgument(flag) => {
                write!(formatter, "{flag} may be specified only once")
            }
            Self::MissingValue(flag) => write!(formatter, "{flag} requires a value"),
            Self::EmptyValue(flag) => write!(formatter, "{flag} requires a non-empty value"),
            Self::PurgeUnsupported => formatter.write_str(
                "termctl has no managed service state; --purge is unsupported and normal uninstall preserves pairing state",
            ),
            Self::NonInteractive => formatter.write_str(
                "installer confirmation requires a TTY; review with --dry-run, then rerun with --yes for non-interactive execution",
            ),
            Self::Cancelled => formatter.write_str("installation cancelled"),
            Self::OpenSelfExecutable(_) => {
                formatter.write_str("failed to pin the running executable")
            }
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
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::OpenSelfExecutable(source)
            | Self::Input(source)
            | Self::Output(source)
            | Self::Spawn(source)
            | Self::WriteInstaller(source)
            | Self::Wait(source) => Some(source),
            _ => None,
        }
    }
}

pub fn run(component: Component, options: Options) -> Result<(), Error> {
    run_with(component, options, &mut SystemRuntime)
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
        (Component::Termrelay, "--allow-open-relay") => {
            Some(boolean_flag("--allow-open-relay", "open relay mode"))
        }
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
    fn write_output(&mut self, message: &str) -> Result<(), Error>;
    fn confirm(&mut self, prompt: &str) -> Result<bool, Error>;
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

    fn write_output(&mut self, message: &str) -> Result<(), Error> {
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(message.as_bytes())
            .and_then(|_| stdout.flush())
            .map_err(Error::Output)
    }

    fn confirm(&mut self, prompt: &str) -> Result<bool, Error> {
        let mut stderr = io::stderr().lock();
        stderr.write_all(prompt.as_bytes()).map_err(Error::Output)?;
        stderr.flush().map_err(Error::Output)?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer).map_err(Error::Input)?;
        Ok(matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    }

    fn execute(&mut self, invocation: &Invocation) -> Result<ExecutionStatus, Error> {
        let mut command = Command::new(INSTALLER_SHELL);
        command
            .env_clear()
            .env("PATH", INSTALLER_PATH)
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

fn run_with(
    component: Component,
    options: Options,
    runtime: &mut dyn Runtime,
) -> Result<(), Error> {
    let parsed = ParsedOptions::parse(component, options)?;
    if parsed.help {
        return runtime.write_output(&help_text(component, parsed.action));
    }

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
        return Ok(());
    }
    if !parsed.yes {
        if !runtime.is_tty() {
            return Err(Error::NonInteractive);
        }
        if !runtime.confirm("Continue? [y/N] ")? {
            return Err(Error::Cancelled);
        }
    }

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
    env.extend(parsed.script_env);
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
            "\ntermd options:\n  --web | --no-web\n  --listen <HOST:PORT> | --public\n  --relay <WS_URL>\n  --relay-daemon-token-file <PATH>\n  --relay-setup-token-file <PATH>\n  --proxy <URL>\n  --tls-cert <PATH> --tls-key <PATH>\n  --user <USER>\n  --supervisor-version <VERSION>\n  --allow-session-loss  Permit an incompatible supervisor upgrade to remove sessions\n",
        ),
        Component::Termrelay => output.push_str(
            "\ntermrelay options:\n  --web | --no-web\n  --listen <HOST:PORT> | --public\n  --setup-token-file <PATH>\n  --daemon-registry <PATH>\n  --allow-open-relay\n  --tls-cert <PATH> --tls-key <PATH>\n",
        ),
        Component::Termctl => {}
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeRuntime {
        tty: bool,
        confirmation: bool,
        output: String,
        execute_calls: usize,
        invocation_args: Vec<OsString>,
        invocation_env: Vec<(&'static str, OsString)>,
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

        fn write_output(&mut self, message: &str) -> Result<(), Error> {
            self.output.push_str(message);
            Ok(())
        }

        fn confirm(&mut self, _prompt: &str) -> Result<bool, Error> {
            Ok(self.confirmation)
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
    fn dry_run_has_no_side_effects_and_redacts_secret_files() {
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

        assert_eq!(runtime.execute_calls, 0);
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
}
