//! Shared types for zj-herdr: the agent model and the versioned JSON wire
//! format exchanged between the daemon, the CLI, remote machines and the
//! Zellij plugin. This crate builds for native targets and `wasm32-wasip1`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

mod cmd;
pub mod view;
pub use cmd::{prompt_reply, PluginCommand, CMD_SCHEMA, PROMPT_ACK, PROMPT_NACK};

/// Pipe name and `schema` tag of a full state snapshot.
pub const STATE_SCHEMA: &str = "zj_herdr.state.v1";

/// What an agent is doing, as far as zj-herdr can tell without hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    /// Generating output or running a tool.
    Working,
    /// Waiting on the user, typically a tool permission prompt.
    Blocked,
    /// Finished its turn and waiting for the next prompt.
    Idle,
    /// The process has exited.
    Done,
    /// Not enough signal to decide.
    #[default]
    Unknown,
}

impl AgentState {
    pub const ALL: [AgentState; 5] = [
        AgentState::Working,
        AgentState::Blocked,
        AgentState::Idle,
        AgentState::Done,
        AgentState::Unknown,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Idle => "idle",
            AgentState::Done => "done",
            AgentState::Unknown => "unknown",
        }
    }

    /// One-cell glyph for list views.
    pub fn glyph(self) -> &'static str {
        match self {
            AgentState::Working => "●",
            AgentState::Blocked => "!",
            AgentState::Idle => "○",
            AgentState::Done => "✓",
            AgentState::Unknown => "?",
        }
    }

    /// Sort key: lower needs the user's attention sooner.
    pub fn attention_rank(self) -> u8 {
        match self {
            AgentState::Blocked => 0,
            AgentState::Idle => 1,
            AgentState::Working => 2,
            AgentState::Unknown => 3,
            AgentState::Done => 4,
        }
    }
}

impl fmt::Display for AgentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for AgentState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        AgentState::ALL
            .into_iter()
            .find(|st| st.as_str() == s)
            .ok_or_else(|| {
                format!("unknown state {s:?} (expected working, blocked, idle, done or unknown)")
            })
    }
}

/// The Zellij pane an agent runs in.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Pane {
    pub session: String,
    pub pane_id: u32,
    /// The tab holding the pane, as the daemon last saw it; `None` when
    /// unknown (an older daemon, or Zellij did not answer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<PaneTab>,
}

impl Pane {
    pub fn new(session: impl Into<String>, pane_id: u32) -> Self {
        Pane {
            session: session.into(),
            pane_id,
            tab: None,
        }
    }
}

/// A Zellij tab, by 0-based position and name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneTab {
    pub position: usize,
    pub name: String,
}

/// One agent process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    /// Stable identifier, `<machine>/<pid>`.
    pub id: String,
    pub machine: String,
    /// Agent family from the known-agents table, e.g. `claude` or `codex`.
    pub kind: String,
    pub pid: u32,
    #[serde(default)]
    pub cwd: Option<String>,
    pub state: AgentState,
    /// Unix seconds at which `state` was entered (best effort).
    pub since: u64,
    /// `None` when the agent runs outside Zellij.
    #[serde(default)]
    pub pane: Option<Pane>,
    /// The agent's own session id (Claude transcript stem, Codex rollout id).
    #[serde(default)]
    pub session_id: Option<String>,
    /// What a `blocked` agent waits on: the pending tool's name and, for a
    /// shell tool, its command, on one line of at most
    /// [`WAITING_MAX_CHARS`]. `None` in any other state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<String>,
    /// Unix seconds at which the process started; `0` when unknown. Tells
    /// the agent apart from a later process that reuses its pid.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub started: u64,
    /// Tokens used by the session so far, from its transcript; `None` when
    /// the agent's transcript is not read or records no usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Token counts summed over a session's replies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl Tokens {
    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }
}

/// Longest [`Agent::waiting`] text.
pub const WAITING_MAX_CHARS: usize = 60;

impl Agent {
    pub fn make_id(machine: &str, pid: u32) -> String {
        format!("{machine}/{pid}")
    }

    /// Whether `selector` names this agent: its id, its pid, or
    /// `<session>:<pane_id>`.
    pub fn matches(&self, selector: &str) -> bool {
        if selector == self.id || selector == self.pid.to_string() {
            return true;
        }
        match (&self.pane, selector.rsplit_once(':')) {
            (Some(pane), Some((session, id))) => {
                pane.session == session && id.parse() == Ok(pane.pane_id)
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MachineStatus {
    #[default]
    Ok,
    Unreachable,
}

/// A machine and the agents found on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    pub name: String,
    /// SSH target for remote machines, `None` for the local one.
    #[serde(default)]
    pub ssh: Option<String>,
    #[serde(default)]
    pub status: MachineStatus,
    #[serde(default)]
    pub error: Option<String>,
    pub agents: Vec<Agent>,
}

/// Full snapshot, `zj_herdr.state.v1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateV1 {
    pub schema: String,
    /// Unix seconds.
    pub generated_at: u64,
    pub machines: Vec<Machine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    Json(String),
    Schema {
        expected: &'static str,
        found: String,
    },
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Json(e) => write!(f, "invalid JSON: {e}"),
            WireError::Schema { expected, found } => {
                write!(f, "unsupported schema {found:?}, expected {expected:?}")
            }
        }
    }
}

impl std::error::Error for WireError {}

impl StateV1 {
    pub fn new(generated_at: u64, machines: Vec<Machine>) -> Self {
        StateV1 {
            schema: STATE_SCHEMA.to_string(),
            generated_at,
            machines,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("state serializes")
    }

    /// Parse a snapshot, rejecting other schema versions.
    pub fn from_json(text: &str) -> Result<Self, WireError> {
        let state: StateV1 =
            serde_json::from_str(text).map_err(|e| WireError::Json(e.to_string()))?;
        if state.schema != STATE_SCHEMA {
            return Err(WireError::Schema {
                expected: STATE_SCHEMA,
                found: state.schema,
            });
        }
        Ok(state)
    }

    pub fn agents(&self) -> impl Iterator<Item = &Agent> {
        self.machines.iter().flat_map(|m| m.agents.iter())
    }
}

/// One pane `zj-herdr restore` would reopen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestorePane {
    pub session: String,
    pub pane_id: u32,
    #[serde(default)]
    pub cwd: Option<String>,
    pub command: Vec<String>,
}

/// `zj-herdr restore --dry-run --json`: what a restore would reopen.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RestorePreview {
    pub count: usize,
    #[serde(default)]
    pub panes: Vec<RestorePane>,
}

impl RestorePreview {
    pub fn new(panes: Vec<RestorePane>) -> Self {
        RestorePreview {
            count: panes.len(),
            panes,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("preview serializes")
    }

    pub fn from_json(text: &str) -> Result<Self, WireError> {
        serde_json::from_str(text.trim()).map_err(|e| WireError::Json(e.to_string()))
    }
}

/// Compact elapsed time: `42s`, `5m`, `3h`, `2d`.
pub fn fmt_elapsed(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// Compact token count, at most four characters below 1000G: `812`,
/// `4.8k`, `97k`, `4.8M`, `12G`. Rounds down, so `9999` is `9.9k`.
pub fn fmt_tokens(n: u64) -> String {
    for (size, unit) in [(1_000_000_000, "G"), (1_000_000, "M"), (1_000, "k")] {
        if n >= size {
            let whole = n / size;
            return if whole < 10 {
                format!("{whole}.{}{unit}", n % size * 10 / size)
            } else {
                format!("{whole}{unit}")
            };
        }
    }
    n.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_compact() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(812), "812");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_000), "1.0k");
        assert_eq!(fmt_tokens(9_999), "9.9k");
        assert_eq!(fmt_tokens(97_400), "97k");
        assert_eq!(fmt_tokens(999_999), "999k");
        assert_eq!(fmt_tokens(4_870_000), "4.8M");
        assert_eq!(fmt_tokens(48_000_000), "48M");
        assert_eq!(fmt_tokens(12_300_000_000), "12G");
        assert_eq!(fmt_tokens(u64::MAX), "18446744073G");
    }

    #[test]
    fn token_total_adds_every_kind() {
        let t = Tokens {
            input: 1,
            output: 2,
            cache_read: 3,
            cache_write: 4,
        };
        assert_eq!(t.total(), 10);
    }

    #[test]
    fn elapsed_is_compact() {
        assert_eq!(fmt_elapsed(0), "0s");
        assert_eq!(fmt_elapsed(59), "59s");
        assert_eq!(fmt_elapsed(60), "1m");
        assert_eq!(fmt_elapsed(7200), "2h");
        assert_eq!(fmt_elapsed(200_000), "2d");
    }

    fn sample() -> StateV1 {
        StateV1::new(
            1_700_000_000,
            vec![
                Machine {
                    name: "box".into(),
                    ssh: None,
                    status: MachineStatus::Ok,
                    error: None,
                    agents: vec![Agent {
                        id: Agent::make_id("box", 42),
                        machine: "box".into(),
                        kind: "claude".into(),
                        pid: 42,
                        cwd: Some("/w/p".into()),
                        state: AgentState::Blocked,
                        since: 1_699_999_990,
                        pane: Some(Pane {
                            session: "main".into(),
                            pane_id: 3,
                            tab: Some(PaneTab {
                                position: 1,
                                name: "work".into(),
                            }),
                        }),
                        session_id: Some("abc".into()),
                        waiting: None,
                        started: 0,
                        tokens: Some(Tokens {
                            input: 1,
                            output: 2,
                            cache_read: 3,
                            cache_write: 4,
                        }),
                    }],
                },
                Machine {
                    name: "far".into(),
                    ssh: Some("far-alias".into()),
                    status: MachineStatus::Unreachable,
                    error: Some("timeout".into()),
                    agents: vec![],
                },
            ],
        )
    }

    #[test]
    fn state_round_trips() {
        let state = sample();
        let json = state.to_json();
        assert!(json.contains("\"schema\":\"zj_herdr.state.v1\""));
        assert!(json.contains("\"state\":\"blocked\""));
        assert!(json.contains("\"status\":\"unreachable\""));
        assert!(json.contains("\"tokens\":{\"input\":1,"), "{json}");
        assert_eq!(StateV1::from_json(&json).unwrap(), state);
    }

    #[test]
    fn rejects_other_schema() {
        let json = sample().to_json().replace("state.v1", "state.v9");
        assert!(matches!(
            StateV1::from_json(&json),
            Err(WireError::Schema { .. })
        ));
    }

    #[test]
    fn optional_fields_default() {
        let json = r#"{"schema":"zj_herdr.state.v1","generated_at":1,"machines":[
            {"name":"m","agents":[{"id":"m/1","machine":"m","kind":"pi","pid":1,"state":"idle","since":0}]}]}"#;
        let state = StateV1::from_json(json).unwrap();
        let agent = state.agents().next().unwrap();
        assert_eq!(agent.pane, None);
        assert_eq!(agent.tokens, None);
        assert_eq!(state.machines[0].status, MachineStatus::Ok);
        assert!(!state.to_json().contains("\"tokens\""));
    }

    #[test]
    fn old_snapshots_without_tabs_still_parse() {
        let json = r#"{"schema":"zj_herdr.state.v1","generated_at":1,"machines":[
            {"name":"m","agents":[{"id":"m/1","machine":"m","kind":"claude","pid":1,
            "state":"idle","since":0,"pane":{"session":"dev","pane_id":4}}]}]}"#;
        let state = StateV1::from_json(json).unwrap();
        let agent = state.agents().next().unwrap();
        assert_eq!(agent.pane, Some(Pane::new("dev", 4)));
        // And a pane without a tab is written the old way.
        assert!(!state.to_json().contains("\"tab\""));
    }

    #[test]
    fn restore_preview_round_trips() {
        let preview = RestorePreview::new(vec![RestorePane {
            session: "dev".into(),
            pane_id: 3,
            cwd: Some("/w".into()),
            command: vec!["claude".into(), "--resume".into(), "abc".into()],
        }]);
        let json = preview.to_json();
        assert!(json.starts_with(r#"{"count":1,"#), "{json}");
        assert_eq!(RestorePreview::from_json(&format!("{json}\n")), Ok(preview));
        assert_eq!(
            RestorePreview::from_json(r#"{"count":0}"#).map(|p| p.count),
            Ok(0)
        );
        assert!(RestorePreview::from_json("would reopen dev:3").is_err());
    }

    #[test]
    fn state_parses_from_str() {
        for st in AgentState::ALL {
            assert_eq!(st.as_str().parse::<AgentState>(), Ok(st));
        }
        assert!("asleep".parse::<AgentState>().is_err());
    }

    #[test]
    fn selector_matching() {
        let state = sample();
        let agent = state.agents().next().unwrap();
        assert!(agent.matches("box/42"));
        assert!(agent.matches("42"));
        assert!(agent.matches("main:3"));
        assert!(!agent.matches("main:4"));
        assert!(!agent.matches("other:3"));
        assert!(!agent.matches("43"));
    }
}
