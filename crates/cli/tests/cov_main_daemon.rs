//! The CLI client commands against a real daemon whose world is fake: a
//! `zellij` and an `ssh` script on PATH, a stand-in `claude` process and a
//! private HOME, so `wait`, `ack`, `adopt`, `watch` and `restore` see agents.

use std::io::{BufRead, BufReader, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use zj_herdr::sessions::Store;
use zj_herdr_core::{Agent, AgentState, Machine, MachineStatus, Pane, StateV1};

const SESSION_ID: &str = "turn-ended";

struct World {
    dir: tempfile::TempDir,
    daemon: Option<Child>,
    agent: Option<u32>,
}

impl World {
    fn path(&self, sub: &str) -> PathBuf {
        self.dir.path().join(sub)
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_zj-herdr"));
        cmd.arg("--socket")
            .arg(self.path("run").join("zj-herdr.sock"))
            .args(args)
            .env("PATH", path)
            .env("HOME", self.path("home"))
            .env("TMPDIR", self.path("tmp"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_RUNTIME_DIR", self.path("run"))
            .env("CLAUDE_CONFIG_DIR", self.path("claude"))
            .env("CODEX_HOME", self.path("codex"))
            .env("FAKE_DIR", self.dir.path())
            .env_remove("ZJ_HERDR_SOCKET")
            .env_remove("ZELLIJ")
            .env_remove("ZELLIJ_SESSION_NAME");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        if let Some(pid) = self.agent {
            // SAFETY: plain kill(2) on the stand-in agent this test started.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        }
    }
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn eventually(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn recorded(pane: u32, sid: &str) -> Agent {
    Agent {
        id: format!("box/{pane}"),
        machine: "box".into(),
        kind: "claude".into(),
        pid: pane,
        cwd: Some("/w".into()),
        state: AgentState::Idle,
        since: 0,
        pane: Some(Pane::new("dev", pane)),
        session_id: Some(sid.into()),
        waiting: None,
        started: 0,
        tokens: None,
        error: false,
        acked: false,
        tasks: vec![],
        model: None,
        context: None,
        background: false,
        title: None,
    }
}

#[test]
fn clients_drive_a_daemon_that_sees_a_local_and_a_remote_machine() {
    let mut w = World {
        dir: tempfile::tempdir().unwrap(),
        daemon: None,
        agent: None,
    };
    for sub in ["bin", "home", "tmp", "state", "config", "run", "work"] {
        std::fs::create_dir_all(w.path(sub)).unwrap();
    }
    let log = w.path("zellij.log");
    script(
        &w.path("bin").join("zellij"),
        r#"echo "$@" >> "$FAKE_DIR/zellij.log"
case "$*" in
  list-sessions*) echo dev ;;
  *list-clients*) printf 'CLIENT_ID\n1 terminal_0 claude\n' ;;
  *list-panes*) echo '[]' ;;
esac
"#,
    );
    // The remote answers only once `ssh-up` exists.
    let remote_state = |state| {
        let agent = Agent {
            state,
            pane: None,
            ..recorded(7, "far-session")
        };
        StateV1::new(
            1,
            vec![Machine {
                name: "far".into(),
                ssh: None,
                status: MachineStatus::Ok,
                error: None,
                agents: vec![agent],
            }],
        )
        .to_json()
    };
    std::fs::write(w.path("remote.json"), remote_state(AgentState::Idle)).unwrap();
    script(
        &w.path("bin").join("ssh"),
        r#"[ -e "$FAKE_DIR/ssh-up" ] && exec cat "$FAKE_DIR/remote.json"
echo "no route" >&2
exit 1
"#,
    );
    std::fs::write(
        w.path("config").join("c.toml"),
        "[[remote]]\nname = \"far\"\nssh = \"far\"\ninterval_secs = 1\n",
    )
    .unwrap();

    // A stand-in agent: a shell under the name `claude`, blocked reading the
    // stdin this test holds, with the session file and transcript Claude Code
    // would leave. Not `sleep`: a multicall coreutils (uutils, as on Ubuntu
    // 26.04) picks the tool from argv[0] and exits when it is `claude`.
    std::fs::create_dir_all(w.path("agent")).unwrap();
    std::fs::copy("/bin/sh", w.path("agent").join("claude")).unwrap();
    let cwd = w.path("work").canonicalize().unwrap();
    let mut agent = Command::new(w.path("agent").join("claude"))
        .args(["-c", "read line"])
        .stdin(Stdio::piped())
        .current_dir(&cwd)
        .spawn()
        .unwrap();
    let _hold_stdin = agent.stdin.take();
    let pid = agent.id();
    w.agent = Some(pid);
    // Reaped as soon as it dies, as init would: `adopt` waits for the pid to go.
    std::thread::spawn(move || agent.wait());
    let claude = w.path("claude");
    std::fs::create_dir_all(claude.join("sessions")).unwrap();
    let project = claude
        .join("projects")
        .join(zj_herdr::transcript::claude_slug(&cwd));
    std::fs::create_dir_all(&project).unwrap();
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude/turn-ended.jsonl"),
        project.join(format!("{SESSION_ID}.jsonl")),
    )
    .unwrap();
    let started_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    std::fs::write(
        claude.join("sessions").join(format!("{pid}.json")),
        format!(
            r#"{{"pid":{pid},"sessionId":"{SESSION_ID}","cwd":"{}","startedAt":{started_ms}}}"#,
            cwd.display()
        ),
    )
    .unwrap();

    // Two recorded panes: one is the running agent, one is gone.
    let mut store = Store::load(Path::new("/nonexistent")).unwrap();
    store.record(&[recorded(3, SESSION_ID), recorded(4, "other")], 1);
    let state = w.path("state").join("zj-herdr");
    std::fs::create_dir_all(&state).unwrap();
    store.save(&state.join("sessions.json")).unwrap();

    let config = w.path("config").join("c.toml");
    let child = w
        .cmd(&["daemon", "--config", config.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    w.daemon = Some(child);

    let mut id = String::new();
    let mut sandboxed = false;
    eventually("the daemon to see the agent", 20, || {
        let daemon = w.daemon.as_mut().unwrap();
        if let Some(status) = daemon.try_wait().unwrap() {
            let mut e = String::new();
            daemon
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut e)
                .unwrap();
            assert!(
                e.contains("Operation not permitted"),
                "the daemon exited ({status}): {e}"
            );
            sandboxed = true;
            return true;
        }
        let o = w.run(&["list", "--json"]);
        let Ok(state) = StateV1::from_json(out(&o).trim()) else {
            return false;
        };
        id = state
            .agents()
            .find(|a| a.pid == pid)
            .map(|a| a.id.clone())
            .unwrap_or_default();
        !id.is_empty() && !err(&o).contains("falling back")
    });
    if sandboxed {
        eprintln!("skipped: this environment forbids binding Unix sockets");
        return;
    }

    // A text `watch` sees the snapshot, then changes as they come.
    let mut watch = w.cmd(&["watch"]).stdout(Stdio::piped()).spawn().unwrap();
    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let reader = {
        let lines = Arc::clone(&lines);
        let stdout = watch.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                lines.lock().unwrap().push(line);
            }
        })
    };
    let seen = |needle: &str| lines.lock().unwrap().iter().any(|l| l.contains(needle));
    eventually("the watch snapshot", 10, || seen("ID"));
    std::fs::write(w.path("ssh-up"), "").unwrap();
    eventually("the remote to come up", 10, || seen("far ok"));
    std::fs::write(w.path("remote.json"), remote_state(AgentState::Working)).unwrap();
    eventually("the remote agent to change", 10, || {
        seen("far/7 claude idle -> working")
    });

    // restore: the running session is skipped, the other one reopened.
    let o = w.run(&["restore"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(err(&o).contains("is already running"), "{}", err(&o));
    assert!(out(&o).contains("reopened dev:4"), "{}", out(&o));
    assert!(std::fs::read_to_string(&log)
        .unwrap()
        .contains("--session dev run --cwd /w -- claude --resume other"));

    // The agent idles on a finished turn: wait, ack, adopt all answer with it.
    let o = w.run(&["wait", &id, "idle", "--timeout", "10"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(out(&o).contains(&id), "{}", out(&o));
    let o = w.run(&["ack", &id]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(
        out(&o).contains(&format!("marked {id} seen")),
        "{}",
        out(&o)
    );
    let o = w.run(&["adopt", &pid.to_string(), "--session", "dev"]);
    assert!(o.status.success(), "{}", err(&o));
    assert!(
        out(&o).contains(&format!("moved {id} into dev")),
        "{}",
        out(&o)
    );

    // The adopted agent is stopped: watch reports its end.
    eventually("the agent to be forgotten", 30, || {
        seen(&format!("{id} gone"))
    });
    // `watch --json` prints the raw events.
    let mut json = w
        .cmd(&["watch", "--json"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    BufReader::new(json.stdout.take().unwrap())
        .read_line(&mut first)
        .unwrap();
    assert!(first.contains("\"snapshot\""), "{first}");

    // The daemon going away ends `watch`, cleanly, with a failure status.
    let mut daemon = w.daemon.take().unwrap();
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    assert!(!watch.wait().unwrap().success());
    assert!(!json.wait().unwrap().success());
    reader.join().unwrap();
}

#[test]
fn a_daemon_that_cannot_bind_its_socket_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_zj-herdr"))
        .arg("--socket")
        .arg(dir.path().join("missing/dir/zj-herdr.sock"))
        .arg("daemon")
        .env("HOME", dir.path())
        .env("XDG_CONFIG_HOME", dir.path())
        .env("XDG_STATE_HOME", dir.path())
        .env("CLAUDE_CONFIG_DIR", dir.path().join("claude"))
        .env("CODEX_HOME", dir.path().join("codex"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(err(&out).starts_with("zj-herdr: "), "{}", err(&out));
}
