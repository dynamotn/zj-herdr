//! End to end: the `zj-herdr` binary itself, its daemon on a real socket
//! and every client command, in a private HOME, TMPDIR and XDG dirs. The
//! process table is the real one, so nothing here depends on which agents
//! happen to run; only the shape of the answers is checked.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use zj_herdr::sessions::Store;
use zj_herdr_core::{Agent, AgentState, Pane, RestorePreview, StateV1};

struct Env {
    dir: tempfile::TempDir,
    daemon: Option<Child>,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["home", "tmp", "state", "config", "run"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        Env { dir, daemon: None }
    }

    fn path(&self, sub: &str) -> PathBuf {
        self.dir.path().join(sub)
    }

    fn socket(&self) -> PathBuf {
        self.path("run").join("zj-herdr.sock")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_zj-herdr"));
        cmd.arg("--socket")
            .arg(self.socket())
            .args(args)
            .env("HOME", self.path("home"))
            .env("TMPDIR", self.path("tmp"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_RUNTIME_DIR", self.path("run"))
            .env_remove("ZJ_HERDR_SOCKET")
            .env_remove("ZELLIJ")
            .env_remove("ZELLIJ_SESSION_NAME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    /// Start the daemon and wait until it answers; `false` when this
    /// environment forbids binding Unix sockets (a sandbox), so the caller
    /// skips.
    fn start_daemon(&mut self) -> bool {
        let child = self
            .cmd(&["daemon"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let child = self.daemon.as_mut().unwrap();
            if let Some(status) = child.try_wait().unwrap() {
                let mut err = String::new();
                use std::io::Read;
                child
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut err)
                    .unwrap();
                if err.contains("Operation not permitted") {
                    eprintln!("skipped: this environment forbids binding Unix sockets ({err})");
                    return false;
                }
                panic!("the daemon exited ({status}): {err}");
            }
            let out = self.run(&["list", "--json"]);
            if out.status.success() && !stderr(&out).contains("falling back") {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the daemon did not answer on {}", self.socket().display());
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn state_of(out: &Output) -> StateV1 {
    StateV1::from_json(stdout(out).trim()).unwrap_or_else(|e| panic!("{e}: {}", stdout(out)))
}

#[test]
fn scan_prints_this_machine_as_json_and_as_a_table() {
    let env = Env::new();
    let out = env.run(&["scan", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let state = state_of(&out);
    assert_eq!(state.machines.len(), 1);
    assert!(state.machines[0].ssh.is_none());
    let out = env.run(&["scan"]);
    assert!(out.status.success());
    assert!(
        stdout(&out).contains(&state.machines[0].name),
        "{}",
        stdout(&out)
    );
}

#[test]
fn without_a_daemon_list_falls_back_and_the_rest_fail_clearly() {
    let env = Env::new();
    let out = env.run(&["list", "--json"]);
    assert!(out.status.success());
    assert!(stderr(&out).contains("falling back to a local scan"));
    state_of(&out);
    for args in [
        &["wait", "x/1", "idle", "--timeout", "0"][..],
        &["ack", "x/1"],
        &["prompt", "x/1", "hi"],
        &["adopt", "x/1", "--session", "dev"],
        &["spawn", "--session", "dev", "--", "true"],
        &["watch"],
    ] {
        let out = env.run(args);
        assert!(!out.status.success(), "{args:?} succeeded");
        assert!(
            stderr(&out).contains("cannot reach the daemon"),
            "{args:?}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn the_daemon_serves_every_client_command() {
    let mut env = Env::new();
    if !env.start_daemon() {
        return;
    }

    let out = env.run(&["list", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let state = state_of(&out);
    assert!(state.machines.iter().any(|m| m.ssh.is_none()));
    let out = env.run(&["list"]);
    assert!(out.status.success());

    // Requests about agents that do not exist fail with the daemon's
    // reason, and a non-zero status.
    let fails = |args: &[&str], reason: &str| {
        let out = env.run(args);
        assert!(!out.status.success(), "{args:?} succeeded");
        assert!(stderr(&out).contains(reason), "{args:?}: {}", stderr(&out));
    };
    fails(&["ack", "nobody/1"], "no agent matches");
    fails(
        &["adopt", "nobody/1", "--session", "dev"],
        "no running agent",
    );
    fails(&["prompt", "nobody/1", "hi"], "nobody/1");
    fails(
        &["wait", "nobody/1", "idle", "--timeout", "0.2"],
        "nobody/1",
    );
    // No Zellij session runs in this TMPDIR.
    let out = env.run(&["spawn", "--session", "nope", "--cwd", ".", "--", "true"]);
    assert!(!out.status.success());

    // `watch` streams: the snapshot comes first.
    let mut watch = env
        .cmd(&["watch", "--json"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    {
        use std::io::BufRead;
        let stdout = watch.stdout.take().unwrap();
        std::io::BufReader::new(stdout)
            .read_line(&mut first)
            .unwrap();
    }
    let _ = watch.kill();
    let _ = watch.wait();
    assert!(first.contains("\"snapshot\""), "{first}");

    // A text `watch` prints the machines too.
    let mut watch = env.cmd(&["watch"]).stdout(Stdio::piped()).spawn().unwrap();
    let mut first = String::new();
    {
        use std::io::BufRead;
        let stdout = watch.stdout.take().unwrap();
        std::io::BufReader::new(stdout)
            .read_line(&mut first)
            .unwrap();
    }
    let _ = watch.kill();
    let _ = watch.wait();
    assert!(!first.trim().is_empty());
}

fn record(store: &mut Store, session: &str, pane: u32, id: &str) {
    let agent = Agent {
        id: format!("box/{pane}"),
        machine: "box".into(),
        kind: "claude".into(),
        pid: pane,
        cwd: Some("/w".into()),
        state: AgentState::Idle,
        since: 0,
        pane: Some(Pane::new(session, pane)),
        session_id: Some(id.into()),
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
    };
    store.record(&[agent], 1);
}

#[test]
fn restore_previews_and_reports_what_it_cannot_reopen() {
    let env = Env::new();
    let path = env.path("state").join("zj-herdr").join("sessions.json");
    let empty = env.run(&["restore", "--dry-run", "--json"]);
    assert!(empty.status.success());
    let preview = RestorePreview::from_json(stdout(&empty).trim()).unwrap();
    assert_eq!(preview.count, 0);

    let mut store = Store::load(Path::new("/nonexistent")).unwrap();
    record(&mut store, "dev", 3, "abc");
    record(&mut store, "ops", 4, "def");
    store.save(&path).unwrap();

    let out = env.run(&["restore", "--dry-run", "--json", "--session", "dev"]);
    let preview = RestorePreview::from_json(stdout(&out).trim()).unwrap();
    assert_eq!(preview.count, 1);
    let out = env.run(&["restore", "--dry-run"]);
    assert!(out.status.success());
    let text = stdout(&out);
    assert!(
        text.contains("would reopen dev:3 /w claude --resume abc"),
        "{text}"
    );
    assert!(text.contains("would reopen ops:4"), "{text}");
    // For real: no Zellij session `dev` runs here, so both fail.
    let out = env.run(&["restore"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("could not be reopened"),
        "{}",
        stderr(&out)
    );
    // Nothing recorded for this session.
    let out = env.run(&["restore", "--session", "nowhere"]);
    assert!(out.status.success());
    assert!(stderr(&out).contains("nothing to restore"));
}

#[test]
fn a_broken_config_stops_the_daemon_with_its_reason() {
    let env = Env::new();
    let config = env.path("config").join("bad.toml");
    std::fs::write(&config, "remote = 3").unwrap();
    let out = env.run(&["daemon", "--config", config.to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(stderr(&out).starts_with("zj-herdr: "), "{}", stderr(&out));
}
