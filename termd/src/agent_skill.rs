//! Installation support for the bundled `termd-file-offer` agent skill.

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

pub const SKILL_NAME: &str = "termd-file-offer";
const SKILL_FILE_NAME: &str = "SKILL.md";
const BUNDLED_SKILL: &str = include_str!("../skills/termd-file-offer/SKILL.md");
const SUPPORTED_AGENTS: [Agent; 3] = [Agent::Codex, Agent::Claude, Agent::OpenCode];
static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Agent {
    Codex,
    Claude,
    OpenCode,
}

impl Agent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::OpenCode => "opencode",
        }
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Agent {
    type Err = AgentSkillError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "codex" => Ok(Self::Codex),
            "claude" | "claude-code" => Ok(Self::Claude),
            "opencode" => Ok(Self::OpenCode),
            _ => Err(AgentSkillError::UnsupportedAgent {
                name: value.to_owned(),
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkillState {
    ConfigurationMissing,
    NotInstalled,
    Current,
    Modified,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillStatus {
    pub agent: Agent,
    pub path: Option<PathBuf>,
    pub state: SkillState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallAction {
    Installed,
    AlreadyCurrent,
    Replaced,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallReport {
    pub agent: Agent,
    pub path: PathBuf,
    pub action: InstallAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UninstallAction {
    Removed,
    AlreadyAbsent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UninstallReport {
    pub agent: Agent,
    pub path: PathBuf,
    pub action: UninstallAction,
}

#[derive(Debug)]
pub enum AgentSkillError {
    UnsupportedAgent {
        name: String,
    },
    LocationUnavailable {
        agent: Agent,
    },
    NoAgentConfigurationDetected,
    ModifiedInstallation {
        agent: Agent,
        path: PathBuf,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for AgentSkillError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedAgent { name } => {
                write!(formatter, "unsupported agent '{name}'")
            }
            Self::LocationUnavailable { agent } => {
                write!(
                    formatter,
                    "cannot determine the {agent} configuration directory"
                )
            }
            Self::NoAgentConfigurationDetected => {
                formatter.write_str("no supported agent configuration directory was detected")
            }
            Self::ModifiedInstallation { agent, path } => write!(
                formatter,
                "the {agent} skill at {} has modified content and was preserved",
                path.display()
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "failed to {operation} agent skill path {}: {source}",
                path.display()
            ),
        }
    }
}

impl Error for AgentSkillError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

pub fn bundled_skill() -> &'static str {
    BUNDLED_SKILL
}

pub fn install_auto(force: bool) -> Result<Vec<InstallReport>, AgentSkillError> {
    install_auto_with(&AgentLocations::from_process(), force)
}

pub fn installation_status() -> Result<Vec<SkillStatus>, AgentSkillError> {
    status_with(&AgentLocations::from_process())
}

pub fn uninstall(agent: Agent) -> Result<UninstallReport, AgentSkillError> {
    uninstall_with(&AgentLocations::from_process(), agent)
}

#[derive(Clone, Debug)]
struct AgentLocation {
    agent: Agent,
    config_root: Option<PathBuf>,
}

impl AgentLocation {
    fn target_path(&self) -> Option<PathBuf> {
        self.config_root
            .as_ref()
            .map(|root| root.join("skills").join(SKILL_NAME))
    }
}

#[derive(Clone, Debug)]
struct AgentLocations {
    entries: Vec<AgentLocation>,
}

impl AgentLocations {
    fn from_process() -> Self {
        Self::from_lookup(|name| std::env::var_os(name))
    }

    fn from_lookup(mut lookup: impl FnMut(&str) -> Option<OsString>) -> Self {
        let home = nonempty_path(lookup("HOME"));
        let codex_root = nonempty_path(lookup("CODEX_HOME"))
            .or_else(|| home.as_ref().map(|home| home.join(".codex")));
        let claude_root = home.as_ref().map(|home| home.join(".claude"));
        let opencode_root = nonempty_path(lookup("XDG_CONFIG_HOME"))
            .or_else(|| home.as_ref().map(|home| home.join(".config")))
            .map(|config| config.join("opencode"));

        Self {
            entries: vec![
                AgentLocation {
                    agent: Agent::Codex,
                    config_root: codex_root,
                },
                AgentLocation {
                    agent: Agent::Claude,
                    config_root: claude_root,
                },
                AgentLocation {
                    agent: Agent::OpenCode,
                    config_root: opencode_root,
                },
            ],
        }
    }

    fn get(&self, agent: Agent) -> &AgentLocation {
        self.entries
            .iter()
            .find(|location| location.agent == agent)
            .expect("all supported agents have a location entry")
    }
}

fn nonempty_path(value: Option<OsString>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

fn status_with(locations: &AgentLocations) -> Result<Vec<SkillStatus>, AgentSkillError> {
    SUPPORTED_AGENTS
        .into_iter()
        .map(|agent| {
            let location = locations.get(agent);
            let Some(target) = location.target_path() else {
                return Ok(SkillStatus {
                    agent,
                    path: None,
                    state: SkillState::ConfigurationMissing,
                });
            };
            let Some(root) = location.config_root.as_deref() else {
                unreachable!("a target path requires a configuration root")
            };
            let state = if directory_exists(root, "inspect", root)? {
                inspect_target(&target)?
            } else {
                SkillState::ConfigurationMissing
            };
            Ok(SkillStatus {
                agent,
                path: Some(target),
                state,
            })
        })
        .collect()
}

fn install_auto_with(
    locations: &AgentLocations,
    force: bool,
) -> Result<Vec<InstallReport>, AgentSkillError> {
    let mut targets = Vec::new();
    for agent in SUPPORTED_AGENTS {
        let location = locations.get(agent);
        let (Some(root), Some(target)) = (location.config_root.as_deref(), location.target_path())
        else {
            continue;
        };
        if !directory_exists(root, "inspect", root)? {
            continue;
        }
        let state = inspect_target(&target)?;
        if state == SkillState::Modified && !force {
            return Err(AgentSkillError::ModifiedInstallation {
                agent,
                path: target,
            });
        }
        targets.push((agent, target));
    }

    if targets.is_empty() {
        return Err(AgentSkillError::NoAgentConfigurationDetected);
    }

    targets
        .into_iter()
        .map(|(agent, path)| {
            // Two agents may intentionally share one configuration root. Re-read after each
            // write so the second adapter observes the content installed by the first one.
            let state = inspect_target(&path)?;
            let action = match state {
                SkillState::Current => InstallAction::AlreadyCurrent,
                SkillState::NotInstalled => {
                    write_bundled_target(&path, false)?;
                    InstallAction::Installed
                }
                SkillState::Modified => {
                    write_bundled_target(&path, true)?;
                    InstallAction::Replaced
                }
                SkillState::ConfigurationMissing => {
                    unreachable!("auto installation includes only existing configuration roots")
                }
            };
            Ok(InstallReport {
                agent,
                path,
                action,
            })
        })
        .collect()
}

fn uninstall_with(
    locations: &AgentLocations,
    agent: Agent,
) -> Result<UninstallReport, AgentSkillError> {
    let path = locations
        .get(agent)
        .target_path()
        .ok_or(AgentSkillError::LocationUnavailable { agent })?;
    match inspect_target(&path)? {
        SkillState::NotInstalled => Ok(UninstallReport {
            agent,
            path,
            action: UninstallAction::AlreadyAbsent,
        }),
        SkillState::Current => {
            remove_path(&path, "uninstall")?;
            Ok(UninstallReport {
                agent,
                path,
                action: UninstallAction::Removed,
            })
        }
        SkillState::Modified | SkillState::ConfigurationMissing => {
            Err(AgentSkillError::ModifiedInstallation { agent, path })
        }
    }
}

fn directory_exists(
    path: &Path,
    operation: &'static str,
    reported_path: &Path,
) -> Result<bool, AgentSkillError> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io_error(operation, reported_path, source)),
    }
}

fn inspect_target(path: &Path) -> Result<SkillState, AgentSkillError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Ok(SkillState::NotInstalled);
        }
        Err(source) => return Err(io_error("inspect", path, source)),
    };
    if !metadata.file_type().is_dir() {
        return Ok(SkillState::Modified);
    }

    let entries = fs::read_dir(path).map_err(|source| io_error("inspect", path, source))?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| io_error("inspect", path, source))?;
        names.push(entry.file_name());
    }
    if names.len() != 1 || names[0] != SKILL_FILE_NAME {
        return Ok(SkillState::Modified);
    }

    let skill_path = path.join(SKILL_FILE_NAME);
    let skill_metadata = fs::symlink_metadata(&skill_path)
        .map_err(|source| io_error("inspect", &skill_path, source))?;
    if !skill_metadata.file_type().is_file() {
        return Ok(SkillState::Modified);
    }
    let contents = fs::read(&skill_path).map_err(|source| io_error("read", &skill_path, source))?;
    if contents == BUNDLED_SKILL.as_bytes() {
        Ok(SkillState::Current)
    } else {
        Ok(SkillState::Modified)
    }
}

fn write_bundled_target(path: &Path, replace: bool) -> Result<(), AgentSkillError> {
    let parent = path.parent().ok_or_else(|| {
        io_error(
            "create",
            path,
            io::Error::new(io::ErrorKind::InvalidInput, "skill path has no parent"),
        )
    })?;
    fs::create_dir_all(parent).map_err(|source| io_error("create", parent, source))?;

    let staging = create_staging_directory(path, "install")?;
    let skill_path = staging.join(SKILL_FILE_NAME);
    if let Err(source) = fs::write(&skill_path, BUNDLED_SKILL) {
        let _ = fs::remove_dir_all(&staging);
        return Err(io_error("write", &skill_path, source));
    }

    if replace {
        let backup = match unused_sibling_path(path, "backup") {
            Ok(backup) => backup,
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        };
        if let Err(source) = fs::rename(path, &backup) {
            let _ = fs::remove_dir_all(&staging);
            return Err(io_error("replace", path, source));
        }
        if let Err(source) = fs::rename(&staging, path) {
            let _ = fs::rename(&backup, path);
            let _ = fs::remove_dir_all(&staging);
            return Err(io_error("replace", path, source));
        }
        remove_path(&backup, "remove replaced")?;
    } else if let Err(source) = fs::rename(&staging, path) {
        let _ = fs::remove_dir_all(&staging);
        return Err(io_error("install", path, source));
    }

    if inspect_target(path)? != SkillState::Current {
        return Err(io_error(
            "verify",
            path,
            io::Error::other("installed skill content does not match the bundled version"),
        ));
    }
    Ok(())
}

fn create_staging_directory(path: &Path, label: &str) -> Result<PathBuf, AgentSkillError> {
    for _ in 0..32 {
        let candidate = sibling_path(path, label);
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(io_error("create staging", &candidate, source)),
        }
    }
    Err(io_error(
        "create staging",
        path,
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique staging directory",
        ),
    ))
}

fn unused_sibling_path(path: &Path, label: &str) -> Result<PathBuf, AgentSkillError> {
    for _ in 0..32 {
        let candidate = sibling_path(path, label);
        match fs::symlink_metadata(&candidate) {
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(candidate),
            Ok(_) => continue,
            Err(source) => return Err(io_error("inspect staging", &candidate, source)),
        }
    }
    Err(io_error(
        "allocate staging",
        path,
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique staging path",
        ),
    ))
}

fn sibling_path(path: &Path, label: &str) -> PathBuf {
    let counter = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(SKILL_NAME);
    path.with_file_name(format!(
        ".{file_name}.{label}-{}-{counter}",
        std::process::id()
    ))
}

fn remove_path(path: &Path, operation: &'static str) -> Result<(), AgentSkillError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => return Err(io_error(operation, path, source)),
    };
    let result = if metadata.file_type().is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|source| io_error(operation, path, source))
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> AgentSkillError {
    AgentSkillError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "termd-agent-skill-{label}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn join(&self, path: impl AsRef<Path>) -> PathBuf {
            self.0.join(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn locations(values: &[(&str, PathBuf)]) -> AgentLocations {
        let values = values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.as_os_str().to_owned()))
            .collect::<HashMap<_, _>>();
        AgentLocations::from_lookup(|name| values.get(name).cloned())
    }

    fn create_roots(root: &TestDir) -> AgentLocations {
        let home = root.join("home");
        let codex = root.join("codex-home");
        let xdg = root.join("xdg");
        fs::create_dir_all(&codex).unwrap();
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(xdg.join("opencode")).unwrap();
        locations(&[
            ("HOME", home),
            ("CODEX_HOME", codex),
            ("XDG_CONFIG_HOME", xdg),
        ])
    }

    #[test]
    fn resolves_documented_agent_locations() {
        let root = TestDir::new("locations");
        let home = root.join("home");
        let codex = root.join("custom-codex");
        let xdg = root.join("custom-config");
        let locations = locations(&[
            ("HOME", home.clone()),
            ("CODEX_HOME", codex.clone()),
            ("XDG_CONFIG_HOME", xdg.clone()),
        ]);

        assert_eq!(
            locations.get(Agent::Codex).target_path().unwrap(),
            codex.join("skills").join(SKILL_NAME)
        );
        assert_eq!(
            locations.get(Agent::Claude).target_path().unwrap(),
            home.join(".claude").join("skills").join(SKILL_NAME)
        );
        assert_eq!(
            locations.get(Agent::OpenCode).target_path().unwrap(),
            xdg.join("opencode").join("skills").join(SKILL_NAME)
        );
    }

    #[test]
    fn empty_overrides_use_home_defaults() {
        let root = TestDir::new("default-locations");
        let home = root.join("home");
        let values = HashMap::from([
            ("HOME".to_owned(), home.as_os_str().to_owned()),
            ("CODEX_HOME".to_owned(), OsString::new()),
            ("XDG_CONFIG_HOME".to_owned(), OsString::new()),
        ]);
        let locations = AgentLocations::from_lookup(|name| values.get(name).cloned());

        assert_eq!(
            locations.get(Agent::Codex).target_path().unwrap(),
            home.join(".codex").join("skills").join(SKILL_NAME)
        );
        assert_eq!(
            locations.get(Agent::OpenCode).target_path().unwrap(),
            home.join(".config")
                .join("opencode")
                .join("skills")
                .join(SKILL_NAME)
        );
    }

    #[test]
    fn auto_install_detects_existing_roots_and_is_idempotent() {
        let root = TestDir::new("install");
        let locations = create_roots(&root);

        let installed = install_auto_with(&locations, false).unwrap();
        assert_eq!(installed.len(), 3);
        assert!(
            installed
                .iter()
                .all(|report| report.action == InstallAction::Installed)
        );
        for report in &installed {
            assert_eq!(
                fs::read_to_string(report.path.join(SKILL_FILE_NAME)).unwrap(),
                bundled_skill()
            );
        }

        let current = install_auto_with(&locations, false).unwrap();
        assert!(
            current
                .iter()
                .all(|report| report.action == InstallAction::AlreadyCurrent)
        );
    }

    #[test]
    fn auto_install_ignores_missing_configuration_roots() {
        let root = TestDir::new("detected-only");
        let home = root.join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        let locations = locations(&[("HOME", home)]);

        let reports = install_auto_with(&locations, false).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].agent, Agent::Claude);
    }

    #[test]
    fn auto_install_handles_agents_that_share_one_configuration_root() {
        let root = TestDir::new("shared-root");
        let home = root.join("home");
        let shared = home.join(".claude");
        fs::create_dir_all(&shared).unwrap();
        let locations = locations(&[("HOME", home), ("CODEX_HOME", shared)]);

        let reports = install_auto_with(&locations, false).unwrap();
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].action, InstallAction::Installed);
        assert_eq!(reports[1].action, InstallAction::AlreadyCurrent);
        assert_eq!(reports[0].path, reports[1].path);
    }

    #[test]
    fn auto_install_requires_a_detected_configuration_root() {
        let root = TestDir::new("none-detected");
        let locations = locations(&[("HOME", root.join("missing-home"))]);

        assert!(matches!(
            install_auto_with(&locations, false),
            Err(AgentSkillError::NoAgentConfigurationDetected)
        ));
    }

    #[test]
    fn modified_install_blocks_all_auto_writes_without_force() {
        let root = TestDir::new("modified-preflight");
        let locations = create_roots(&root);
        let claude_target = locations.get(Agent::Claude).target_path().unwrap();
        fs::create_dir_all(&claude_target).unwrap();
        fs::write(claude_target.join(SKILL_FILE_NAME), "user changes\n").unwrap();

        assert!(matches!(
            install_auto_with(&locations, false),
            Err(AgentSkillError::ModifiedInstallation {
                agent: Agent::Claude,
                ..
            })
        ));
        assert!(!locations.get(Agent::Codex).target_path().unwrap().exists());
        assert_eq!(
            fs::read_to_string(claude_target.join(SKILL_FILE_NAME)).unwrap(),
            "user changes\n"
        );
    }

    #[test]
    fn force_replaces_modified_directory_as_a_whole() {
        let root = TestDir::new("force");
        let locations = create_roots(&root);
        install_auto_with(&locations, false).unwrap();
        let target = locations.get(Agent::Codex).target_path().unwrap();
        fs::write(target.join(SKILL_FILE_NAME), "changed\n").unwrap();
        fs::write(target.join("notes.txt"), "keep unless forced\n").unwrap();

        let reports = install_auto_with(&locations, true).unwrap();
        let codex = reports
            .iter()
            .find(|report| report.agent == Agent::Codex)
            .unwrap();
        assert_eq!(codex.action, InstallAction::Replaced);
        assert_eq!(inspect_target(&target).unwrap(), SkillState::Current);
        assert!(!target.join("notes.txt").exists());
    }

    #[test]
    fn status_distinguishes_missing_absent_current_and_modified() {
        let root = TestDir::new("status");
        let home = root.join("home");
        fs::create_dir_all(home.join(".claude")).unwrap();
        let locations = locations(&[("HOME", home)]);

        let initial = status_with(&locations).unwrap();
        assert_eq!(initial[0].state, SkillState::ConfigurationMissing);
        assert_eq!(initial[1].state, SkillState::NotInstalled);
        assert_eq!(initial[2].state, SkillState::ConfigurationMissing);

        install_auto_with(&locations, false).unwrap();
        assert_eq!(
            status_with(&locations).unwrap()[1].state,
            SkillState::Current
        );
        let target = locations.get(Agent::Claude).target_path().unwrap();
        fs::write(target.join("extra"), "modified").unwrap();
        assert_eq!(
            status_with(&locations).unwrap()[1].state,
            SkillState::Modified
        );
    }

    #[test]
    fn uninstall_removes_only_current_content() {
        let root = TestDir::new("uninstall");
        let locations = create_roots(&root);
        install_auto_with(&locations, false).unwrap();
        let target = locations.get(Agent::OpenCode).target_path().unwrap();

        let removed = uninstall_with(&locations, Agent::OpenCode).unwrap();
        assert_eq!(removed.action, UninstallAction::Removed);
        assert!(!target.exists());
        assert_eq!(
            uninstall_with(&locations, Agent::OpenCode).unwrap().action,
            UninstallAction::AlreadyAbsent
        );
    }

    #[test]
    fn uninstall_preserves_modified_content() {
        let root = TestDir::new("uninstall-modified");
        let locations = create_roots(&root);
        install_auto_with(&locations, false).unwrap();
        let target = locations.get(Agent::Claude).target_path().unwrap();
        fs::write(target.join(SKILL_FILE_NAME), "personalized\n").unwrap();

        assert!(matches!(
            uninstall_with(&locations, Agent::Claude),
            Err(AgentSkillError::ModifiedInstallation {
                agent: Agent::Claude,
                ..
            })
        ));
        assert_eq!(
            fs::read_to_string(target.join(SKILL_FILE_NAME)).unwrap(),
            "personalized\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_target_is_modified_and_never_removed() {
        use std::os::unix::fs::symlink;

        let root = TestDir::new("symlink");
        let home = root.join("home");
        fs::create_dir_all(home.join(".claude").join("skills")).unwrap();
        let locations = locations(&[("HOME", home)]);
        let target = locations.get(Agent::Claude).target_path().unwrap();
        let external = root.join("external");
        fs::create_dir(&external).unwrap();
        fs::write(external.join(SKILL_FILE_NAME), bundled_skill()).unwrap();
        symlink(&external, &target).unwrap();

        assert_eq!(inspect_target(&target).unwrap(), SkillState::Modified);
        assert!(matches!(
            uninstall_with(&locations, Agent::Claude),
            Err(AgentSkillError::ModifiedInstallation { .. })
        ));
        assert!(target.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(external.join(SKILL_FILE_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn force_replaces_a_symlink_without_touching_its_target() {
        use std::os::unix::fs::symlink;

        let root = TestDir::new("force-symlink");
        let home = root.join("home");
        fs::create_dir_all(home.join(".claude").join("skills")).unwrap();
        let locations = locations(&[("HOME", home)]);
        let target = locations.get(Agent::Claude).target_path().unwrap();
        let external = root.join("external");
        fs::create_dir(&external).unwrap();
        fs::write(external.join("personal.txt"), "preserve me\n").unwrap();
        symlink(&external, &target).unwrap();

        let reports = install_auto_with(&locations, true).unwrap();
        assert_eq!(reports[0].action, InstallAction::Replaced);
        assert_eq!(inspect_target(&target).unwrap(), SkillState::Current);
        assert_eq!(
            fs::read_to_string(external.join("personal.txt")).unwrap(),
            "preserve me\n"
        );
    }

    #[test]
    fn bundled_document_covers_the_offer_contract() {
        let skill = bundled_skill();
        assert!(skill.contains("name: termd-file-offer"));
        assert!(skill.contains("termd offer-file <PATH> [--socket <PATH>] [--json]"));
        assert!(skill.contains("directory, package it into an archive"));
        assert!(skill.contains("Each successful invocation creates one broadcast"));
        assert!(skill.contains("Show the command output"));
        assert!(skill.contains("Do not scan for daemon sockets"));
    }

    #[test]
    fn parses_supported_agent_names() {
        assert_eq!("codex".parse::<Agent>().unwrap(), Agent::Codex);
        assert_eq!("claude".parse::<Agent>().unwrap(), Agent::Claude);
        assert_eq!("claude-code".parse::<Agent>().unwrap(), Agent::Claude);
        assert_eq!("opencode".parse::<Agent>().unwrap(), Agent::OpenCode);
        assert!(matches!(
            "auto".parse::<Agent>(),
            Err(AgentSkillError::UnsupportedAgent { .. })
        ));
    }
}
