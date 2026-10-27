//! Hook-free state detection: one [`Detector`] per agent family, all sharing
//! the rules in [`classify`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use zj_herdr_core::AgentState;

use crate::procs::ProcInfo;
use crate::transcript::{self, LastTurn, Transcript};

/// A TUI redrawing a spinner or streaming a reply stays above this share of
/// one core; one sitting at a prompt stays below it.
pub const CPU_ACTIVE_PERCENT: f32 = 2.0;
/// Agents append to their transcript as each block completes, so a write
/// this recent means the turn is still moving.
pub const ACTIVE_WINDOW_SECS: u64 = 5;
/// A requested tool that has kept CPU and transcript quiet this long is
/// waiting on a permission prompt rather than running.
pub const BLOCKED_AFTER_SECS: u64 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatus {
    pub state: AgentState,
    pub session_id: Option<String>,
    /// Unix seconds of the last observed activity (transcript write).
    pub last_activity: Option<u64>,
}

pub trait Detector: Send + Sync {
    /// The `AgentSpec::kind` this detector handles.
    fn kind(&self) -> &'static str;
    /// `None` when this detector has no opinion (no transcript found).
    fn detect(&self, proc: &ProcInfo) -> Option<AgentStatus>;
}

/// The state rules, shared by every detector.
pub fn classify(proc: &ProcInfo, transcript: Option<&Transcript>) -> AgentState {
    let now = proc.sampled_at;
    let age = transcript.map(|t| now.saturating_sub(t.modified));
    let cpu_active = proc.cpu >= CPU_ACTIVE_PERCENT;
    let recent = age.is_some_and(|a| a < ACTIVE_WINDOW_SECS);
    let quiet_long = !cpu_active
        && proc.cpu_quiet_for.is_none_or(|q| q >= BLOCKED_AFTER_SECS)
        && age.is_none_or(|a| a >= BLOCKED_AFTER_SECS);

    match transcript.map(|t| t.last) {
        Some(LastTurn::PendingTool) if quiet_long => AgentState::Blocked,
        Some(LastTurn::PendingTool) => AgentState::Working,
        _ if cpu_active || recent => AgentState::Working,
        // A reply that never came while everything is quiet: the agent was
        // stopped or crashed mid-turn and is back at its prompt.
        _ => AgentState::Idle,
    }
}

fn status(proc: &ProcInfo, transcript: Transcript) -> AgentStatus {
    AgentStatus {
        state: classify(proc, Some(&transcript)),
        last_activity: Some(transcript.modified),
        session_id: Some(transcript.session_id),
    }
}

/// Value of `--flag <v>` or `--flag=<v>` in argv.
fn flag_value<'a>(argv: &'a [String], flags: &[&str]) -> Option<&'a str> {
    argv.iter().enumerate().find_map(|(i, arg)| {
        flags.iter().find_map(|f| {
            if arg == f {
                argv.get(i + 1).map(String::as_str).filter(|v| !v.starts_with('-'))
            } else {
                arg.strip_prefix(f)?.strip_prefix('=')
            }
        })
    })
}

/// Claude Code: `<projects>/<cwd-slug>/<session-id>.jsonl`.
pub struct ClaudeDetector {
    pub projects: PathBuf,
}

impl ClaudeDetector {
    fn transcript_path(&self, proc: &ProcInfo) -> Option<PathBuf> {
        let dir = self.projects.join(transcript::claude_slug(proc.cwd.as_deref()?));
        // An explicit session wins while the agent keeps writing to it.
        if let Some(id) = flag_value(&proc.argv, &["--resume", "-r", "--session-id"]) {
            let path = dir.join(format!("{id}.jsonl"));
            if transcript::modified_secs(&path).is_some_and(|m| m >= proc.start_time) {
                return Some(path);
            }
        }
        transcript::newest_file(&dir, "jsonl")
    }
}

impl Detector for ClaudeDetector {
    fn kind(&self) -> &'static str {
        "claude"
    }

    fn detect(&self, proc: &ProcInfo) -> Option<AgentStatus> {
        let transcript = transcript::read_claude(&self.transcript_path(proc)?)?;
        Some(status(proc, transcript))
    }
}

/// Session id and recorded cwd of a rollout.
type RolloutMeta = (String, Option<String>);

/// Codex: newest `<sessions>/**/rollout-*.jsonl` recorded for the same cwd.
pub struct CodexDetector {
    pub sessions: PathBuf,
    /// First-line metadata never changes, so read it once per file.
    meta: Mutex<HashMap<PathBuf, Option<RolloutMeta>>>,
}

impl CodexDetector {
    pub fn new(sessions: PathBuf) -> Self {
        CodexDetector {
            sessions,
            meta: Mutex::new(HashMap::new()),
        }
    }

    fn meta(&self, path: &Path) -> Option<RolloutMeta> {
        let mut cache = self.meta.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .entry(path.to_path_buf())
            .or_insert_with(|| transcript::codex_meta(path))
            .clone()
    }
}

impl Detector for CodexDetector {
    fn kind(&self) -> &'static str {
        "codex"
    }

    fn detect(&self, proc: &ProcInfo) -> Option<AgentStatus> {
        let cwd = proc.cwd.as_deref()?.to_string_lossy().into_owned();
        let resumed = proc
            .argv
            .iter()
            .position(|a| a == "resume")
            .and_then(|i| proc.argv.get(i + 1))
            .filter(|id| !id.starts_with('-'));
        let rollouts = transcript::codex_rollouts(&self.sessions);
        let (path, id) = rollouts.into_iter().find_map(|(_, path)| {
            let (id, rec_cwd) = self.meta(&path)?;
            let hit = match resumed {
                Some(want) => &id == want,
                None => rec_cwd.as_deref() == Some(cwd.as_str()),
            };
            hit.then_some((path, id))
        })?;
        Some(status(proc, transcript::read_codex(&path, id)?))
    }
}

/// Every other agent: process signals only.
pub struct ProcessDetector;

impl Detector for ProcessDetector {
    fn kind(&self) -> &'static str {
        "*"
    }

    fn detect(&self, proc: &ProcInfo) -> Option<AgentStatus> {
        Some(AgentStatus {
            state: classify(proc, None),
            session_id: None,
            last_activity: None,
        })
    }
}

/// Where agents keep their transcripts on this machine.
#[derive(Debug, Clone, Default)]
pub struct Roots {
    pub claude_projects: Option<PathBuf>,
    pub codex_sessions: Option<PathBuf>,
}

impl Roots {
    /// Honours `CLAUDE_CONFIG_DIR` and `CODEX_HOME` like the agents do.
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let dir = |var: &str, default: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|h| h.join(default)))
        };
        Roots {
            claude_projects: dir("CLAUDE_CONFIG_DIR", ".claude").map(|d| d.join("projects")),
            codex_sessions: dir("CODEX_HOME", ".codex").map(|d| d.join("sessions")),
        }
    }

    pub fn detectors(&self) -> Vec<Box<dyn Detector>> {
        let mut out: Vec<Box<dyn Detector>> = Vec::new();
        if let Some(p) = &self.claude_projects {
            out.push(Box::new(ClaudeDetector { projects: p.clone() }));
        }
        if let Some(s) = &self.codex_sessions {
            out.push(Box::new(CodexDetector::new(s.clone())));
        }
        out
    }
}

/// Run the detector for the process's family, falling back to process signals.
pub fn detect(detectors: &[Box<dyn Detector>], proc: &ProcInfo) -> AgentStatus {
    detectors
        .iter()
        .filter(|d| d.kind() == proc.spec.kind)
        .find_map(|d| d.detect(proc))
        .or_else(|| ProcessDetector.detect(proc))
        .expect("process detector always answers")
}
