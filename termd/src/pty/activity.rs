use std::fs;

use termd_proto::{
    SessionActivityAgent, SessionActivityKind, SessionActivityState, SessionAiActivityPayload,
    UnixTimestampMillis,
};

const OSC_PAYLOAD_MAX_BYTES: usize = 256;
const OSC_COMMAND_MAX_BYTES: usize = 5;
const FOREGROUND_PROBE_MIN_INTERVAL_MS: u64 = 100;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ForegroundProcess {
    Agent(SessionActivityAgent),
    Other,
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum EventIdentityProbeState {
    /// A conflicting explicit identity hint may trigger one immediate probe.
    #[default]
    Ready,
    /// A plain probe was uncertain; the next structured event gets one retry.
    Pending,
    /// An event retry was inconclusive; wait for the normal probe interval.
    Consumed,
}

#[derive(Debug)]
pub(super) struct SessionActivityDetector {
    parser: BoundedOscParser,
    activity: Option<SessionAiActivityPayload>,
    last_foreground: ForegroundProcess,
    last_foreground_probe_at_ms: Option<u64>,
    /// Completed grants exactly one immediate probe for the next non-empty output.
    completed_identity_probe_pending: bool,
    event_identity_probe_state: EventIdentityProbeState,
}

impl Default for SessionActivityDetector {
    fn default() -> Self {
        Self {
            parser: BoundedOscParser::default(),
            activity: None,
            last_foreground: ForegroundProcess::Unknown,
            last_foreground_probe_at_ms: None,
            completed_identity_probe_pending: false,
            event_identity_probe_state: EventIdentityProbeState::Ready,
        }
    }
}

impl SessionActivityDetector {
    pub(super) fn activity(&self) -> Option<SessionAiActivityPayload> {
        self.activity
    }

    /// Returns true only when the semantic wire state changed.
    pub(super) fn observe_output(
        &mut self,
        bytes: &[u8],
        now_ms: u64,
        mut foreground: impl FnMut() -> ForegroundProcess,
    ) -> bool {
        let events = self.parser.push(bytes);
        let tracks_foreground = self
            .activity
            .is_some_and(|activity| !matches!(activity.state, SessionActivityState::Completed));
        let needs_identity_probe = self.activity.is_none() && !bytes.is_empty();
        let probe_due = self.last_foreground_probe_at_ms.is_none_or(|last_ms| {
            now_ms.saturating_sub(last_ms) >= FOREGROUND_PROBE_MIN_INTERVAL_MS
        });
        let completed_needs_probe = self.activity.is_some_and(|activity| {
            activity.state == SessionActivityState::Completed
                && !bytes.is_empty()
                && (self.completed_identity_probe_pending || probe_due)
        });
        if events.is_empty()
            && !tracks_foreground
            && !needs_identity_probe
            && !completed_needs_probe
        {
            return false;
        }

        let first_structured_event = self.activity.is_none()
            && !events.is_empty()
            && self.event_identity_probe_state == EventIdentityProbeState::Ready
            && matches!(
                self.last_foreground,
                ForegroundProcess::Other | ForegroundProcess::Unknown
            );
        let event_identity_probe_pending = !events.is_empty()
            && self.event_identity_probe_state == EventIdentityProbeState::Pending;
        let identity_hint_requires_probe = self.event_identity_probe_state
            == EventIdentityProbeState::Ready
            && events
                .iter()
                .filter_map(osc_event_identity_hint)
                .any(|hint| {
                    matches!(
                        self.last_foreground,
                        ForegroundProcess::Agent(cached) if cached != hint
                    )
                });
        let should_probe = self.completed_identity_probe_pending && !bytes.is_empty()
            || first_structured_event
            || event_identity_probe_pending
            || identity_hint_requires_probe
            || probe_due;
        if should_probe {
            let observed_foreground = foreground();
            if self.completed_identity_probe_pending {
                self.completed_identity_probe_pending = false;
            }
            self.event_identity_probe_state = if events.is_empty() {
                if matches!(
                    observed_foreground,
                    ForegroundProcess::Other | ForegroundProcess::Unknown
                ) {
                    EventIdentityProbeState::Pending
                } else {
                    EventIdentityProbeState::Ready
                }
            } else if matches!(
                observed_foreground,
                ForegroundProcess::Other | ForegroundProcess::Unknown
            ) || event_identity_hints_mismatch(&events, observed_foreground)
            {
                EventIdentityProbeState::Consumed
            } else {
                EventIdentityProbeState::Ready
            };
            self.last_foreground = observed_foreground;
            self.last_foreground_probe_at_ms = Some(now_ms);
        }

        let mut changed = self.observe_foreground(self.last_foreground, now_ms);
        let ForegroundProcess::Agent(agent) = self.last_foreground else {
            return changed;
        };
        for event in events {
            changed |= match event {
                OscEvent::Title(title) => self.observe_title(agent, &title, now_ms),
                OscEvent::ClaudeStatus(payload) if agent == SessionActivityAgent::ClaudeCode => {
                    self.observe_claude_status(&payload, now_ms)
                }
                OscEvent::ItermProgress(payload) if agent != SessionActivityAgent::Codex => {
                    self.observe_iterm_progress(agent, &payload, now_ms)
                }
                OscEvent::ClaudeStatus(_) | OscEvent::ItermProgress(_) => false,
            };
        }
        changed
    }

    fn observe_foreground(&mut self, foreground: ForegroundProcess, now_ms: u64) -> bool {
        match foreground {
            ForegroundProcess::Agent(agent) => match self.activity {
                Some(activity) if activity.agent != agent => {
                    if agent == SessionActivityAgent::Codex {
                        self.activity = None;
                        true
                    } else {
                        self.transition(agent, SessionActivityState::Idle, now_ms)
                    }
                }
                None if agent != SessionActivityAgent::Codex => {
                    self.transition(agent, SessionActivityState::Idle, now_ms)
                }
                Some(_) | None => false,
            },
            ForegroundProcess::Unknown => false,
            ForegroundProcess::Other => match self.activity.map(|activity| activity.state) {
                Some(SessionActivityState::Running | SessionActivityState::Attention) => {
                    let agent = self.activity.expect("active activity exists").agent;
                    self.transition(agent, SessionActivityState::Completed, now_ms)
                }
                Some(SessionActivityState::Idle) => {
                    self.activity = None;
                    true
                }
                Some(SessionActivityState::Completed) | None => false,
            },
        }
    }

    fn observe_title(&mut self, agent: SessionActivityAgent, title: &[u8], now_ms: u64) -> bool {
        if agent == SessionActivityAgent::Codex {
            return self.observe_codex_title(title, now_ms);
        }

        match self.activity {
            Some(activity) if activity.agent == agent => false,
            _ => self.transition(agent, SessionActivityState::Idle, now_ms),
        }
    }

    fn observe_codex_title(&mut self, title: &[u8], now_ms: u64) -> bool {
        let Ok(title) = std::str::from_utf8(title) else {
            return false;
        };
        let title = title.trim();
        if title.eq_ignore_ascii_case("action required")
            || title.to_ascii_lowercase().contains("action required")
        {
            return self.transition(
                SessionActivityAgent::Codex,
                SessionActivityState::Attention,
                now_ms,
            );
        }
        if is_codex_running_title(title) {
            return self.transition(
                SessionActivityAgent::Codex,
                SessionActivityState::Running,
                now_ms,
            );
        }

        match self.activity.map(|activity| activity.state) {
            Some(SessionActivityState::Running | SessionActivityState::Attention) => self
                .transition(
                    SessionActivityAgent::Codex,
                    SessionActivityState::Completed,
                    now_ms,
                ),
            Some(SessionActivityState::Completed) => false,
            Some(SessionActivityState::Idle) => false,
            None => self.transition(
                SessionActivityAgent::Codex,
                SessionActivityState::Idle,
                now_ms,
            ),
        }
    }

    fn observe_claude_status(&mut self, payload: &[u8], now_ms: u64) -> bool {
        let Some(status) = parse_claude_status(payload) else {
            return false;
        };
        let Ok(status) = std::str::from_utf8(&status) else {
            return false;
        };
        let status = status.trim().to_ascii_lowercase();
        if status.starts_with("working") || status == "busy" || status == "running" {
            return self.transition(
                SessionActivityAgent::ClaudeCode,
                SessionActivityState::Running,
                now_ms,
            );
        }
        if status.starts_with("waiting") || status.contains("attention") {
            return self.transition(
                SessionActivityAgent::ClaudeCode,
                SessionActivityState::Attention,
                now_ms,
            );
        }
        if status.is_empty() || status == "idle" {
            return self.finish_or_idle(SessionActivityAgent::ClaudeCode, now_ms);
        }
        false
    }

    fn observe_iterm_progress(
        &mut self,
        agent: SessionActivityAgent,
        payload: &[u8],
        now_ms: u64,
    ) -> bool {
        match parse_iterm_progress_state(payload) {
            Some(0) => self.finish_or_idle(agent, now_ms),
            Some(1 | 3) => self.transition(agent, SessionActivityState::Running, now_ms),
            Some(2 | 4) => self.transition(agent, SessionActivityState::Attention, now_ms),
            Some(_) | None => false,
        }
    }

    fn finish_or_idle(&mut self, agent: SessionActivityAgent, now_ms: u64) -> bool {
        match self.activity {
            Some(activity)
                if activity.agent == agent
                    && matches!(
                        activity.state,
                        SessionActivityState::Running | SessionActivityState::Attention
                    ) =>
            {
                self.transition(agent, SessionActivityState::Completed, now_ms)
            }
            Some(activity)
                if activity.agent == agent && activity.state == SessionActivityState::Completed =>
            {
                false
            }
            _ => self.transition(agent, SessionActivityState::Idle, now_ms),
        }
    }

    fn transition(
        &mut self,
        agent: SessionActivityAgent,
        state: SessionActivityState,
        now_ms: u64,
    ) -> bool {
        if self
            .activity
            .is_some_and(|activity| activity.agent == agent && activity.state == state)
        {
            return false;
        }
        self.activity = Some(SessionAiActivityPayload {
            kind: SessionActivityKind::Ai,
            agent,
            state,
            changed_at_ms: UnixTimestampMillis(now_ms),
        });
        self.completed_identity_probe_pending = state == SessionActivityState::Completed;
        true
    }
}

fn is_codex_running_title(title: &str) -> bool {
    let lower = title.to_ascii_lowercase();
    if lower.contains("working") || lower.contains("thinking") || lower.contains("running") {
        return true;
    }

    is_codex_spinner_title(title)
}

fn is_codex_spinner_title(title: &str) -> bool {
    let mut chars = title.chars();
    if chars
        .next()
        .is_some_and(|first| ('\u{2800}'..='\u{28ff}').contains(&first))
    {
        return true;
    }
    let Some(close) = title.find(']') else {
        return false;
    };
    if !title.starts_with('[') || close > 8 {
        return false;
    }
    let marker = title[1..close].trim();
    marker.chars().count() == 1 && !title[close + 1..].trim().is_empty()
}

pub(super) fn foreground_process_for_shell(shell_pid: Option<u32>) -> ForegroundProcess {
    let Some(shell_pid) = shell_pid else {
        return ForegroundProcess::Unknown;
    };
    let Ok(stat) = fs::read_to_string(format!("/proc/{shell_pid}/stat")) else {
        return ForegroundProcess::Unknown;
    };
    let Some(foreground_pgrp) = parse_linux_tpgid(&stat) else {
        return ForegroundProcess::Unknown;
    };
    let Ok(cmdline) = fs::read(format!("/proc/{foreground_pgrp}/cmdline")) else {
        return ForegroundProcess::Unknown;
    };
    if let Some(agent) = agent_for_cmdline(&cmdline) {
        ForegroundProcess::Agent(agent)
    } else if cmdline.iter().any(|byte| *byte != 0) {
        ForegroundProcess::Other
    } else {
        ForegroundProcess::Unknown
    }
}

fn parse_linux_tpgid(stat: &str) -> Option<u32> {
    let after_comm = stat.rsplit_once(") ")?.1;
    // Fields after comm start at state (field 3); tpgid is field 8.
    let tpgid = after_comm
        .split_ascii_whitespace()
        .nth(5)?
        .parse::<i32>()
        .ok()?;
    u32::try_from(tpgid).ok().filter(|pid| *pid > 0)
}

fn agent_for_cmdline(cmdline: &[u8]) -> Option<SessionActivityAgent> {
    let args = cmdline
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .collect::<Vec<_>>();
    let program = *args.first()?;
    if let Some(agent) = agent_for_executable_name(path_file_name(program)?) {
        return Some(agent);
    }
    let program_name = path_file_name(program)?;
    if !matches!(program_name, b"node" | b"nodejs" | b"bun" | b"bunx") {
        return None;
    }

    let mut entrypoints = args
        .iter()
        .skip(1)
        .copied()
        .filter(|arg| !arg.starts_with(b"-"));
    let mut entrypoint = entrypoints.next()?;
    if matches!(program_name, b"bun" | b"bunx") && matches!(entrypoint, b"run" | b"x" | b"exec") {
        entrypoint = entrypoints.next()?;
    }
    agent_for_script_entrypoint(entrypoint)
}

fn agent_for_executable_name(name: &[u8]) -> Option<SessionActivityAgent> {
    match name {
        b"codex" => Some(SessionActivityAgent::Codex),
        b"claude" => Some(SessionActivityAgent::ClaudeCode),
        b"opencode" => Some(SessionActivityAgent::OpenCode),
        b"zcode" | b"zcode-cli" => Some(SessionActivityAgent::ZCode),
        _ => None,
    }
}

fn agent_for_script_entrypoint(script: &[u8]) -> Option<SessionActivityAgent> {
    let name = path_file_name(script)?;
    if let Some(agent) = agent_for_executable_name(name) {
        return Some(agent);
    }
    if name == b"codex.js" && path_has_scoped_package(script, b"@openai", b"codex") {
        return Some(SessionActivityAgent::Codex);
    }
    if name == b"cli.js" && path_has_scoped_package(script, b"@anthropic-ai", b"claude-code") {
        return Some(SessionActivityAgent::ClaudeCode);
    }
    if matches!(name, b"cli.js" | b"index.js" | b"opencode.js")
        && (path_has_component(script, b"opencode-ai") || path_has_component(script, b"opencode"))
    {
        return Some(SessionActivityAgent::OpenCode);
    }
    if matches!(
        name,
        b"cli.js" | b"index.js" | b"zcode.js" | b"zcode-cli.js"
    ) && (path_has_component(script, b"zcode") || path_has_component(script, b"zcode-cli"))
    {
        return Some(SessionActivityAgent::ZCode);
    }
    None
}

fn path_has_scoped_package(path: &[u8], scope: &[u8], package: &[u8]) -> bool {
    let components = path.split(|byte| *byte == b'/').collect::<Vec<_>>();
    components
        .windows(2)
        .any(|pair| pair[0] == scope && pair[1] == package)
}

fn path_has_component(path: &[u8], expected: &[u8]) -> bool {
    path.split(|byte| *byte == b'/')
        .any(|component| component == expected)
}

fn path_file_name(path: &[u8]) -> Option<&[u8]> {
    path.rsplit(|byte| *byte == b'/')
        .find(|part| !part.is_empty())
}

fn parse_claude_status(payload: &[u8]) -> Option<Vec<u8>> {
    let mut key = Vec::new();
    let mut value = Vec::new();
    let mut reading_value = false;
    let mut escaped = false;

    for byte in payload.iter().copied().chain(std::iter::once(b';')) {
        if escaped {
            if reading_value {
                value.push(byte);
            } else {
                key.push(byte);
            }
            escaped = false;
            continue;
        }
        match byte {
            b'\\' => escaped = true,
            b'=' if !reading_value => reading_value = true,
            b';' => {
                if key == b"status" && reading_value {
                    return Some(value);
                }
                key.clear();
                value.clear();
                reading_value = false;
            }
            _ if reading_value => value.push(byte),
            _ => key.push(byte),
        }
    }
    None
}

fn parse_iterm_progress_state(payload: &[u8]) -> Option<u8> {
    let mut fields = payload.split(|byte| *byte == b';');
    if fields.next()? != b"4" {
        return None;
    }
    match fields.next()? {
        [state @ b'0'..=b'9'] => Some(state - b'0'),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OscPayloadKind {
    Title,
    ClaudeStatus,
    ItermProgress,
}

impl OscPayloadKind {
    fn event(self, payload: Vec<u8>) -> OscEvent {
        match self {
            Self::Title => OscEvent::Title(payload),
            Self::ClaudeStatus => OscEvent::ClaudeStatus(payload),
            Self::ItermProgress => OscEvent::ItermProgress(payload),
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum OscEvent {
    Title(Vec<u8>),
    ClaudeStatus(Vec<u8>),
    ItermProgress(Vec<u8>),
}

fn osc_event_identity_hint(event: &OscEvent) -> Option<SessionActivityAgent> {
    match event {
        OscEvent::ClaudeStatus(_) => Some(SessionActivityAgent::ClaudeCode),
        OscEvent::Title(title) => title_identity_hint(title),
        OscEvent::ItermProgress(_) => None,
    }
}

fn event_identity_hints_mismatch(events: &[OscEvent], foreground: ForegroundProcess) -> bool {
    let ForegroundProcess::Agent(agent) = foreground else {
        return false;
    };
    events
        .iter()
        .filter_map(osc_event_identity_hint)
        .any(|hint| hint != agent)
}

fn title_identity_hint(title: &[u8]) -> Option<SessionActivityAgent> {
    let title = std::str::from_utf8(title).ok()?.trim();
    let lower = title.to_ascii_lowercase();
    if lower == "opencode" || lower.starts_with("oc |") {
        return Some(SessionActivityAgent::OpenCode);
    }
    if lower == "zcode" || lower.starts_with("zcode |") || lower.starts_with("zcode -") {
        return Some(SessionActivityAgent::ZCode);
    }
    if lower.contains("claude code") {
        return Some(SessionActivityAgent::ClaudeCode);
    }
    if lower.contains("codex") || lower.contains("action required") || is_codex_spinner_title(title)
    {
        return Some(SessionActivityAgent::Codex);
    }
    None
}

#[derive(Debug, Default)]
struct BoundedOscParser {
    state: OscParserState,
}

#[derive(Debug, Default)]
enum OscParserState {
    #[default]
    Ground,
    Escape,
    Command(Vec<u8>),
    Payload(OscPayloadKind, Vec<u8>),
    PayloadEscape(OscPayloadKind, Vec<u8>),
    Discard,
    DiscardEscape,
}

impl BoundedOscParser {
    fn push(&mut self, bytes: &[u8]) -> Vec<OscEvent> {
        let mut events = Vec::new();
        for &byte in bytes {
            let state = std::mem::take(&mut self.state);
            self.state = match state {
                OscParserState::Ground if byte == 0x1b => OscParserState::Escape,
                OscParserState::Ground => OscParserState::Ground,
                OscParserState::Escape if byte == b']' => OscParserState::Command(Vec::new()),
                OscParserState::Escape if byte == 0x1b => OscParserState::Escape,
                OscParserState::Escape => OscParserState::Ground,
                OscParserState::Command(mut command)
                    if byte.is_ascii_digit() && command.len() < OSC_COMMAND_MAX_BYTES =>
                {
                    command.push(byte);
                    OscParserState::Command(command)
                }
                OscParserState::Command(command) if byte == b';' => match command.as_slice() {
                    b"0" | b"2" => OscParserState::Payload(OscPayloadKind::Title, Vec::new()),
                    b"9" => OscParserState::Payload(OscPayloadKind::ItermProgress, Vec::new()),
                    b"21337" => OscParserState::Payload(OscPayloadKind::ClaudeStatus, Vec::new()),
                    _ => OscParserState::Discard,
                },
                OscParserState::Command(_) if byte == 0x07 => OscParserState::Ground,
                OscParserState::Command(_) if byte == 0x1b => OscParserState::DiscardEscape,
                OscParserState::Command(_) => OscParserState::Discard,
                OscParserState::Payload(kind, payload) if byte == 0x07 => {
                    events.push(kind.event(payload));
                    OscParserState::Ground
                }
                OscParserState::Payload(kind, payload) if byte == 0x1b => {
                    OscParserState::PayloadEscape(kind, payload)
                }
                OscParserState::Payload(kind, mut payload)
                    if payload.len() < OSC_PAYLOAD_MAX_BYTES =>
                {
                    payload.push(byte);
                    OscParserState::Payload(kind, payload)
                }
                OscParserState::Payload(_, _) => OscParserState::Discard,
                OscParserState::PayloadEscape(kind, payload) if byte == b'\\' => {
                    events.push(kind.event(payload));
                    OscParserState::Ground
                }
                OscParserState::PayloadEscape(_, _) if byte == 0x07 => OscParserState::Ground,
                OscParserState::PayloadEscape(_, _) if byte == 0x1b => {
                    OscParserState::DiscardEscape
                }
                OscParserState::PayloadEscape(_, _) => OscParserState::Discard,
                OscParserState::Discard if byte == 0x07 => OscParserState::Ground,
                OscParserState::Discard if byte == 0x1b => OscParserState::DiscardEscape,
                OscParserState::Discard => OscParserState::Discard,
                OscParserState::DiscardEscape if byte == b'\\' || byte == 0x07 => {
                    OscParserState::Ground
                }
                OscParserState::DiscardEscape if byte == 0x1b => OscParserState::DiscardEscape,
                OscParserState::DiscardEscape => OscParserState::Discard,
            };
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc_titles_span_chunks_and_accept_bel_and_st() {
        let mut parser = BoundedOscParser::default();
        assert!(parser.push(b"before\x1b]0;[.").is_empty());
        assert_eq!(
            parser.push(b"] Working\x07after"),
            [OscEvent::Title(b"[.] Working".to_vec())]
        );
        assert_eq!(
            parser.push(b"\x1b]2;Action Required\x1b\\"),
            [OscEvent::Title(b"Action Required".to_vec())]
        );
    }

    #[test]
    fn overlong_osc_title_is_discarded_and_parser_recovers() {
        let mut bytes = b"\x1b]0;".to_vec();
        bytes.extend(std::iter::repeat_n(b'x', OSC_PAYLOAD_MAX_BYTES + 1));
        bytes.extend_from_slice(b"\x07\x1b]0;ready\x07");
        let mut parser = BoundedOscParser::default();
        assert_eq!(parser.push(&bytes), [OscEvent::Title(b"ready".to_vec())]);
    }

    #[test]
    fn state_transitions_latch_completed_and_ignore_repeated_spinner_frames() {
        let mut detector = SessionActivityDetector::default();
        let codex = || ForegroundProcess::Agent(SessionActivityAgent::Codex);
        assert!(detector.observe_output(b"\x1b]0;project\x07", 10, codex));
        assert_eq!(
            detector.activity().unwrap().state,
            SessionActivityState::Idle
        );
        assert!(detector.observe_output(b"\x1b]0;[.] Working\x07", 110, codex));
        assert!(!detector.observe_output(b"\x1b]0;[o] Working\x07", 210, codex));
        assert_eq!(
            detector.activity().unwrap().changed_at_ms,
            UnixTimestampMillis(110)
        );
        assert!(detector.observe_output(b"\x1b]2;Action Required\x1b\\", 310, codex));
        assert!(detector.observe_output(b"\x1b]0;project\x07", 410, codex));
        assert_eq!(
            detector.activity().unwrap().state,
            SessionActivityState::Completed
        );
        assert!(!detector.observe_output(b"\x1b]0;project\x07", 510, codex));
        assert!(detector.observe_output(b"\x1b]0;[/] Working\x07", 610, codex));
        assert_eq!(
            detector.activity().unwrap().state,
            SessionActivityState::Running
        );
    }

    #[test]
    fn only_codex_foreground_can_create_activity_and_exit_completes_active_work() {
        let mut detector = SessionActivityDetector::default();
        assert!(
            !detector.observe_output(b"\x1b]0;[.] Working\x07", 100, || ForegroundProcess::Other)
        );
        assert_eq!(detector.activity(), None);
        assert!(detector.observe_output(b"\x1b]0;[.] Working\x07", 200, || {
            ForegroundProcess::Agent(SessionActivityAgent::Codex)
        }));
        assert!(detector.observe_output(b"shell prompt", 300, || ForegroundProcess::Other));
        assert_eq!(
            detector.activity().unwrap().state,
            SessionActivityState::Completed
        );
    }

    #[test]
    fn idle_activity_clears_when_plain_output_returns_to_shell() {
        let mut detector = SessionActivityDetector::default();
        assert!(detector.observe_output(b"\x1b]0;project\x07", 100, || {
            ForegroundProcess::Agent(SessionActivityAgent::Codex)
        }));
        assert_eq!(
            detector.activity().unwrap().state,
            SessionActivityState::Idle
        );

        assert!(detector.observe_output(
            b"shell prompt",
            100 + FOREGROUND_PROBE_MIN_INTERVAL_MS + 1,
            || ForegroundProcess::Other,
        ));
        assert_eq!(detector.activity(), None);
    }

    #[test]
    fn claude_tab_status_maps_only_structured_status_values() {
        let mut detector = SessionActivityDetector::default();
        let claude = || ForegroundProcess::Agent(SessionActivityAgent::ClaudeCode);
        assert!(detector.observe_output(
            b"\x1b]21337;indicator=#ff9500;future=x\\;y;status=Working;status-color=#ff9500\x07",
            100,
            claude,
        ));
        assert_eq!(
            detector
                .activity()
                .map(|activity| (activity.agent, activity.state)),
            Some((
                SessionActivityAgent::ClaudeCode,
                SessionActivityState::Running
            ))
        );
        assert!(detector.observe_output(
            b"\x1b]21337;status=Waiting;unknown=value\x1b\\",
            201,
            claude,
        ));
        assert_eq!(
            detector.activity().unwrap().state,
            SessionActivityState::Attention
        );
        assert!(detector.observe_output(b"\x1b]21337;status=Idle\x07", 302, claude));
        assert_eq!(
            detector.activity().unwrap().state,
            SessionActivityState::Completed
        );
    }

    #[test]
    fn iterm_progress_maps_structured_states_for_recognized_agent() {
        let mut detector = SessionActivityDetector::default();
        let opencode = || ForegroundProcess::Agent(SessionActivityAgent::OpenCode);
        assert!(detector.observe_output(b"\x1b]0;OpenCode\x07", 100, opencode));
        assert_eq!(
            detector
                .activity()
                .map(|activity| (activity.agent, activity.state)),
            Some((SessionActivityAgent::OpenCode, SessionActivityState::Idle))
        );
        assert!(detector.observe_output(b"\x1b]9;4;3;\x1b\\", 201, opencode));
        assert_eq!(
            detector.activity().unwrap().state,
            SessionActivityState::Running
        );
        assert!(detector.observe_output(b"\x1b]9;4;0;\x07", 302, opencode));
        assert_eq!(
            detector.activity().unwrap().state,
            SessionActivityState::Completed
        );
    }

    #[test]
    fn unstructured_output_does_not_guess_non_codex_activity() {
        let mut detector = SessionActivityDetector::default();
        let zcode = || ForegroundProcess::Agent(SessionActivityAgent::ZCode);
        assert!(detector.observe_output(b"\x1b]2;ZCode\x07", 100, zcode));
        let idle = detector.activity().unwrap();
        assert_eq!(idle.state, SessionActivityState::Idle);
        assert!(!detector.observe_output(b"lots of ordinary output", 201, zcode));
        assert_eq!(detector.activity(), Some(idle));
    }

    #[test]
    fn initial_plain_output_recognizes_non_codex_agent_as_idle() {
        let mut detector = SessionActivityDetector::default();
        assert!(detector.observe_output(b"plain startup output", 100, || {
            ForegroundProcess::Agent(SessionActivityAgent::ZCode)
        }));
        assert_eq!(
            detector
                .activity()
                .map(|activity| (activity.agent, activity.state)),
            Some((SessionActivityAgent::ZCode, SessionActivityState::Idle))
        );
    }

    #[test]
    fn first_osc_event_refreshes_recent_other_foreground_cache() {
        let mut detector = SessionActivityDetector::default();
        assert!(!detector.observe_output(b"shell output", 100, || ForegroundProcess::Other));
        assert!(detector.observe_output(b"\x1b]0;[.] Working\x07", 150, || {
            ForegroundProcess::Agent(SessionActivityAgent::Codex)
        }));
        assert_eq!(
            detector
                .activity()
                .map(|activity| (activity.agent, activity.state)),
            Some((SessionActivityAgent::Codex, SessionActivityState::Running))
        );
    }

    #[test]
    fn completed_agent_switches_to_new_non_codex_agent_on_plain_output() {
        let mut detector = SessionActivityDetector::default();
        let opencode = || ForegroundProcess::Agent(SessionActivityAgent::OpenCode);
        assert!(detector.observe_output(b"\x1b]9;4;1;25\x07", 100, opencode));
        assert!(detector.observe_output(b"\x1b]9;4;0;\x07", 201, opencode));
        assert_eq!(
            detector.activity().unwrap().state,
            SessionActivityState::Completed
        );

        assert!(detector.observe_output(b"plain zcode output", 250, || {
            ForegroundProcess::Agent(SessionActivityAgent::ZCode)
        }));
        assert_eq!(
            detector
                .activity()
                .map(|activity| (activity.agent, activity.state)),
            Some((SessionActivityAgent::ZCode, SessionActivityState::Idle))
        );
    }

    #[test]
    fn completed_agent_plain_output_reprobe_keeps_same_agent_or_other_unchanged() {
        let mut detector = SessionActivityDetector::default();
        let opencode = || ForegroundProcess::Agent(SessionActivityAgent::OpenCode);
        assert!(detector.observe_output(b"\x1b]9;4;1;25\x07", 100, opencode));
        assert!(detector.observe_output(b"\x1b]9;4;0;\x07", 201, opencode));
        let completed = detector.activity().unwrap();

        assert!(!detector.observe_output(b"same agent output", 302, opencode));
        assert_eq!(detector.activity(), Some(completed));
        assert!(!detector.observe_output(b"shell output", 403, || ForegroundProcess::Other));
        assert_eq!(detector.activity(), Some(completed));
    }

    #[test]
    fn completed_plain_other_then_unhinted_progress_refreshes_cached_foreground() {
        for next_agent in [SessionActivityAgent::OpenCode, SessionActivityAgent::ZCode] {
            let mut detector = SessionActivityDetector::default();
            let opencode = || ForegroundProcess::Agent(SessionActivityAgent::OpenCode);
            assert!(detector.observe_output(b"\x1b]9;4;1;25\x07", 100, opencode));
            assert!(detector.observe_output(b"\x1b]9;4;0;\x07", 201, opencode));
            assert!(
                !detector.observe_output(b"shell output", 250, || { ForegroundProcess::Other })
            );

            assert!(detector.observe_output(b"\x1b]9;4;3;\x07", 270, || {
                ForegroundProcess::Agent(next_agent)
            }));
            assert_eq!(
                detector
                    .activity()
                    .map(|activity| (activity.agent, activity.state)),
                Some((next_agent, SessionActivityState::Running))
            );
        }
    }

    #[test]
    fn completed_one_shot_plain_probe_returns_to_interval_throttle() {
        let mut detector = SessionActivityDetector::default();
        let opencode = || ForegroundProcess::Agent(SessionActivityAgent::OpenCode);
        assert!(detector.observe_output(b"\x1b]9;4;1;25\x07", 100, opencode));
        assert!(detector.observe_output(b"\x1b]9;4;0;\x07", 201, opencode));
        let completed = detector.activity().unwrap();
        let probe_count = std::cell::Cell::new(0);
        let mut counted_probe = || {
            probe_count.set(probe_count.get() + 1);
            ForegroundProcess::Agent(SessionActivityAgent::OpenCode)
        };

        assert!(!detector.observe_output(b"first plain frame", 250, &mut counted_probe));
        assert_eq!(probe_count.get(), 1);
        assert_eq!(detector.activity(), Some(completed));
        assert!(!detector.observe_output(b"second plain frame", 260, &mut counted_probe));
        assert_eq!(probe_count.get(), 1);
        assert_eq!(detector.activity(), Some(completed));
    }

    #[test]
    fn uncertain_completed_probe_allows_one_event_retry_then_throttles() {
        for uncertain in [ForegroundProcess::Unknown, ForegroundProcess::Other] {
            let mut detector = SessionActivityDetector::default();
            let opencode = || ForegroundProcess::Agent(SessionActivityAgent::OpenCode);
            assert!(detector.observe_output(b"\x1b]9;4;1;25\x07", 100, opencode));
            assert!(detector.observe_output(b"\x1b]9;4;0;\x07", 201, opencode));
            let completed = detector.activity().unwrap();
            let probe_count = std::cell::Cell::new(0);
            let mut uncertain_probe = || {
                probe_count.set(probe_count.get() + 1);
                uncertain
            };

            assert!(!detector.observe_output(b"uncertain plain frame", 250, &mut uncertain_probe));
            assert_eq!(probe_count.get(), 1);
            assert!(!detector.observe_output(b"another plain frame", 260, &mut uncertain_probe));
            assert_eq!(probe_count.get(), 1);

            assert!(!detector.observe_output(b"\x1b]9;4;3;\x07", 270, &mut uncertain_probe));
            assert_eq!(probe_count.get(), 2);
            assert!(!detector.observe_output(b"\x1b]9;4;3;\x07", 280, &mut uncertain_probe));
            assert_eq!(probe_count.get(), 2);

            assert!(!detector.observe_output(b"\x1b]9;4;3;\x07", 370, &mut uncertain_probe));
            assert_eq!(probe_count.get(), 3);
            assert_eq!(detector.activity(), Some(completed));
        }
    }

    #[test]
    fn claude_status_refreshes_recent_different_agent_cache() {
        let mut detector = SessionActivityDetector::default();
        assert!(detector.observe_output(b"\x1b]0;[.] Working\x07", 100, || {
            ForegroundProcess::Agent(SessionActivityAgent::Codex)
        }));
        assert!(
            detector.observe_output(b"\x1b]21337;status=Working\x07", 150, || {
                ForegroundProcess::Agent(SessionActivityAgent::ClaudeCode)
            })
        );
        assert_eq!(
            detector
                .activity()
                .map(|activity| (activity.agent, activity.state)),
            Some((
                SessionActivityAgent::ClaudeCode,
                SessionActivityState::Running
            ))
        );
    }

    #[test]
    fn explicit_agent_title_refreshes_recent_different_agent_cache() {
        let mut detector = SessionActivityDetector::default();
        assert!(detector.observe_output(b"\x1b]0;[.] Working\x07", 100, || {
            ForegroundProcess::Agent(SessionActivityAgent::Codex)
        }));
        assert!(detector.observe_output(b"\x1b]0;OpenCode\x07", 150, || {
            ForegroundProcess::Agent(SessionActivityAgent::OpenCode)
        }));
        assert_eq!(
            detector
                .activity()
                .map(|activity| (activity.agent, activity.state)),
            Some((SessionActivityAgent::OpenCode, SessionActivityState::Idle))
        );
    }

    #[test]
    fn repeated_codex_spinner_titles_keep_foreground_probe_throttled() {
        let mut detector = SessionActivityDetector::default();
        let probe_count = std::cell::Cell::new(0);
        let mut codex_probe = || {
            probe_count.set(probe_count.get() + 1);
            ForegroundProcess::Agent(SessionActivityAgent::Codex)
        };

        assert!(detector.observe_output(b"\x1b]0;[.] Working\x07", 100, &mut codex_probe));
        assert!(!detector.observe_output(b"\x1b]0;[o] Working\x07", 120, &mut codex_probe));
        assert!(!detector.observe_output(b"\x1b]0;[/] Working\x07", 140, &mut codex_probe));
        assert!(!detector.observe_output(b"\x1b]0;[-] Working\x07", 160, &mut codex_probe));
        assert_eq!(probe_count.get(), 1);

        assert!(!detector.observe_output(b"\x1b]0;[.] Working\x07", 201, &mut codex_probe));
        assert_eq!(probe_count.get(), 2);
        assert!(!detector.observe_output(b"\x1b]0;[o] Working\x07", 220, &mut codex_probe));
        assert_eq!(probe_count.get(), 2);
        assert!(!detector.observe_output(b"\x1b]0;[/] Working\x07", 301, &mut codex_probe));
        assert_eq!(probe_count.get(), 3);
    }

    #[test]
    fn completed_codex_spinner_uses_one_shot_probe_then_returns_to_throttle() {
        let mut detector = SessionActivityDetector::default();
        let codex = || ForegroundProcess::Agent(SessionActivityAgent::Codex);
        assert!(detector.observe_output(b"\x1b]0;[.] Working\x07", 100, codex));
        assert!(detector.observe_output(b"\x1b]0;project\x07", 201, codex));
        assert_eq!(
            detector.activity().unwrap().state,
            SessionActivityState::Completed
        );
        let probe_count = std::cell::Cell::new(0);
        let mut counted_probe = || {
            probe_count.set(probe_count.get() + 1);
            ForegroundProcess::Agent(SessionActivityAgent::Codex)
        };

        assert!(detector.observe_output(b"\x1b]0;[o] Working\x07", 250, &mut counted_probe));
        assert_eq!(probe_count.get(), 1);
        assert!(!detector.observe_output(b"\x1b]0;[/] Working\x07", 260, &mut counted_probe));
        assert_eq!(probe_count.get(), 1);
    }

    #[test]
    fn completed_state_ignores_same_agent_titles_until_explicit_progress() {
        let mut detector = SessionActivityDetector::default();
        let opencode = || ForegroundProcess::Agent(SessionActivityAgent::OpenCode);
        assert!(detector.observe_output(b"\x1b]9;4;1;25\x07", 100, opencode));
        assert!(detector.observe_output(b"\x1b]9;4;0;\x07", 201, opencode));
        let completed = detector.activity().unwrap();
        assert_eq!(completed.state, SessionActivityState::Completed);

        assert!(!detector.observe_output(b"\x1b]0;OC | next session\x07", 302, opencode));
        assert_eq!(detector.activity(), Some(completed));
        assert!(detector.observe_output(b"\x1b]9;4;3;\x07", 403, opencode));
        assert_eq!(
            detector.activity().unwrap().state,
            SessionActivityState::Running
        );
    }

    #[test]
    fn parses_linux_tpgid_and_matches_supported_agent_process_shapes() {
        assert_eq!(
            parse_linux_tpgid("42 (shell with spaces) S 1 42 42 34816 73 0 0"),
            Some(73)
        );
        for (cmdline, expected) in [
            (
                b"/usr/local/bin/codex\0--no-alt-screen\0".as_slice(),
                SessionActivityAgent::Codex,
            ),
            (
                b"/usr/bin/node\0/usr/lib/node_modules/@openai/codex/bin/codex.js\0".as_slice(),
                SessionActivityAgent::Codex,
            ),
            (
                b"node\0/root/.nvm/versions/node/v24.14.1/bin/codex\0--yolo\0".as_slice(),
                SessionActivityAgent::Codex,
            ),
            (
                b"bun\0run\0/usr/lib/node_modules/@openai/codex/bin/codex.js\0".as_slice(),
                SessionActivityAgent::Codex,
            ),
            (
                b"/usr/local/bin/claude\0".as_slice(),
                SessionActivityAgent::ClaudeCode,
            ),
            (
                b"node\0/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js\0".as_slice(),
                SessionActivityAgent::ClaudeCode,
            ),
            (
                b"bun\0/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js\0".as_slice(),
                SessionActivityAgent::ClaudeCode,
            ),
            (
                b"/home/me/.opencode/bin/opencode\0".as_slice(),
                SessionActivityAgent::OpenCode,
            ),
            (
                b"bun\0run\0/usr/lib/node_modules/opencode-ai/cli.js\0".as_slice(),
                SessionActivityAgent::OpenCode,
            ),
            (
                b"node\0/usr/lib/node_modules/opencode-ai/index.js\0".as_slice(),
                SessionActivityAgent::OpenCode,
            ),
            (
                b"/usr/local/bin/zcode\0".as_slice(),
                SessionActivityAgent::ZCode,
            ),
            (
                b"/usr/local/bin/zcode-cli\0".as_slice(),
                SessionActivityAgent::ZCode,
            ),
            (
                b"node\0/usr/lib/node_modules/zcode-cli/cli.js\0".as_slice(),
                SessionActivityAgent::ZCode,
            ),
            (b"bunx\0zcode-cli\0".as_slice(), SessionActivityAgent::ZCode),
        ] {
            assert_eq!(agent_for_cmdline(cmdline), Some(expected));
        }
        assert_eq!(agent_for_cmdline(b"/bin/bash\0-c\0echo codex\0"), None);
        assert_eq!(
            agent_for_cmdline(b"node\0/usr/lib/node_modules/example/cli.js\0zcode\0"),
            None
        );
        assert_eq!(
            agent_for_cmdline(b"node\0/usr/lib/node_modules/@openai/codex-fork/bin/codex.js\0"),
            None
        );
    }
}
