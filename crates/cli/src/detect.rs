//! Hook-free state detection: one [`Detector`] per agent family, all sharing
//! the rules in [`classify`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use zj_herdr_core::AgentState;

use crate::procs::ProcInfo;
use crate::transcript::{self, FileTimes, LastTurn, Transcript};

/// A TUI redrawing a spinner or streaming a reply stays above this share of
/// one core; one sitting at a prompt stays below it.
pub const CPU_ACTIVE_PERCENT: f32 = 2.0;
/// Agents append to their transcript as each block completes, so a write
/// this recent means the turn is still moving.
pub const ACTIVE_WINDOW_SECS: u64 = 5;
/// A requested tool that has kept CPU and transcript quiet this long is
/// waiting on a permission prompt rather than running.
pub const BLOCKED_AFTER_SECS: u64 = 5;
/// A transcript is created after its process starts, but both times are
/// read at whole-second resolution.
const CREATED_SLACK_SECS: u64 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatus {
    pub state: AgentState,
    pub session_id: Option<String>,
    /// Unix seconds of the last observed activity (transcript write).
    pub last_activity: Option<u64>,
    /// For a `blocked` agent, the pending call it waits on.
    pub waiting: Option<String>,
}

pub trait Detector: Send + Sync {
    /// The `AgentSpec::kind` this detector handles.
    fn kind(&self) -> &'static str;
    /// Called once per scan with every process of this detector's kind,
    /// before any [`Detector::detect`], so that transcripts can be shared
    /// out between processes.
    fn prepare(&self, _procs: &[&ProcInfo]) {}
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

/// State from process signals alone, for agents whose transcripts zj-herdr
/// does not read and for any agent whose transcript is not found.
pub fn signals_only(proc: &ProcInfo) -> AgentStatus {
    AgentStatus {
        state: classify(proc, None),
        session_id: None,
        last_activity: None,
        waiting: None,
    }
}

fn status(proc: &ProcInfo, transcript: Transcript) -> AgentStatus {
    let state = classify(proc, Some(&transcript));
    AgentStatus {
        state,
        last_activity: Some(transcript.modified),
        session_id: Some(transcript.session_id),
        waiting: transcript.pending.filter(|_| state == AgentState::Blocked),
    }
}

/// Value of `--flag <v>` or `--flag=<v>` in argv.
fn flag_value<'a>(argv: &'a [String], flags: &[&str]) -> Option<&'a str> {
    argv.iter().enumerate().find_map(|(i, arg)| {
        flags.iter().find_map(|f| {
            if arg == f {
                argv.get(i + 1)
                    .map(String::as_str)
                    .filter(|v| !v.starts_with('-'))
            } else {
                arg.strip_prefix(f)?.strip_prefix('=')
            }
        })
    })
}

/// An agent process competing for the transcripts recorded for its cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimant {
    pub pid: u32,
    pub start_time: u64,
    /// Transcript named on the command line (`--resume`, `codex resume`).
    pub explicit: Option<PathBuf>,
}

/// Share the transcripts of one cwd out between the agent processes of one
/// family running there, so that two agents in the same cwd are never both
/// given the newest file, and an agent never inherits a file whose last write
/// came before it started (unless named on its command line).
///
/// Files are replayed in creation order. Each goes to a process that was
/// already running when it was created, preferring one with no transcript
/// yet, then one whose current transcript went quiet before this file
/// appeared (`/clear` moves on to a new file), then the latest started.
///
/// shortcut: when an agent clears while another in the same cwd has not
/// written yet, the new file goes to the silent one. Timestamps alone cannot
/// tell that from two agents prompted a second apart; upgrade by reading a
/// link between the files (if the agent writes one) or the process's open
/// file descriptors. Likewise a `--resume`d process stays on its resumed
/// file after a `/clear`: a newer file in its cwd may as well belong to an
/// agent that has since exited.
/// A process keeps the most recently written of the files it was given.
/// Processes left without one (`--continue` reopens an older file) take the
/// newest unclaimed file written since they started.
pub fn assign_transcripts(claimants: &[Claimant], files: &[FileTimes]) -> HashMap<u32, PathBuf> {
    let mut owned: HashMap<u32, Vec<&FileTimes>> = HashMap::new();
    let mut taken: Vec<&Path> = Vec::new();
    for c in claimants {
        let explicit = c
            .explicit
            .as_ref()
            .and_then(|e| files.iter().find(|f| &f.path == e))
            .filter(|f| !taken.contains(&f.path.as_path()));
        if let Some(f) = explicit {
            owned.entry(c.pid).or_default().push(f);
            taken.push(&f.path);
        }
    }
    let replayed: Vec<&Claimant> = claimants
        .iter()
        .filter(|c| !owned.contains_key(&c.pid))
        .collect();
    let mut by_creation: Vec<&FileTimes> = files
        .iter()
        .filter(|f| !taken.contains(&f.path.as_path()))
        .collect();
    by_creation.sort_by_key(|f| (f.created, f.modified));
    for file in by_creation {
        let owner = replayed
            .iter()
            .filter(|c| c.start_time <= file.created + CREATED_SLACK_SECS)
            .max_by_key(|c| match owned.get(&c.pid).and_then(|fs| fs.last()) {
                None => (2, c.start_time),
                Some(cur) if cur.modified <= file.created + CREATED_SLACK_SECS => (1, cur.modified),
                Some(_) => (0, c.start_time),
            });
        if let Some(owner) = owner {
            owned.entry(owner.pid).or_default().push(file);
            taken.push(&file.path);
        }
    }
    let mut out: HashMap<u32, PathBuf> = owned
        .iter()
        .filter_map(|(pid, fs)| Some((*pid, fs.iter().max_by_key(|f| f.modified)?.path.clone())))
        .collect();
    for c in &replayed {
        if out.contains_key(&c.pid) {
            continue;
        }
        let reopened = files
            .iter()
            .filter(|f| f.modified >= c.start_time && !taken.contains(&f.path.as_path()))
            .max_by_key(|f| f.modified);
        if let Some(f) = reopened {
            taken.push(&f.path);
            out.insert(c.pid, f.path.clone());
        }
    }
    out
}

/// Claude Code: `<projects>/<cwd-slug>/<session-id>.jsonl`.
pub struct ClaudeDetector {
    pub projects: PathBuf,
    /// Transcript per pid from the last [`Detector::prepare`]; `None` means
    /// the process was considered and given none.
    assigned: Mutex<HashMap<u32, Option<PathBuf>>>,
}

impl ClaudeDetector {
    pub fn new(projects: PathBuf) -> Self {
        ClaudeDetector {
            projects,
            assigned: Mutex::new(HashMap::new()),
        }
    }

    fn claimant(proc: &ProcInfo, dir: &Path, files: &[FileTimes]) -> Claimant {
        // An explicit session wins while the agent keeps writing to it, and
        // from the start until the agent begins another file (`/clear`):
        // a session just resumed by `restore` has written nothing yet, and
        // must still count as running.
        let explicit = flag_value(&proc.argv, &["--resume", "-r", "--session-id"])
            .map(|id| dir.join(format!("{id}.jsonl")))
            .filter(|path| {
                files.iter().any(|f| {
                    &f.path == path
                        && (f.modified >= proc.start_time
                            || !files.iter().any(|other| {
                                &other.path != path
                                    && other.created + CREATED_SLACK_SECS >= proc.start_time
                            }))
                })
            });
        Claimant {
            pid: proc.pid,
            start_time: proc.start_time,
            explicit,
        }
    }

    fn assign(&self, procs: &[&ProcInfo]) -> HashMap<u32, Option<PathBuf>> {
        let mut out: HashMap<u32, Option<PathBuf>> = procs.iter().map(|p| (p.pid, None)).collect();
        let mut by_dir: HashMap<PathBuf, Vec<&ProcInfo>> = HashMap::new();
        for proc in procs {
            if let Some(cwd) = proc.cwd.as_deref() {
                let dir = transcript::claude_project_dir(&self.projects, cwd);
                by_dir.entry(dir).or_default().push(proc);
            }
        }
        for (dir, procs) in by_dir {
            let files = transcript::files_with_times(&dir, "jsonl");
            let claimants: Vec<Claimant> = procs
                .iter()
                .map(|p| Self::claimant(p, &dir, &files))
                .collect();
            for (pid, path) in assign_transcripts(&claimants, &files) {
                out.insert(pid, Some(path));
            }
        }
        out
    }

    fn transcript_path(&self, proc: &ProcInfo) -> Option<PathBuf> {
        let mut assigned = self.assigned.lock().unwrap_or_else(|e| e.into_inner());
        if !assigned.contains_key(&proc.pid) {
            // Not seen by `prepare` (a one-off call): judge it on its own.
            let alone = self.assign(&[proc]);
            assigned.extend(alone);
        }
        assigned.get(&proc.pid).cloned().flatten()
    }
}

impl Detector for ClaudeDetector {
    fn kind(&self) -> &'static str {
        "claude"
    }

    fn prepare(&self, procs: &[&ProcInfo]) {
        let fresh = self.assign(procs);
        *self.assigned.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
    }

    fn detect(&self, proc: &ProcInfo) -> Option<AgentStatus> {
        let transcript = transcript::read_claude(&self.transcript_path(proc)?)?;
        Some(status(proc, transcript))
    }
}

/// Session id and recorded cwd of a rollout.
type RolloutMeta = (String, Option<String>);

/// Codex: the `<sessions>/**/rollout-*.jsonl` recorded for the process's cwd,
/// shared out between concurrent Codex processes by [`assign_transcripts`].
pub struct CodexDetector {
    pub sessions: PathBuf,
    /// First-line metadata never changes, so read it once per file.
    meta: Mutex<HashMap<PathBuf, RolloutMeta>>,
    /// Rollout per pid from the last [`Detector::prepare`]; `None` means the
    /// process was considered and given none.
    assigned: Mutex<HashMap<u32, Option<PathBuf>>>,
}

impl CodexDetector {
    pub fn new(sessions: PathBuf) -> Self {
        CodexDetector {
            sessions,
            meta: Mutex::new(HashMap::new()),
            assigned: Mutex::new(HashMap::new()),
        }
    }

    fn meta(&self, path: &Path) -> Option<RolloutMeta> {
        let mut cache = self.meta.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(meta) = cache.get(path) {
            return Some(meta.clone());
        }
        // Only successes are cached: a rollout caught before its first line
        // is complete is read again on the next scan.
        let meta = transcript::codex_meta(path)?;
        cache.insert(path.to_path_buf(), meta.clone());
        Some(meta)
    }

    /// The session id after `resume` in argv.
    fn resumed(proc: &ProcInfo) -> Option<&str> {
        proc.argv
            .iter()
            .position(|a| a == "resume")
            .and_then(|i| proc.argv.get(i + 1))
            .map(String::as_str)
            .filter(|id| !id.starts_with('-'))
    }

    fn assign(&self, procs: &[&ProcInfo]) -> HashMap<u32, Option<PathBuf>> {
        let mut out: HashMap<u32, Option<PathBuf>> = procs.iter().map(|p| (p.pid, None)).collect();
        if procs.is_empty() {
            return out;
        }
        // Newest first, so an explicit id resolves to its latest rollout.
        let rollouts: Vec<(FileTimes, RolloutMeta)> = transcript::codex_rollouts(&self.sessions)
            .into_iter()
            .filter_map(|f| {
                let meta = self.meta(&f.path)?;
                Some((f, meta))
            })
            .collect();
        let mut by_cwd: HashMap<String, Vec<Claimant>> = HashMap::new();
        for proc in procs {
            let Some(cwd) = proc.cwd.as_deref() else {
                continue;
            };
            let explicit = Self::resumed(proc).and_then(|want| {
                rollouts
                    .iter()
                    .find(|(_, (id, _))| id == want)
                    .map(|(f, _)| f.path.clone())
            });
            by_cwd
                .entry(cwd.to_string_lossy().into_owned())
                .or_default()
                .push(Claimant {
                    pid: proc.pid,
                    start_time: proc.start_time,
                    explicit,
                });
        }
        for (cwd, claimants) in by_cwd {
            let files: Vec<FileTimes> = rollouts
                .iter()
                .filter(|(f, (_, rec_cwd))| {
                    rec_cwd.as_deref() == Some(cwd.as_str())
                        || claimants
                            .iter()
                            .any(|c| c.explicit.as_ref() == Some(&f.path))
                })
                .map(|(f, _)| f.clone())
                .collect();
            for (pid, path) in assign_transcripts(&claimants, &files) {
                out.insert(pid, Some(path));
            }
        }
        out
    }

    fn rollout_path(&self, proc: &ProcInfo) -> Option<PathBuf> {
        let mut assigned = self.assigned.lock().unwrap_or_else(|e| e.into_inner());
        if !assigned.contains_key(&proc.pid) {
            // Not seen by `prepare` (a one-off call): judge it on its own.
            let alone = self.assign(&[proc]);
            assigned.extend(alone);
        }
        assigned.get(&proc.pid).cloned().flatten()
    }
}

impl Detector for CodexDetector {
    fn kind(&self) -> &'static str {
        "codex"
    }

    fn prepare(&self, procs: &[&ProcInfo]) {
        let fresh = self.assign(procs);
        *self.assigned.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
    }

    fn detect(&self, proc: &ProcInfo) -> Option<AgentStatus> {
        let path = self.rollout_path(proc)?;
        let (id, _) = self.meta(&path)?;
        Some(status(proc, transcript::read_codex(&path, id)?))
    }
}

/// Declares the detector of a family that has no transcript reader yet.
macro_rules! signal_detector {
    ($($(#[$doc:meta])* $name:ident => $kind:literal;)*) => {$(
        $(#[$doc])*
        pub struct $name;

        impl Detector for $name {
            fn kind(&self) -> &'static str {
                $kind
            }

            fn detect(&self, proc: &ProcInfo) -> Option<AgentStatus> {
                Some(signals_only(proc))
            }
        }
    )*};
}

signal_detector! {
    /// OpenCode: process signals only.
    OpencodeDetector => "opencode";
    /// Gemini CLI: process signals only.
    GeminiDetector => "gemini";
    /// GitHub Copilot CLI: process signals only.
    CopilotDetector => "copilot";
    /// Cursor Agent: process signals only.
    CursorAgentDetector => "cursor-agent";
    /// pi: process signals only.
    PiDetector => "pi";
    /// Aider: process signals only.
    AiderDetector => "aider";
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

    /// One detector per family in [`crate::agents::KNOWN_AGENTS`]; the
    /// transcript readers only when their root is known.
    pub fn detectors(&self) -> Vec<Box<dyn Detector>> {
        let mut out: Vec<Box<dyn Detector>> = Vec::new();
        if let Some(p) = &self.claude_projects {
            out.push(Box::new(ClaudeDetector::new(p.clone())));
        }
        if let Some(s) = &self.codex_sessions {
            out.push(Box::new(CodexDetector::new(s.clone())));
        }
        out.push(Box::new(OpencodeDetector));
        out.push(Box::new(GeminiDetector));
        out.push(Box::new(CopilotDetector));
        out.push(Box::new(CursorAgentDetector));
        out.push(Box::new(PiDetector));
        out.push(Box::new(AiderDetector));
        out
    }
}

/// Give every detector the processes of its family for this scan.
pub fn prepare(detectors: &[Box<dyn Detector>], procs: &[ProcInfo]) {
    for d in detectors {
        let mine: Vec<&ProcInfo> = procs.iter().filter(|p| p.spec.kind == d.kind()).collect();
        d.prepare(&mine);
    }
}

/// Run the detector for the process's family, falling back to process signals.
pub fn detect(detectors: &[Box<dyn Detector>], proc: &ProcInfo) -> AgentStatus {
    detectors
        .iter()
        .filter(|d| d.kind() == proc.spec.kind)
        .find_map(|d| d.detect(proc))
        .unwrap_or_else(|| signals_only(proc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::KNOWN_AGENTS;

    fn file(name: &str, created: u64, modified: u64) -> FileTimes {
        FileTimes {
            path: PathBuf::from(name),
            created,
            modified,
        }
    }

    fn claimant(pid: u32, start_time: u64) -> Claimant {
        Claimant {
            pid,
            start_time,
            explicit: None,
        }
    }

    fn owner(map: &HashMap<u32, PathBuf>, pid: u32) -> Option<&str> {
        map.get(&pid).and_then(|p| p.to_str())
    }

    #[test]
    fn every_family_has_exactly_one_detector() {
        let roots = Roots {
            claude_projects: Some("/p".into()),
            codex_sessions: Some("/s".into()),
        };
        let detectors = roots.detectors();
        for spec in KNOWN_AGENTS {
            let n = detectors.iter().filter(|d| d.kind() == spec.kind).count();
            assert_eq!(n, 1, "{} has {n} detectors", spec.kind);
        }
        assert_eq!(detectors.len(), KNOWN_AGENTS.len());
    }

    #[test]
    fn two_claudes_in_one_cwd_get_their_own_transcripts() {
        // A started at 100 and wrote a.jsonl; B started at 200 and wrote
        // b.jsonl. A is still the newest writer.
        let files = [
            file("a", 110, 400),
            file("b", 210, 300),
            file("old", 10, 20),
        ];
        let map = assign_transcripts(&[claimant(1, 100), claimant(2, 200)], &files);
        assert_eq!(owner(&map, 1), Some("a"));
        assert_eq!(owner(&map, 2), Some("b"));
    }

    #[test]
    fn a_process_without_its_own_transcript_gets_none() {
        // B has not been prompted yet, so it has written nothing.
        let files = [file("a", 110, 400)];
        let map = assign_transcripts(&[claimant(1, 100), claimant(2, 200)], &files);
        assert_eq!(owner(&map, 1), Some("a"));
        assert_eq!(owner(&map, 2), None);
    }

    #[test]
    fn clear_moves_a_process_to_its_new_transcript() {
        // A cleared at 300 (a1 went quiet, a2 began) while B kept writing b.
        let files = [
            file("a1", 110, 290),
            file("b", 210, 500),
            file("a2", 300, 450),
        ];
        let map = assign_transcripts(&[claimant(1, 100), claimant(2, 200)], &files);
        assert_eq!(owner(&map, 1), Some("a2"));
        assert_eq!(owner(&map, 2), Some("b"));
    }

    #[test]
    fn codex_meta_is_read_again_until_the_first_line_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout-x.jsonl");
        std::fs::write(&path, r#"{"type":"session_meta","payload":{"id":"s1","#).unwrap();
        let codex = CodexDetector::new(dir.path().into());
        assert_eq!(codex.meta(&path), None);
        std::fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"s1\",\"cwd\":\"/w\"}}\n",
        )
        .unwrap();
        assert_eq!(codex.meta(&path), Some(("s1".into(), Some("/w".into()))));
    }

    #[test]
    fn a_newcomer_just_started_takes_the_new_file() {
        // B started at 298, after A last wrote at 290: a2 is B's first file.
        let files = [file("a1", 110, 290), file("a2", 300, 450)];
        let map = assign_transcripts(&[claimant(1, 100), claimant(2, 298)], &files);
        assert_eq!(owner(&map, 1), Some("a1"));
        assert_eq!(owner(&map, 2), Some("a2"));
    }

    #[test]
    fn explicit_resume_and_continue_reuse_older_files() {
        let files = [
            file("resumed", 10, 300),
            file("continued", 20, 250),
            file("other", 30, 40),
        ];
        let mut resumer = claimant(1, 100);
        resumer.explicit = Some(PathBuf::from("resumed"));
        let map = assign_transcripts(&[resumer, claimant(2, 200)], &files);
        assert_eq!(owner(&map, 1), Some("resumed"));
        assert_eq!(owner(&map, 2), Some("continued"));
    }
}
