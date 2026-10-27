//! The socket API end to end, against a fake Zellij.
//!
//! Connections use `UnixStream::pair()` so the protocol is tested even where
//! binding a socket file is not allowed; `bind` itself has its own test.

use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use zj_herdr::api::{CommandLine, Event, Request};
use zj_herdr::client;
use zj_herdr::registry::Registry;
use zj_herdr::server::{self, Context};
use zj_herdr::zellij::Zellij;
use zj_herdr_core::{Agent, AgentState, Machine, MachineStatus, Pane, PluginCommand, CMD_SCHEMA};

#[derive(Default)]
struct FakeZellij {
    calls: Mutex<Vec<String>>,
}

impl Zellij for FakeZellij {
    fn sessions(&self) -> Result<Vec<String>, String> {
        Ok(vec!["dev".into()])
    }
    fn pipe(&self, session: &str, name: &str, payload: &str) -> Result<(), String> {
        let call = format!("pipe {session} {name} {payload}");
        self.calls.lock().unwrap().push(call);
        Ok(())
    }
    fn run(&self, session: &str, cwd: Option<&str>, command: &[String]) -> Result<(), String> {
        let call = format!("run {session} {cwd:?} {}", command.join(" "));
        self.calls.lock().unwrap().push(call);
        Ok(())
    }
}

fn agent(machine: &str, pid: u32, state: AgentState, pane: Option<Pane>) -> Agent {
    Agent {
        id: Agent::make_id(machine, pid),
        machine: machine.into(),
        kind: "claude".into(),
        pid,
        cwd: Some("/w".into()),
        state,
        since: 0,
        pane,
        session_id: None,
    }
}

fn machine(name: &str, agents: Vec<Agent>) -> Machine {
    Machine {
        name: name.into(),
        ssh: None,
        status: MachineStatus::Ok,
        error: None,
        agents,
    }
}

fn dev_pane() -> Option<Pane> {
    Some(Pane {
        session: "dev".into(),
        pane_id: 3,
    })
}

struct Daemon {
    registry: Arc<Registry>,
    fake: Arc<FakeZellij>,
    ctx: Arc<Context>,
}

impl Daemon {
    fn new() -> Self {
        let registry = Arc::new(Registry::default());
        let fake = Arc::new(FakeZellij::default());
        let ctx = Arc::new(Context {
            registry: Arc::clone(&registry),
            zellij: fake.clone(),
            machine: "box".into(),
        });
        Daemon { registry, fake, ctx }
    }

    /// A client stream whose other end is served by the daemon.
    fn connect(&self) -> UnixStream {
        let (client, server_end) = UnixStream::pair().unwrap();
        let ctx = Arc::clone(&self.ctx);
        thread::spawn(move || server::handle(server_end, &ctx));
        client
    }

    fn request(&self, req: Request) -> Result<zj_herdr::api::Response, String> {
        client::request_on(self.connect(), &req)
    }

    fn last_call(&self) -> String {
        self.fake.calls.lock().unwrap().last().cloned().unwrap_or_default()
    }
}

fn prompt(agent: &str, text: &str, submit: bool) -> Request {
    Request::Prompt {
        agent: agent.into(),
        text: text.into(),
        submit,
    }
}

#[test]
fn only_the_owning_user_is_served() {
    let (a, _b) = UnixStream::pair().unwrap();
    let me = zj_herdr::paths::uid();
    assert_eq!(server::peer_uid(&a).unwrap(), me);
    server::authorize(&a, me).unwrap();
    let err = server::authorize(&a, me.wrapping_add(1)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);
}

#[test]
fn bind_refuses_a_shared_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let shared = tmp.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
    let err = server::bind(&shared.join("s.sock")).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::PermissionDenied);
    assert!(err.to_string().contains("writable by other users"), "{err}");
}

#[test]
fn bound_socket_is_private_and_not_stolen() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("s.sock");
    let listener = match server::bind(&path) {
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            eprintln!("skipped: this environment forbids binding Unix sockets ({e})");
            return;
        }
        other => other.unwrap(),
    };
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(
        server::bind(&path).is_err(),
        "a second daemon must not take over the socket"
    );
    drop(listener);
    // Nobody listens any more: the stale file is replaced.
    server::bind(&path).unwrap();
}

#[test]
fn list_returns_every_machine() {
    let d = Daemon::new();
    d.registry
        .publish(machine("box", vec![agent("box", 10, AgentState::Idle, dev_pane())]), 1);
    d.registry
        .publish(machine("far", vec![agent("far", 20, AgentState::Idle, dev_pane())]), 1);
    let state = d.request(Request::List).unwrap().state.unwrap();
    assert_eq!(state.agents().count(), 2);
    assert_eq!(state.machines.len(), 2);
}

#[test]
fn prompt_goes_through_the_plugin_pipe() {
    let d = Daemon::new();
    d.registry
        .publish(machine("box", vec![agent("box", 10, AgentState::Idle, dev_pane())]), 1);
    d.registry
        .publish(machine("far", vec![agent("far", 20, AgentState::Idle, dev_pane())]), 1);

    d.request(prompt("box/10", "go on", true)).unwrap();
    let expected = PluginCommand::Prompt {
        pane_id: 3,
        text: "go on\r".into(),
    }
    .to_json();
    assert_eq!(d.last_call(), format!("pipe dev {CMD_SCHEMA} {expected}"));

    // `dev:3` exists on both machines; a pane selector means the local one.
    d.request(prompt("dev:3", "again", false)).unwrap();
    let expected = PluginCommand::Prompt {
        pane_id: 3,
        text: "again".into(),
    }
    .to_json();
    assert_eq!(d.last_call(), format!("pipe dev {CMD_SCHEMA} {expected}"));

    let err = d.request(prompt("far/20", "x", false)).unwrap_err();
    assert!(err.contains("remote"), "{err}");
    let err = d.request(prompt("nobody", "x", false)).unwrap_err();
    assert!(err.contains("no agent"), "{err}");
}

#[test]
fn prompt_needs_a_pane() {
    let d = Daemon::new();
    d.registry
        .publish(machine("box", vec![agent("box", 10, AgentState::Idle, None)]), 1);
    let err = d.request(prompt("10", "x", true)).unwrap_err();
    assert!(err.contains("not running inside Zellij"), "{err}");
}

#[test]
fn spawn_runs_in_the_session() {
    let d = Daemon::new();
    let spawn = Request::Spawn {
        session: "dev".into(),
        cwd: Some("/w".into()),
        command: CommandLine::Argv(vec!["claude".into()]),
    };
    d.request(spawn).unwrap();
    assert_eq!(d.last_call(), "run dev Some(\"/w\") claude");
}

#[test]
fn wait_and_subscribe_see_changes() {
    let d = Daemon::new();
    d.registry.publish(
        machine("box", vec![agent("box", 10, AgentState::Working, dev_pane())]),
        1,
    );

    let wait = Request::Wait {
        agent: "10".into(),
        state: AgentState::Blocked,
        timeout: Some(5.0),
    };
    let wait_conn = d.connect();
    let waiter = thread::spawn(move || client::request_on(wait_conn, &wait));
    let sub_conn = d.connect();
    let events = thread::spawn(move || {
        let mut seen = Vec::new();
        client::subscribe_on(sub_conn, |e| {
            seen.push(e);
            seen.len() < 2
        })
        .unwrap();
        seen
    });
    thread::sleep(Duration::from_millis(100));
    d.registry.publish(
        machine("box", vec![agent("box", 10, AgentState::Blocked, dev_pane())]),
        2,
    );

    let got = waiter.join().unwrap().unwrap().agent.unwrap();
    assert_eq!(got.state, AgentState::Blocked);
    let seen = events.join().unwrap();
    assert!(matches!(&seen[0], Event::Snapshot { .. }));
    assert!(matches!(
        &seen[1],
        Event::Change {
            from: Some(AgentState::Working),
            ..
        }
    ));

    let err = d
        .request(Request::Wait {
            agent: "10".into(),
            state: AgentState::Idle,
            timeout: Some(0.05),
        })
        .unwrap_err();
    assert!(err.contains("timed out"), "{err}");
}

#[test]
fn malformed_lines_get_an_error_and_the_connection_stays_open() {
    let d = Daemon::new();
    let mut stream = d.connect();
    stream.write_all(b"not json\n{\"op\":\"list\"}\n").unwrap();
    let mut lines = BufReader::new(stream).lines();
    let first = lines.next().unwrap().unwrap();
    assert!(
        first.contains("\"ok\":false") && first.contains("bad request"),
        "{first}"
    );
    let second = lines.next().unwrap().unwrap();
    assert!(second.contains("\"ok\":true"), "{second}");
}
