//! Socket server paths `socket.rs` does not reach: a peer that cannot be
//! identified, blank lines, the accept loop, and the error replies.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use zj_herdr::adopt::Processes;
use zj_herdr::api::{CommandLine, Request};
use zj_herdr::client;
use zj_herdr::registry::Registry;
use zj_herdr::server::{self, Context};
use zj_herdr::zellij::Zellij;
use zj_herdr_core::{Agent, AgentState, Machine, MachineStatus, Pane, PaneTab};

/// Nothing is reachable: `run` fails, the rest is never asked.
struct DeadZellij;

impl Zellij for DeadZellij {
    fn sessions(&self) -> Result<Vec<String>, String> {
        Ok(vec![])
    }
    fn pipe(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn deliver(&self, _: &str, _: &str, _: &str, _: &str) -> Result<String, String> {
        Ok(String::new())
    }
    fn run(&self, session: &str, _: Option<&str>, _: &[String]) -> Result<(), String> {
        Err(format!("no session {session}"))
    }
    fn panes(&self, _: &str) -> Result<HashMap<u32, PaneTab>, String> {
        Ok(HashMap::new())
    }
}

struct NoProcs;

impl Processes for NoProcs {
    fn is_agent(&self, _: &Agent) -> bool {
        false
    }
    fn helpers(&self, _: &Agent) -> Vec<u32> {
        vec![]
    }
    fn terminate(&self, _: u32) -> Result<(), String> {
        Ok(())
    }
    fn alive(&self, _: u32) -> bool {
        false
    }
}

fn context() -> Arc<Context> {
    Arc::new(Context {
        registry: Arc::new(Registry::default()),
        zellij: Arc::new(DeadZellij),
        machine: "box".into(),
        plugin: "zj_herdr".into(),
        procs: Arc::new(NoProcs),
        adopt_timeout: Duration::from_millis(10),
        adopting: Default::default(),
    })
}

fn agent(pid: u32) -> Agent {
    Agent {
        id: Agent::make_id("box", pid),
        machine: "box".into(),
        kind: "claude".into(),
        pid,
        cwd: None,
        state: AgentState::Idle,
        since: 0,
        pane: Some(Pane::new("dev", 3)),
        session_id: None,
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
fn a_descriptor_that_is_not_a_socket_has_no_peer() {
    let devnull = OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
    let err = server::peer_uid(&UnixStream::from(devnull)).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::ENOTSOCK));
}

#[test]
fn blank_lines_are_skipped_and_the_rest_answered() {
    let ctx = context();
    let (client, server_end) = UnixStream::pair().unwrap();
    let handler = thread::spawn(move || server::handle(server_end, &ctx));
    let mut writer = client.try_clone().unwrap();
    writer.write_all(b"\n  \n{\"op\":\"list\"}\n").unwrap();
    let mut first = String::new();
    BufReader::new(client).read_line(&mut first).unwrap();
    assert!(first.contains("\"ok\":true"), "{first}");
    drop(writer);
    handler.join().unwrap().unwrap();
}

#[test]
fn accept_serves_a_connection_and_reports_a_failed_accept() {
    let ctx = context();
    assert!(server::accept(Err(std::io::Error::other("no fds")), &ctx).is_none());
    // A line that is not UTF-8 ends the connection with a reported error.
    let (mut client, server_end) = UnixStream::pair().unwrap();
    let worker = server::accept(Ok(server_end), &ctx).unwrap();
    client.write_all(b"\xff\n").unwrap();
    worker.join().unwrap();
    // A client that leaves is not an error worth printing.
    let (client, server_end) = UnixStream::pair().unwrap();
    drop(client);
    server::accept(Ok(server_end), &ctx)
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn serve_answers_each_connection_on_a_bound_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("s.sock");
    let listener = server::bind(&path).unwrap();
    let ctx = context();
    thread::spawn(move || server::serve(listener, ctx));
    for _ in 0..2 {
        let resp = client::request(&path, &Request::List).unwrap();
        assert!(resp.state.is_some());
    }
}

#[test]
fn respond_reports_what_cannot_be_done() {
    let ctx = context();
    let wait = |timeout| {
        let (agent, state) = ("x".into(), AgentState::Idle);
        server::respond(
            &ctx,
            Request::Wait {
                agent,
                state,
                timeout,
            },
        )
    };
    let err = wait(Some(-1.0)).into_result().unwrap_err();
    assert!(err.contains("non-negative"), "{err}");

    let spawn = Request::Spawn {
        session: "dev".into(),
        cwd: None,
        command: CommandLine::Argv(vec!["claude".into()]),
    };
    let err = server::respond(&ctx, spawn).into_result().unwrap_err();
    assert_eq!(err, "no session dev");
    let err = server::respond(&ctx, Request::Subscribe)
        .into_result()
        .unwrap_err();
    assert!(err.contains("own connection"), "{err}");

    let ack = |agent: &str| {
        server::respond(
            &ctx,
            Request::Ack {
                agent: agent.into(),
            },
        )
    };
    assert!(ack("nobody").into_result().is_err());
    ctx.registry.publish(
        Machine {
            name: "box".into(),
            ssh: None,
            status: MachineStatus::Ok,
            error: None,
            agents: vec![agent(10)],
        },
        1,
    );
    let got = ack("box/10").into_result().unwrap().agent.unwrap();
    assert_eq!(got.id, "box/10");
}
