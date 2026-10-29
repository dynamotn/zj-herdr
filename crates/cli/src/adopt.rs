//! `adopt`: move an agent running outside Zellij into a Zellij session, by
//! stopping it and resuming its session in a new pane there.

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use zj_herdr_core::Agent;

use crate::agents;
use crate::procs::{self, RawProc};
use crate::zellij::Zellij;

/// How long the agent gets to exit after SIGTERM.
pub const ADOPT_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a stopping agent is checked for exit.
const POLL: Duration = Duration::from_millis(50);

/// The few things `adopt` does to processes, behind a trait so tests never
/// signal a real one.
pub trait Processes: Send + Sync {
    /// Whether `agent.pid` still runs that agent, not a process that took
    /// over its pid.
    fn is_agent(&self, agent: &Agent) -> bool;
    /// Send SIGTERM.
    fn terminate(&self, pid: u32) -> Result<(), String>;
    fn alive(&self, pid: u32) -> bool;
}

pub struct SystemProcesses;

/// Whether a process is the agent: of the agent's kind, in its cwd (when
/// known), and outside Zellij like it.
pub fn is_same_agent(agent: &Agent, proc: &RawProc) -> bool {
    let kind = agents::identify(&proc.name, proc.exe.as_deref(), &proc.argv).map(|s| s.kind);
    kind == Some(agent.kind.as_str())
        && agent
            .cwd
            .as_deref()
            .is_none_or(|cwd| proc.cwd.as_deref() == Some(Path::new(cwd)))
        && proc.pane.is_none()
}

impl Processes for SystemProcesses {
    fn is_agent(&self, agent: &Agent) -> bool {
        procs::snapshot(agent.pid).is_some_and(|p| is_same_agent(agent, &p))
    }

    fn terminate(&self, pid: u32) -> Result<(), String> {
        let pid = libc::pid_t::try_from(pid).map_err(|_| format!("bad pid {pid}"))?;
        // SAFETY: kill(2) takes plain integers.
        if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
            Ok(())
        } else {
            Err(format!(
                "cannot stop pid {pid}: {}",
                std::io::Error::last_os_error()
            ))
        }
    }

    fn alive(&self, pid: u32) -> bool {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 only checks that the process exists.
        let rc = unsafe { libc::kill(pid, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

/// Stop `agent` (a local agent outside Zellij) and resume its session in a
/// new pane of Zellij session `session`, in its cwd. Nothing is spawned
/// unless the agent has exited within `timeout`. Returns the command run.
pub fn adopt(
    agent: &Agent,
    session: &str,
    zellij: &dyn Zellij,
    procs: &dyn Processes,
    timeout: Duration,
) -> Result<Vec<String>, String> {
    if let Some(pane) = &agent.pane {
        return Err(format!(
            "{} already runs in Zellij session {}",
            agent.id, pane.session
        ));
    }
    let session_id = agent
        .session_id
        .as_deref()
        .ok_or_else(|| format!("{} has no known session to resume", agent.id))?;
    let argv = agents::resume_command(&agent.kind, session_id)
        .ok_or_else(|| format!("{} has no resume command", agent.kind))?;
    if !procs.is_agent(agent) {
        return Err(format!(
            "pid {} is no longer {} ({})",
            agent.pid, agent.id, agent.kind
        ));
    }
    procs.terminate(agent.pid)?;
    let deadline = Instant::now() + timeout;
    while procs.alive(agent.pid) {
        if Instant::now() >= deadline {
            return Err(format!(
                "{} did not exit within {}s of SIGTERM; nothing was opened",
                agent.id,
                timeout.as_secs_f64()
            ));
        }
        thread::sleep(POLL);
    }
    zellij.run(session, agent.cwd.as_deref(), &argv)?;
    Ok(argv)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use zj_herdr_core::{AgentState, Pane, PaneTab};

    #[derive(Default)]
    struct FakeZellij {
        runs: Mutex<Vec<String>>,
    }

    impl Zellij for FakeZellij {
        fn sessions(&self) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
        fn pipe(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn deliver(&self, _: &str, _: &str, _: &str, _: &str) -> Result<String, String> {
            Ok(String::new())
        }
        fn run(&self, session: &str, cwd: Option<&str>, command: &[String]) -> Result<(), String> {
            let call = format!("{session} {cwd:?} {}", command.join(" "));
            self.runs.lock().unwrap().push(call);
            Ok(())
        }
        fn panes(&self, _: &str) -> Result<HashMap<u32, PaneTab>, String> {
            Ok(HashMap::new())
        }
    }

    /// A process that is (or is not) the agent, and exits on SIGTERM after
    /// `exits_after` checks (never, when `None`).
    struct FakeProcs {
        is_agent: bool,
        exits_after: Option<u32>,
        checks: Mutex<u32>,
        signals: Mutex<Vec<u32>>,
    }

    impl FakeProcs {
        fn new(is_agent: bool, exits_after: Option<u32>) -> Self {
            FakeProcs {
                is_agent,
                exits_after,
                checks: Mutex::new(0),
                signals: Mutex::new(vec![]),
            }
        }
    }

    impl Processes for FakeProcs {
        fn is_agent(&self, _: &Agent) -> bool {
            self.is_agent
        }
        fn terminate(&self, pid: u32) -> Result<(), String> {
            self.signals.lock().unwrap().push(pid);
            Ok(())
        }
        fn alive(&self, _: u32) -> bool {
            let mut checks = self.checks.lock().unwrap();
            *checks += 1;
            self.exits_after.is_none_or(|n| *checks <= n)
        }
    }

    fn outside() -> Agent {
        Agent {
            id: "box/42".into(),
            machine: "box".into(),
            kind: "claude".into(),
            pid: 42,
            cwd: Some("/w/proj".into()),
            state: AgentState::Idle,
            since: 0,
            pane: None,
            session_id: Some("abc".into()),
            waiting: None,
        }
    }

    const SHORT: Duration = Duration::from_millis(200);

    #[test]
    fn stops_the_agent_then_resumes_it_in_the_session() {
        let zellij = FakeZellij::default();
        let procs = FakeProcs::new(true, Some(2));
        let argv = adopt(&outside(), "dev", &zellij, &procs, SHORT).unwrap();
        assert_eq!(argv, ["claude", "--resume", "abc"]);
        assert_eq!(*procs.signals.lock().unwrap(), [42]);
        assert_eq!(
            *zellij.runs.lock().unwrap(),
            ["dev Some(\"/w/proj\") claude --resume abc"]
        );
    }

    #[test]
    fn spawns_nothing_when_the_agent_does_not_exit() {
        let zellij = FakeZellij::default();
        let procs = FakeProcs::new(true, None);
        let err = adopt(&outside(), "dev", &zellij, &procs, SHORT).unwrap_err();
        assert!(err.contains("did not exit"), "{err}");
        assert!(zellij.runs.lock().unwrap().is_empty());
    }

    #[test]
    fn refuses_without_signalling_anything() {
        let zellij = FakeZellij::default();
        let cases: Vec<(Agent, bool, &str)> = vec![
            (
                Agent {
                    pane: Some(Pane::new("dev", 3)),
                    ..outside()
                },
                true,
                "already runs",
            ),
            (
                Agent {
                    session_id: None,
                    ..outside()
                },
                true,
                "no known session",
            ),
            (
                Agent {
                    kind: "aider".into(),
                    ..outside()
                },
                true,
                "no resume command",
            ),
            (outside(), false, "no longer"),
        ];
        for (agent, is_agent, want) in cases {
            let procs = FakeProcs::new(is_agent, Some(0));
            let err = adopt(&agent, "dev", &zellij, &procs, SHORT).unwrap_err();
            assert!(err.contains(want), "{err}");
            assert!(procs.signals.lock().unwrap().is_empty());
        }
        assert!(zellij.runs.lock().unwrap().is_empty());
    }

    #[test]
    fn recognises_the_agent_by_kind_cwd_and_no_pane() {
        let agent = outside();
        let proc = RawProc {
            pid: 42,
            name: "claude".into(),
            argv: vec!["claude".into()],
            cwd: Some(PathBuf::from("/w/proj")),
            ..RawProc::default()
        };
        assert!(is_same_agent(&agent, &proc));
        let reused = RawProc {
            name: "vim".into(),
            argv: vec!["vim".into()],
            ..proc.clone()
        };
        assert!(!is_same_agent(&agent, &reused));
        let elsewhere = RawProc {
            cwd: Some(PathBuf::from("/w/other")),
            ..proc.clone()
        };
        assert!(!is_same_agent(&agent, &elsewhere));
        let in_zellij = RawProc {
            pane: Some(Pane::new("dev", 1)),
            ..proc
        };
        assert!(!is_same_agent(&agent, &in_zellij));
    }
}
