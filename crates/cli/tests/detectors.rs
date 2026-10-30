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
fn claude_blocked_agent_says_what_it_waits_on() {
    let tmp = tempfile::tempdir().unwrap();
    install_claude(tmp.path(), "/work/proj", "pending-tool", "s1", T0);
    let detector = ClaudeDetector::new(tmp.path().to_path_buf());
    let blocked = detector
        .detect(&proc("claude", "/work/proj", T0 + 30, 0.0))
        .unwrap();
    assert_eq!(blocked.state, AgentState::Blocked);
    assert_eq!(blocked.waiting.as_deref(), Some("Bash: cargo test"));
    // The same call while the tool runs is no wait at all.
    let working = detector
        .detect(&proc("claude", "/work/proj", T0 + 30, 85.0))
        .unwrap();
    assert_eq!(working.state, AgentState::Working);
    assert_eq!(working.waiting, None);
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
fn claude_just_resumed_reports_its_session_before_writing() {
    // `restore` reopened `resumed`, last written before the process started.
    let tmp = tempfile::tempdir().unwrap();
    install_claude(tmp.path(), "/work/proj", "turn-ended", "resumed", T0 - 100);
    install_claude(tmp.path(), "/work/proj", "pending-tool", "older", T0 - 200);
    let detector = ClaudeDetector::new(tmp.path().to_path_buf());
    let mut p = proc("claude", "/work/proj", T0 + 30, 0.0);
    p.start_time = T0;
    p.argv = vec!["claude".into(), "--resume".into(), "resumed".into()];
    let status = detector.detect(&p).unwrap();
    assert_eq!(status.session_id.as_deref(), Some("resumed"));
    assert_eq!(status.state, AgentState::Idle);

    // Once it has moved on to a new file (`/clear`), that file is its own.
    install_claude(tmp.path(), "/work/proj", "pending-tool", "cleared", T0 + 10);
    let status = ClaudeDetector::new(tmp.path().to_path_buf())
        .detect(&p)
        .unwrap();
    assert_eq!(status.session_id.as_deref(), Some("cleared"));
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
    assert_eq!(status.waiting.as_deref(), Some("shell: make"));

    let status = detector
        .detect(&proc("codex", "/work/codex-proj", T0 + 30, 50.0))
        .unwrap();
    assert_eq!(status.state, AgentState::Working);
    assert_eq!(status.waiting, None);
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

#[test]
fn scanner_reports_claude_token_totals_as_they_grow() {
    let tmp = tempfile::tempdir().unwrap();
    install_claude(tmp.path(), "/work/proj", "turn-ended", "s1", T0 - 1);
    let path = tmp
        .path()
        .join(claude_slug(Path::new("/work/proj")))
        .join("s1.jsonl");
    let reply = r#"{"type":"assistant","message":{"usage":{"input_tokens":3,"output_tokens":4,"cache_read_input_tokens":1000,"cache_creation_input_tokens":20}}}"#;
    let mut scanner = Scanner::with_detectors(
        "box".into(),
        vec![Box::new(ClaudeDetector::new(tmp.path().to_path_buf()))],
    );
    let mut agent = raw(101, 1, &["claude"], 0.0);
    agent.cwd = Some("/work/proj".into());

    // The fixture records no usage.
    let scan = scanner.scan_from(std::slice::from_ref(&agent), T0 + 30);
    assert_eq!(scan.agents[0].tokens, None);

    for _ in 0..2 {
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str(reply);
        text.push('\n');
        fs::write(&path, text).unwrap();
        set_mtime(&path, T0 - 1);
    }
    let scan = scanner.scan_from(std::slice::from_ref(&agent), T0 + 31);
    let tokens = scan.agents[0].tokens.unwrap();
    assert_eq!(
        (tokens.input, tokens.cache_read, tokens.total()),
        (6, 2000, 2054)
    );
}

/// Write Claude's `<sessions>/<pid>.json`, with a field zj-herdr must ignore.
fn write_session_file(sessions: &Path, pid: u32, id: &str, cwd: &str, started_at_secs: u64) {
    fs::create_dir_all(sessions).unwrap();
    let body = serde_json::json!({
        "pid": pid,
        "sessionId": id,
        "cwd": cwd,
        "startedAt": started_at_secs * 1000 + 250,
        "kind": "interactive",
    });
    fs::write(sessions.join(format!("{pid}.json")), body.to_string()).unwrap();
}

/// `<tmp>/projects` and `<tmp>/sessions`, like a Claude config dir.
fn claude_config() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let projects = tmp.path().join("projects");
    let sessions = tmp.path().join("sessions");
    fs::create_dir_all(&projects).unwrap();
    (tmp, projects, sessions)
}

fn claude_with_sessions(projects: &Path, sessions: &Path) -> ClaudeDetector {
    let mut detector = ClaudeDetector::new(projects.to_path_buf());
    detector.sessions = Some(sessions.to_path_buf());
    detector
}

#[test]
fn claude_session_file_wins_over_timestamps() {
    let (_tmp, projects, sessions) = claude_config();
    install_claude(&projects, "/work/proj", "turn-ended", "mine", T0 - 100);
    install_claude(&projects, "/work/proj", "pending-tool", "newer", T0);
    let p = proc("claude", "/work/proj", T0 + 30, 0.0);
    write_session_file(&sessions, p.pid, "mine", "/work/proj", p.start_time);
    let status = claude_with_sessions(&projects, &sessions)
        .detect(&p)
        .unwrap();
    assert_eq!(status.session_id.as_deref(), Some("mine"));
    assert_eq!(status.state, AgentState::Idle);
}

#[test]
fn claude_session_file_reports_its_id_before_the_transcript_exists() {
    let (_tmp, projects, sessions) = claude_config();
    install_claude(&projects, "/work/proj", "pending-tool", "other", T0);
    let p = proc("claude", "/work/proj", T0 + 30, 0.0);
    write_session_file(&sessions, p.pid, "fresh", "/work/proj", p.start_time);
    let status = claude_with_sessions(&projects, &sessions)
        .detect(&p)
        .unwrap();
    assert_eq!(status.session_id.as_deref(), Some("fresh"));
    assert_eq!(status.state, AgentState::Idle);
}

#[test]
fn claude_session_file_of_an_earlier_process_is_ignored() {
    let (_tmp, projects, sessions) = claude_config();
    install_claude(&projects, "/work/proj", "turn-ended", "stale", T0 - 100);
    install_claude(&projects, "/work/proj", "pending-tool", "newer", T0);
    let p = proc("claude", "/work/proj", T0 + 30, 0.0);
    // Same pid, but written by a process that started a minute earlier.
    write_session_file(&sessions, p.pid, "stale", "/work/proj", p.start_time - 60);
    let status = claude_with_sessions(&projects, &sessions)
        .detect(&p)
        .unwrap();
    assert_eq!(status.session_id.as_deref(), Some("newer"));
}

#[test]
fn claude_session_file_with_a_bad_id_is_ignored() {
    let (_tmp, projects, sessions) = claude_config();
    install_claude(&projects, "/work/proj", "pending-tool", "newer", T0);
    let p = proc("claude", "/work/proj", T0 + 30, 0.0);
    write_session_file(&sessions, p.pid, "--evil", "/work/proj", p.start_time);
    let status = claude_with_sessions(&projects, &sessions)
        .detect(&p)
        .unwrap();
    assert_eq!(status.session_id.as_deref(), Some("newer"));
}

#[test]
fn claude_session_file_finds_a_transcript_in_a_sibling_project_dir() {
    // Started in /work/proj, then moved into a worktree: the transcript
    // sits under the worktree's slug.
    let (_tmp, projects, sessions) = claude_config();
    install_claude(
        &projects,
        "/work/proj/.worktrees/x",
        "pending-tool",
        "wt",
        T0,
    );
    install_claude(&projects, "/work/proj", "turn-ended", "newer", T0 + 5);
    let p = proc("claude", "/work/proj", T0 + 30, 0.0);
    write_session_file(&sessions, p.pid, "wt", "/work/proj", p.start_time);
    let status = claude_with_sessions(&projects, &sessions)
        .detect(&p)
        .unwrap();
    assert_eq!(status.session_id.as_deref(), Some("wt"));
    assert_eq!(status.state, AgentState::Blocked);
}

#[test]
fn claude_session_file_search_is_not_repeated_every_scan() {
    // Nothing written yet anywhere: the project directories are searched
    // once, then not again for a while, however often the daemon scans.
    let (_tmp, projects, sessions) = claude_config();
    let p = proc("claude", "/work/proj", T0 + 30, 0.0);
    write_session_file(&sessions, p.pid, "late", "/work/proj", p.start_time);
    let detector = claude_with_sessions(&projects, &sessions);
    detector.prepare(&[&p]);
    assert_eq!(detector.detect(&p).unwrap().state, AgentState::Idle);
    install_claude(
        &projects,
        "/work/proj/.worktrees/x",
        "pending-tool",
        "late",
        T0,
    );
    detector.prepare(&[&p]);
    assert_eq!(detector.detect(&p).unwrap().state, AgentState::Idle);
}

#[test]
fn scanner_keeps_session_file_transcripts_out_of_the_shared_pool() {
    let (_tmp, projects, sessions) = claude_config();
    install_claude(&projects, "/work/proj", "pending-tool", "s1", T0 - 1);
    install_claude(&projects, "/work/proj", "turn-ended", "s2", T0 - 2);
    let mut scanner = Scanner::with_detectors(
        "box".into(),
        vec![Box::new(claude_with_sessions(&projects, &sessions))],
    );
    let claude = |pid| {
        let mut p = raw(pid, 1, &["claude"], 0.0);
        p.cwd = Some("/work/proj".into());
        p
    };
    // 101 says it writes s1, the newest file, which timestamps alone would
    // have given to either process.
    write_session_file(&sessions, 101, "s1", "/work/proj", T0 - 3600);
    let scan = scanner.scan_from(&[claude(101), claude(102)], T0 + 30);
    let id = |pid: u32| {
        scan.agents
            .iter()
            .find(|a| a.pid == pid)
            .unwrap()
            .session_id
            .as_deref()
    };
    assert_eq!(id(101), Some("s1"));
    assert_eq!(id(102), Some("s2"));
}

/// A Claude transcript whose turn ended after launching `command` in the
/// background at Unix second `at`, as `<projects>/<slug>/s1.jsonl`.
fn claude_background(projects: &Path, command: &str, at: &str) {
    let dir = projects.join(claude_slug(Path::new("/work/proj")));
    fs::create_dir_all(&dir).unwrap();
    let lines = [
        format!(
            r#"{{"type":"assistant","timestamp":"{at}","message":{{"content":[{{"type":"tool_use","id":"t1","name":"Bash","input":{{"command":"{command}","run_in_background":true}}}}]}}}}"#
        ),
        format!(
            r#"{{"type":"user","timestamp":"{at}","message":{{"content":[{{"type":"tool_result","tool_use_id":"t1"}}]}},"toolUseResult":{{"backgroundTaskId":"b1"}}}}"#
        ),
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"started"}]}}"#.into(),
    ];
    let path = dir.join("s1.jsonl");
    fs::write(&path, lines.join("\n") + "\n").unwrap();
    set_mtime(&path, T0);
}

#[test]
fn claude_waiting_on_background_work_is_working_but_not_on_a_server() {
    let status = |command: &str| {
        let tmp = tempfile::tempdir().unwrap();
        claude_background(tmp.path(), command, "2025-06-15T15:06:40Z");
        let detector = ClaudeDetector::new(tmp.path().to_path_buf());
        detector
            .detect(&proc("claude", "/work/proj", T0 + 60, 0.0))
            .unwrap()
    };
    let make = status("make");
    assert_eq!(make.state, AgentState::Working, "its turn waits on make");
    assert_eq!(make.tasks.len(), 1);
    assert_eq!(make.tasks[0].label, "make");
    let server = status("npm run dev");
    assert_eq!(server.state, AgentState::Idle, "a dev server never ends");
    assert_eq!(server.tasks[0].label, "npm run dev");
}

#[test]
fn claude_session_file_says_background_spare_and_name() {
    let (_tmp, projects, sessions) = claude_config();
    install_claude(&projects, "/work/proj", "turn-ended", "s1", T0);
    let p = proc("claude", "/work/proj", T0 + 30, 0.0);
    let write = |extra: serde_json::Value| {
        let mut body = serde_json::json!({
            "pid": p.pid,
            "sessionId": "s1",
            "cwd": "/work/proj",
            "startedAt": p.start_time * 1000,
        });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        fs::create_dir_all(&sessions).unwrap();
        fs::write(sessions.join(format!("{}.json", p.pid)), body.to_string()).unwrap();
    };
    let detector = claude_with_sessions(&projects, &sessions);
    write(serde_json::json!({"kind": "bg", "name": "api\nwork"}));
    let status = detector.detect(&p).unwrap();
    assert!(status.background && !status.spare);
    assert_eq!(status.title.as_deref(), Some("api work"), "one line");
    write(serde_json::json!({"kind": "interactive"}));
    let status = detector.detect(&p).unwrap();
    assert!(!status.background && status.title.is_none());
    write(serde_json::json!({"kind": "bg", "spare": true}));
    assert!(detector.detect(&p).unwrap().spare);
}

#[test]
fn scanner_drops_spare_claude_sessions() {
    let (_tmp, projects, sessions) = claude_config();
    let mut raw = RawProc {
        pid: 4242,
        ppid: Some(1),
        name: "claude".into(),
        argv: vec!["claude".into()],
        cwd: Some(PathBuf::from("/work/proj")),
        start_time: T0,
        ..RawProc::default()
    };
    let detectors: Vec<Box<dyn Detector>> =
        vec![Box::new(claude_with_sessions(&projects, &sessions))];
    let mut scanner = Scanner::with_detectors("box".into(), detectors);
    let body = |spare: bool| {
        serde_json::json!({"pid": 4242, "sessionId": "s1", "cwd": "/work/proj",
            "startedAt": T0 * 1000, "kind": "bg", "spare": spare})
        .to_string()
    };
    fs::create_dir_all(&sessions).unwrap();
    fs::write(sessions.join("4242.json"), body(true)).unwrap();
    assert!(scanner.scan_from(&[raw.clone()], T0 + 5).agents.is_empty());
    fs::write(sessions.join("4242.json"), body(false)).unwrap();
    raw.cpu = 0.0;
    let agents = scanner.scan_from(&[raw], T0 + 6).agents;
    assert_eq!(agents.len(), 1);
    assert!(agents[0].background);
}

#[test]
fn a_transcript_that_cannot_be_read_gives_no_status() {
    let tmp = tempfile::tempdir().unwrap();
    // A directory where the transcript should be: listed, but not readable.
    let dir = tmp.path().join(claude_slug(Path::new("/work/proj")));
    fs::create_dir_all(dir.join("s1.jsonl")).unwrap();
    let detector = ClaudeDetector::new(tmp.path().to_path_buf());
    assert!(detector
        .detect(&proc("claude", "/work/proj", T0, 0.0))
        .is_none());
}

#[test]
fn a_codex_without_a_cwd_gets_no_rollout_and_a_missing_tree_has_none() {
    let (_tmp, detector) = codex_tree();
    let mut p = proc("codex", "/work/codex-proj", T0, 0.0);
    p.cwd = None;
    detector.prepare(&[&p]);
    assert!(detector.detect(&p).is_none());
    assert!(zj_herdr::transcript::codex_rollouts(Path::new("/nonexistent/zj-herdr")).is_empty());
}
