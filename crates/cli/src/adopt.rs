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
    /// Child processes of the agent's own kind: the native helper a script
    /// launcher (`node .../codex`) runs, which must exit too.
    fn helpers(&self, agent: &Agent) -> Vec<u32>;
    /// Send SIGTERM.
    fn terminate(&self, pid: u32) -> Result<(), String>;
    fn alive(&self, pid: u32) -> bool;
}

pub struct SystemProcesses;

fn kind_of(proc: &RawProc) -> Option<&'static str> {
    agents::identify(&proc.name, proc.exe.as_deref(), &proc.argv).map(|s| s.kind)
}

/// Whether a process is the agent: started when it did (not a later
/// process that reuses its pid), of its kind, in its cwd, and outside
/// Zellij like it. An agent with no known cwd or start time never matches.
pub fn is_same_agent(agent: &Agent, proc: &RawProc) -> bool {
    agent.started != 0
        && proc.start_time == agent.started
        && kind_of(proc) == Some(agent.kind.as_str())
        && agent
            .cwd
            .as_deref()
            .is_some_and(|cwd| proc.cwd.as_deref() == Some(Path::new(cwd)))
        && proc.pane.is_none()
}

/// Never init, this process, or a pid that `kill(2)` would read as a
/// process group (0, or negative once cast).
pub fn may_signal(pid: u32) -> bool {
    pid > 1 && pid != std::process::id() && libc::pid_t::try_from(pid).is_ok()
}

/// Children of `agent.pid` of the agent's kind.
pub fn same_kind_children(agent: &Agent, table: &[RawProc]) -> Vec<u32> {
    table
        .iter()
        .filter(|p| p.ppid == Some(agent.pid) && p.pid != agent.pid)
        .filter(|p| kind_of(p) == Some(agent.kind.as_str()))
        .map(|p| p.pid)
        .collect()
}

impl Processes for SystemProcesses {
    fn is_agent(&self, agent: &Agent) -> bool {
        procs::snapshot(agent.pid).is_some_and(|p| is_same_agent(agent, &p))
    }

    fn helpers(&self, agent: &Agent) -> Vec<u32> {
        same_kind_children(agent, &procs::ProcessTable::new().sample())
    }

    fn terminate(&self, pid: u32) -> Result<(), String> {
        if !may_signal(pid) {
            return Err(format!("refusing to stop pid {pid}"));
        }
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
/// new pane of Zellij session `session`, in its cwd. Nothing is stopped
/// unless the session is running, and nothing is spawned unless the agent
/// (and its native helper, if any) exited within `timeout`. Returns the
/// command run.
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
    let cwd = agent
        .cwd
        .as_deref()
        .ok_or_else(|| format!("{} has no known cwd to resume in", agent.id))?;
    let argv = agents::resume_command(&agent.kind, session_id)
        .ok_or_else(|| format!("{} has no resume command", agent.kind))?;
    if !zellij.sessions()?.iter().any(|s| s == session) {
        return Err(format!("no running Zellij session {session:?}"));
    }
    if !procs.is_agent(agent) {
        return Err(format!(
            "pid {} is no longer {} ({})",
            agent.pid, agent.id, agent.kind
        ));
    }
    let mut pids = vec![agent.pid];
    pids.extend(procs.helpers(agent));
    procs.terminate(agent.pid)?;
    let deadline = Instant::now() + timeout;
    while pids.iter().any(|&pid| procs.alive(pid)) {
        if Instant::now() >= deadline {
            return Err(format!(
                "{} did not exit within {}s of SIGTERM; nothing was opened",
                agent.id,
                timeout.as_secs_f64()
            ));
        }
        thread::sleep(POLL);
    }
    zellij.run(session, Some(cwd), &argv).map_err(|e| {
        format!(
            "{} stopped, but reopening it failed ({e}); resume it with `{}` in {cwd}",
            agent.id,
            argv.join(" ")
        )
    })?;
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
            Ok(vec!["dev".into()])
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
    /// `exits_after` checks (never, when `None`); its helper, if any, after
    /// its own count.
    struct FakeProcs {
        is_agent: bool,
        exits_after: Option<u32>,
        helper: Option<(u32, Option<u32>)>,
        checks: Mutex<HashMap<u32, u32>>,
        signals: Mutex<Vec<u32>>,
    }

    impl FakeProcs {
        fn new(is_agent: bool, exits_after: Option<u32>) -> Self {
            FakeProcs {
                is_agent,
                exits_after,
                helper: None,
                checks: Mutex::new(HashMap::new()),
                signals: Mutex::new(vec![]),
            }
        }
    }

    impl Processes for FakeProcs {
        fn is_agent(&self, _: &Agent) -> bool {
            self.is_agent
        }
        fn helpers(&self, _: &Agent) -> Vec<u32> {
            self.helper.iter().map(|(pid, _)| *pid).collect()
        }
        fn terminate(&self, pid: u32) -> Result<(), String> {
            self.signals.lock().unwrap().push(pid);
            Ok(())
        }
        fn alive(&self, pid: u32) -> bool {
            let mut checks = self.checks.lock().unwrap();
            let n = checks.entry(pid).or_default();
            *n += 1;
            let limit = match self.helper {
                Some((helper, limit)) if helper == pid => limit,
                _ => self.exits_after,
            };
            limit.is_none_or(|limit| *n <= limit)
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
            started: 1000,
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
            (
                Agent {
                    cwd: None,
                    ..outside()
                },
                true,
                "no known cwd",
            ),
            (outside(), false, "no longer"),
        ];
        for (agent, is_agent, want) in cases {
            let procs = FakeProcs::new(is_agent, Some(0));
            let err = adopt(&agent, "dev", &zellij, &procs, SHORT).unwrap_err();
            assert!(err.contains(want), "{err}");
            assert!(procs.signals.lock().unwrap().is_empty());
        }
        // A mistyped session: the agent is left running.
        let procs = FakeProcs::new(true, Some(0));
        let err = adopt(&outside(), "deev", &zellij, &procs, SHORT).unwrap_err();
        assert!(err.contains("no running Zellij session"), "{err}");
        assert!(procs.signals.lock().unwrap().is_empty());
        assert!(zellij.runs.lock().unwrap().is_empty());
    }

    #[test]
    fn waits_for_the_native_helper_too() {
        let zellij = FakeZellij::default();
        let mut procs = FakeProcs::new(true, Some(0));
        procs.helper = Some((43, None));
        let err = adopt(&outside(), "dev", &zellij, &procs, SHORT).unwrap_err();
        assert!(err.contains("did not exit"), "{err}");
        assert!(zellij.runs.lock().unwrap().is_empty());
        assert_eq!(*procs.signals.lock().unwrap(), [42], "only the agent");

        let mut procs = FakeProcs::new(true, Some(0));
        procs.helper = Some((43, Some(3)));
        adopt(&outside(), "dev", &zellij, &procs, SHORT).unwrap();
        assert_eq!(zellij.runs.lock().unwrap().len(), 1);
    }

    #[test]
    fn finds_native_helpers_of_the_same_kind() {
        let raw = |pid, ppid, name: &str| RawProc {
            pid,
            ppid: Some(ppid),
            name: name.into(),
            argv: vec![name.into()],
            ..RawProc::default()
        };
        let table = [
            raw(42, 1, "claude"),
            raw(43, 42, "claude"),
            raw(44, 42, "cargo"),
            raw(45, 43, "claude"),
            raw(46, 7, "claude"),
        ];
        assert_eq!(same_kind_children(&outside(), &table), [43]);
    }

    #[test]
    fn recognises_the_agent_by_start_kind_cwd_and_no_pane() {
        let agent = outside();
        let proc = RawProc {
            pid: 42,
            name: "claude".into(),
            argv: vec!["claude".into()],
            cwd: Some(PathBuf::from("/w/proj")),
            start_time: 1000,
            ..RawProc::default()
        };
        assert!(is_same_agent(&agent, &proc));
        // Another claude in the same cwd that took over the pid.
        let pid_reused = RawProc {
            start_time: 1001,
            ..proc.clone()
        };
        assert!(!is_same_agent(&agent, &pid_reused));
        let unknown_start = Agent {
            started: 0,
            ..outside()
        };
        assert!(!is_same_agent(
            &unknown_start,
            &RawProc {
                start_time: 0,
                ..proc.clone()
            }
        ));
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
            ..proc.clone()
        };
        assert!(!is_same_agent(&agent, &in_zellij));
        let unknown_cwd = Agent {
            cwd: None,
            ..outside()
        };
        assert!(!is_same_agent(&unknown_cwd, &proc));
    }

    #[test]
    fn never_signals_init_itself_or_a_process_group() {
        assert!(!may_signal(0));
        assert!(!may_signal(1));
        assert!(!may_signal(std::process::id()));
        assert!(!may_signal(u32::MAX), "would be -1: every process");
        assert!(may_signal(4242));
    }

    #[test]
    fn system_processes_see_and_stop_a_real_process() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let sys = SystemProcesses;
        let agent = Agent {
            pid,
            started: 0,
            ..outside()
        };
        assert!(!sys.is_agent(&agent), "`sleep` is no claude");
        assert!(sys.helpers(&agent).is_empty());
        assert!(sys.alive(pid));
        sys.terminate(pid).unwrap();
        child.wait().unwrap();
        assert!(!sys.alive(pid));
        assert!(sys.terminate(1).unwrap_err().contains("refusing"));
        assert!(!sys.alive(u32::MAX));
    }
}
