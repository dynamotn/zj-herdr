//! Detector state rules, driven by the fixture transcripts in `fixtures/`.
//! Every test builds its own transcript tree in a temp dir; nothing here
//! looks at the real `~/.claude` or `~/.codex`.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use zj_herdr::agents;
use zj_herdr::detect::{self, AiderDetector, ClaudeDetector, CodexDetector, Detector};
use zj_herdr::procs::{ProcInfo, RawProc};
use zj_herdr::scan::Scanner;
use zj_herdr::transcript::claude_slug;
use zj_herdr_core::{AgentState, Pane};

const T0: u64 = 1_750_000_000;
const CODEX_NEW: &str = "rollout-2025-05-01T10-00-00-0196a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.jsonl";
const CODEX_OLD: &str = "rollout-2025-05-01T09-00-00-0196a1b2-0000-7e5f-8a9b-0c1d2e3f4a5b.jsonl";
const CODEX_OTHER: &str = "rollout-2025-04-30T08-00-00-0196a1b2-ffff-7e5f-8a9b-0c1d2e3f4a5b.jsonl";

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(rel)
}

fn set_mtime(path: &Path, secs: u64) {
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap();
}

/// Copy a Claude fixture to `<projects>/<slug(cwd)>/<id>.jsonl`.
fn install_claude(projects: &Path, cwd: &str, name: &str, id: &str, mtime: u64) {
    let dir = projects.join(claude_slug(Path::new(cwd)));
    fs::create_dir_all(&dir).unwrap();
    let dest = dir.join(format!("{id}.jsonl"));
    fs::copy(fixture(&format!("claude/{name}.jsonl")), &dest).unwrap();
    set_mtime(&dest, mtime);
}

/// Copy a Codex fixture into the dated `sessions/YYYY/MM/DD` layout.
fn install_codex(sessions: &Path, name: &str, mtime: u64) {
    let dir = sessions.join("2025/05/01");
    fs::create_dir_all(&dir).unwrap();
    let dest = dir.join(name);
    fs::copy(fixture(&format!("codex/{name}")), &dest).unwrap();
    set_mtime(&dest, mtime);
}

fn proc(kind: &str, cwd: &str, sampled_at: u64, cpu: f32) -> ProcInfo {
    ProcInfo {
        pid: 4242,
        spec: agents::spec(kind).unwrap(),
        argv: vec![kind.to_string()],
        cwd: Some(PathBuf::from(cwd)),
        pane: None,
        cpu,
        start_time: T0 - 3600,
        sampled_at,
        cpu_quiet_for: None,
    }
}

fn claude_state(name: &str, age: u64, cpu: f32) -> AgentState {
    let tmp = tempfile::tempdir().unwrap();
    install_claude(tmp.path(), "/work/proj", name, "s1", T0);
    let detector = ClaudeDetector::new(tmp.path().to_path_buf());
    detector
        .detect(&proc("claude", "/work/proj", T0 + age, cpu))
        .unwrap()
        .state
}

#[test]
fn claude_pending_tool_with_quiet_process_is_blocked() {
    assert_eq!(claude_state("pending-tool", 30, 0.0), AgentState::Blocked);
}

#[test]
fn claude_pending_tool_with_busy_tree_is_working() {
    // The tool itself (e.g. `cargo test`) is burning CPU under the agent.
    assert_eq!(claude_state("pending-tool", 30, 85.0), AgentState::Working);
}

#[test]
fn claude_pending_tool_just_written_is_working() {
    assert_eq!(claude_state("pending-tool", 1, 0.0), AgentState::Working);
}

#[test]
fn claude_pending_tool_recently_busy_is_working() {
    let tmp = tempfile::tempdir().unwrap();
    install_claude(tmp.path(), "/work/proj", "pending-tool", "s1", T0);
    let detector = ClaudeDetector::new(tmp.path().to_path_buf());
    let mut p = proc("claude", "/work/proj", T0 + 30, 0.0);
    p.cpu_quiet_for = Some(2);
    assert_eq!(detector.detect(&p).unwrap().state, AgentState::Working);
    p.cpu_quiet_for = Some(20);
    assert_eq!(detector.detect(&p).unwrap().state, AgentState::Blocked);
}

#[test]
fn claude_ended_turn_is_idle_once_quiet() {
    assert_eq!(claude_state("turn-ended", 60, 0.0), AgentState::Idle);
    assert_eq!(claude_state("turn-ended", 1, 0.0), AgentState::Working);
    assert_eq!(claude_state("turn-ended", 60, 12.0), AgentState::Working);
}

#[test]
fn claude_awaiting_reply_is_working_while_active() {
    assert_eq!(claude_state("awaiting-reply", 2, 0.0), AgentState::Working);
    assert_eq!(
        claude_state("awaiting-reply", 120, 5.0),
        AgentState::Working
    );
    assert_eq!(claude_state("awaiting-reply", 120, 0.0), AgentState::Idle);
}

#[test]
fn claude_interrupted_turn_is_idle() {
    assert_eq!(claude_state("interrupted", 30, 0.0), AgentState::Idle);
}

#[test]
fn claude_reports_newest_transcript_as_session() {
    let tmp = tempfile::tempdir().unwrap();
    install_claude(tmp.path(), "/work/proj", "turn-ended", "older", T0 - 100);
    install_claude(tmp.path(), "/work/proj", "pending-tool", "newer", T0);
    install_claude(tmp.path(), "/work/other", "turn-ended", "other", T0 + 5);
    let detector = ClaudeDetector::new(tmp.path().to_path_buf());
    let status = detector
        .detect(&proc("claude", "/work/proj", T0 + 30, 0.0))
        .unwrap();
    assert_eq!(status.session_id.as_deref(), Some("newer"));
    assert_eq!(status.state, AgentState::Blocked);
    assert_eq!(status.last_activity, Some(T0));
}

#[test]
fn claude_resume_flag_selects_its_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    install_claude(tmp.path(), "/work/proj", "turn-ended", "resumed", T0 - 100);
    install_claude(tmp.path(), "/work/proj", "pending-tool", "newer", T0);
    let detector = ClaudeDetector::new(tmp.path().to_path_buf());
    let mut p = proc("claude", "/work/proj", T0 + 30, 0.0);
    p.argv = vec!["claude".into(), "--resume".into(), "resumed".into()];
    let status = detector.detect(&p).unwrap();
    assert_eq!(status.session_id.as_deref(), Some("resumed"));
    assert_eq!(status.state, AgentState::Idle);
}

#[test]
fn claude_without_transcript_defers_to_process_signals() {
    let tmp = tempfile::tempdir().unwrap();
    let detectors: Vec<Box<dyn Detector>> =
        vec![Box::new(ClaudeDetector::new(tmp.path().to_path_buf()))];
    let status = detect::detect(&detectors, &proc("claude", "/nowhere", T0, 0.0));
    assert_eq!(status.state, AgentState::Idle);
    assert_eq!(status.session_id, None);
}

fn codex_tree() -> (tempfile::TempDir, CodexDetector) {
    let tmp = tempfile::tempdir().unwrap();
    install_codex(tmp.path(), CODEX_NEW, T0);
    install_codex(tmp.path(), CODEX_OLD, T0 - 3600);
    install_codex(tmp.path(), CODEX_OTHER, T0 + 10);
    let detector = CodexDetector::new(tmp.path().to_path_buf());
    (tmp, detector)
}

#[test]
fn codex_newest_rollout_for_cwd_decides_state() {
    let (_tmp, detector) = codex_tree();
    let status = detector
        .detect(&proc("codex", "/work/codex-proj", T0 + 30, 0.0))
        .unwrap();
    assert_eq!(
        status.session_id.as_deref(),
        Some("0196a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b")
    );
    assert_eq!(status.state, AgentState::Blocked);

    let status = detector
        .detect(&proc("codex", "/work/codex-proj", T0 + 30, 50.0))
        .unwrap();
    assert_eq!(status.state, AgentState::Working);
}

#[test]
fn codex_completed_task_is_idle() {
    let (_tmp, detector) = codex_tree();
    let status = detector
        .detect(&proc("codex", "/work/elsewhere", T0 + 60, 0.0))
        .unwrap();
    assert_eq!(
        status.session_id.as_deref(),
        Some("0196a1b2-ffff-7e5f-8a9b-0c1d2e3f4a5b")
    );
    assert_eq!(status.state, AgentState::Idle);
}

#[test]
fn codex_resume_argument_selects_rollout() {
    let (_tmp, detector) = codex_tree();
    let mut p = proc("codex", "/work/codex-proj", T0 + 30, 0.0);
    p.argv = vec![
        "codex".into(),
        "resume".into(),
        "0196a1b2-0000-7e5f-8a9b-0c1d2e3f4a5b".into(),
    ];
    let status = detector.detect(&p).unwrap();
    assert_eq!(
        status.session_id.as_deref(),
        Some("0196a1b2-0000-7e5f-8a9b-0c1d2e3f4a5b")
    );
    assert_eq!(status.state, AgentState::Idle);
}

#[test]
fn codex_unknown_cwd_has_no_opinion() {
    let (_tmp, detector) = codex_tree();
    assert!(detector
        .detect(&proc("codex", "/work/unknown", T0, 0.0))
        .is_none());
}

#[test]
fn other_agents_use_cpu_only() {
    assert_eq!(
        AiderDetector
            .detect(&proc("aider", "/w", T0, 30.0))
            .unwrap()
            .state,
        AgentState::Working
    );
    assert_eq!(
        AiderDetector
            .detect(&proc("aider", "/w", T0, 0.1))
            .unwrap()
            .state,
        AgentState::Idle
    );
}

fn raw(pid: u32, ppid: u32, argv: &[&str], cpu: f32) -> RawProc {
    RawProc {
        pid,
        ppid: Some(ppid),
        name: argv[0].to_string(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        cpu,
        start_time: T0 - 3600,
        ..RawProc::default()
    }
}

#[test]
fn scanner_tracks_cpu_history_between_scans() {
    let tmp = tempfile::tempdir().unwrap();
    // Written 1s before the first scan, so the transcript itself is fresh.
    install_claude(tmp.path(), "/work/proj", "pending-tool", "s1", T0 - 1);
    let mut scanner = Scanner::with_detectors(
        "box".into(),
        vec![Box::new(ClaudeDetector::new(tmp.path().to_path_buf()))],
    );

    let mut shell = raw(100, 1, &["zsh"], 0.0);
    shell.pane = Some(Pane::new("dev", 4));
    let mut agent = raw(101, 100, &["claude"], 0.5);
    agent.cwd = Some("/work/proj".into());
    let tool = raw(102, 101, &["cargo"], 90.0);

    let first = scanner.scan_from(&[shell.clone(), agent.clone(), tool], T0);
    let a = &first.agents[0];
    assert_eq!((a.id.as_str(), a.state), ("box/101", AgentState::Working));
    assert_eq!(a.pane, Some(Pane::new("dev", 4)));
    assert_eq!(a.session_id.as_deref(), Some("s1"));

    // Tool finished, permission prompt for the next one: quiet for 3s only.
    let quiet = scanner.scan_from(&[shell.clone(), agent.clone()], T0 + 3);
    assert_eq!(quiet.agents[0].state, AgentState::Working);

    let blocked = scanner.scan_from(&[shell, agent], T0 + 30);
    assert_eq!(blocked.agents[0].state, AgentState::Blocked);
    assert_eq!(blocked.agents[0].since, T0 - 1);
}

#[test]
fn scanner_never_gives_two_claudes_the_same_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    install_claude(tmp.path(), "/work/proj", "pending-tool", "s1", T0 - 1);
    install_claude(tmp.path(), "/work/proj", "turn-ended", "s2", T0 - 2);
    let mut scanner = Scanner::with_detectors(
        "box".into(),
        vec![Box::new(ClaudeDetector::new(tmp.path().to_path_buf()))],
    );
    let claude = |pid| {
        let mut p = raw(pid, 1, &["claude"], 0.0);
        p.cwd = Some("/work/proj".into());
        p
    };

    let scan = scanner.scan_from(&[claude(101), claude(102), claude(103)], T0 + 30);
    let mut ids: Vec<Option<&str>> = scan
        .agents
        .iter()
        .map(|x| x.session_id.as_deref())
        .collect();
    ids.sort();
    assert_eq!(ids, vec![None, Some("s1"), Some("s2")]);
}

#[test]
fn scanner_shares_codex_rollouts_between_agents_in_one_cwd() {
    let (tmp, _) = codex_tree();
    let mut scanner = Scanner::with_detectors(
        "box".into(),
        vec![Box::new(CodexDetector::new(tmp.path().to_path_buf()))],
    );
    let codex = |pid, start_time| {
        let mut p = raw(pid, 1, &["codex"], 0.0);
        p.cwd = Some("/work/codex-proj".into());
        p.start_time = start_time;
        p
    };
    // 101 wrote the old rollout, 102 the new one; 103 started after both
    // were last written and has not written anything yet.
    let procs = [
        codex(101, T0 - 7200),
        codex(102, T0 - 1800),
        codex(103, T0 + 100),
    ];
    let scan = scanner.scan_from(&procs, T0 + 200);
    let id = |pid: u32| {
        scan.agents
            .iter()
            .find(|a| a.pid == pid)
            .unwrap()
            .session_id
            .as_deref()
    };
    assert_eq!(id(101), Some("0196a1b2-0000-7e5f-8a9b-0c1d2e3f4a5b"));
    assert_eq!(id(102), Some("0196a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"));
    assert_eq!(id(103), None);
}

#[test]
fn codex_started_after_its_cwds_rollouts_inherits_none() {
    let (_tmp, detector) = codex_tree();
    let mut p = proc("codex", "/work/codex-proj", T0 + 200, 0.0);
    p.start_time = T0 + 100;
    assert!(detector.detect(&p).is_none());
}
