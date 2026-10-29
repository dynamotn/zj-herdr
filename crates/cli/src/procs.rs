//! Process inspection: turn a process table into the list of agent processes,
//! each with its Zellij pane and the CPU used by its whole process tree.

use std::collections::HashMap;
use std::path::PathBuf;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use zj_herdr_core::Pane;

use crate::agents::{self, AgentSpec};

/// Bounds the parent walk; real process trees are far shallower.
const MAX_ANCESTRY: usize = 64;

/// One row of the process table, the only input detection needs from the OS.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawProc {
    pub pid: u32,
    pub ppid: Option<u32>,
    pub name: String,
    pub exe: Option<String>,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// From `ZELLIJ_SESSION_NAME` and `ZELLIJ_PANE_ID` in the environment.
    pub pane: Option<Pane>,
    /// Percent of one core since the previous sample.
    pub cpu: f32,
    /// Unix seconds.
    pub start_time: u64,
}

/// An agent process, ready for a detector.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcInfo {
    pub pid: u32,
    pub spec: &'static AgentSpec,
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub pane: Option<Pane>,
    /// CPU of the agent and all its descendants (tools it runs count too).
    pub cpu: f32,
    pub start_time: u64,
    /// Unix seconds at which this sample was taken.
    pub sampled_at: u64,
    /// Seconds since the tree was last seen busy; `None` when it has not
    /// been seen busy since zj-herdr started watching it.
    pub cpu_quiet_for: Option<u64>,
}

pub fn pane_from_env<'a>(vars: impl IntoIterator<Item = (&'a str, &'a str)>) -> Option<Pane> {
    let mut session = None;
    let mut pane_id = None;
    for (key, value) in vars {
        match key {
            "ZELLIJ_SESSION_NAME" if !value.is_empty() => session = Some(value.to_string()),
            "ZELLIJ_PANE_ID" => pane_id = value.parse().ok(),
            _ => {}
        }
    }
    Some(Pane::new(session?, pane_id?))
}

/// Pick the agent processes out of a process table.
///
/// A process whose direct parent is a script launcher of the same kind is
/// that launcher's native helper (Codex's node launcher starts the native
/// binary, for instance) and is folded into it rather than listed twice.
/// Any other agent below an agent, such as one started by another agent's
/// shell tool, is an agent in its own right and is listed.
pub fn find_agents(procs: &[RawProc], self_pid: u32, sampled_at: u64) -> Vec<ProcInfo> {
    let by_pid: HashMap<u32, &RawProc> = procs.iter().map(|p| (p.pid, p)).collect();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in procs {
        if let Some(ppid) = p.ppid.filter(|&pp| pp != p.pid) {
            children.entry(ppid).or_default().push(p.pid);
        }
    }
    let kinds: HashMap<u32, &'static AgentSpec> = procs
        .iter()
        .filter(|p| p.pid != self_pid)
        .filter_map(|p| agents::identify(&p.name, p.exe.as_deref(), &p.argv).map(|s| (p.pid, s)))
        .filter(|(pid, spec)| {
            let p = by_pid[pid];
            !agents::is_machinery(spec, &p.argv, p.cwd.as_deref())
                && !agents::is_one_shot(spec, &p.argv)
        })
        .collect();

    let ancestors = |pid: u32| {
        std::iter::successors(by_pid.get(&pid).and_then(|p| p.ppid), |&pp| {
            by_pid.get(&pp).and_then(|p| p.ppid)
        })
        .take(MAX_ANCESTRY)
        .take_while(move |&a| a != pid)
    };

    let mut found: Vec<ProcInfo> = kinds
        .iter()
        .filter(|(pid, spec)| {
            let parent = by_pid[pid].ppid.filter(|pp| pp != *pid);
            let helper = parent.is_some_and(|pp| {
                kinds.get(&pp).is_some_and(|k| k.kind == spec.kind)
                    && by_pid
                        .get(&pp)
                        .is_some_and(|p| agents::is_script_launcher(&p.name, &p.argv))
            });
            !helper
        })
        .map(|(&pid, &spec)| {
            let p = by_pid[&pid];
            let pane = if agents::is_detached(spec, &p.argv) {
                None
            } else {
                p.pane
                    .clone()
                    .or_else(|| ancestors(pid).find_map(|a| by_pid.get(&a)?.pane.clone()))
            };
            ProcInfo {
                pid,
                spec,
                argv: p.argv.clone(),
                cwd: p.cwd.clone(),
                pane,
                cpu: subtree_cpu(pid, &by_pid, &children),
                start_time: p.start_time,
                sampled_at,
                cpu_quiet_for: None,
            }
        })
        .collect();
    found.sort_by_key(|p| p.pid);
    found
}

fn subtree_cpu(
    root: u32,
    by_pid: &HashMap<u32, &RawProc>,
    children: &HashMap<u32, Vec<u32>>,
) -> f32 {
    let mut total = 0.0;
    let mut stack = vec![root];
    let mut seen = 0usize;
    while let Some(pid) = stack.pop() {
        seen += 1;
        if seen > by_pid.len() {
            break; // a pid cycle in a torn snapshot
        }
        total += by_pid.get(&pid).map_or(0.0, |p| p.cpu);
        stack.extend(children.get(&pid).into_iter().flatten());
    }
    total
}

/// Live process table backed by `sysinfo`. CPU figures are relative to the
/// previous [`ProcessTable::sample`], so keep one instance around.
pub struct ProcessTable {
    sys: System,
}

impl Default for ProcessTable {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessTable {
    pub fn new() -> Self {
        let mut table = ProcessTable { sys: System::new() };
        table.refresh();
        table
    }

    fn refresh(&mut self) {
        let kind = ProcessRefreshKind::nothing()
            .with_cpu()
            .with_cmd(UpdateKind::OnlyIfNotSet)
            .with_exe(UpdateKind::OnlyIfNotSet)
            .with_environ(UpdateKind::OnlyIfNotSet)
            .with_cwd(UpdateKind::Always);
        self.sys
            .refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    }

    /// Refresh and return the table. Call at least
    /// [`sysinfo::MINIMUM_CPU_UPDATE_INTERVAL`] after the previous call.
    pub fn sample(&mut self) -> Vec<RawProc> {
        self.refresh();
        self.sys
            .processes()
            .values()
            .filter(|p| p.thread_kind().is_none())
            .map(raw_proc)
            .collect()
    }
}

fn raw_proc(p: &sysinfo::Process) -> RawProc {
    RawProc {
        pid: p.pid().as_u32(),
        ppid: p.parent().map(|pp| pp.as_u32()),
        name: p.name().to_string_lossy().into_owned(),
        exe: p.exe().map(|e| e.to_string_lossy().into_owned()),
        argv: p
            .cmd()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
        cwd: p.cwd().map(PathBuf::from),
        pane: pane_from_env(
            p.environ()
                .iter()
                .filter_map(|kv| kv.to_str()?.split_once('=')),
        ),
        cpu: p.cpu_usage(),
        start_time: p.start_time(),
    }
}

/// One process as it is now, without CPU figures; `None` once it is gone.
pub fn snapshot(pid: u32) -> Option<RawProc> {
    let mut sys = System::new();
    let kind = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::Always)
        .with_exe(UpdateKind::Always)
        .with_environ(UpdateKind::Always)
        .with_cwd(UpdateKind::Always);
    let pid = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), true, kind);
    sys.process(pid).map(raw_proc)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, ppid: u32, argv: &[&str]) -> RawProc {
        RawProc {
            pid,
            ppid: Some(ppid),
            name: argv[0].rsplit('/').next().unwrap().to_string(),
            argv: argv.iter().map(|s| s.to_string()).collect(),
            ..RawProc::default()
        }
    }

    fn pane(session: &str, id: u32) -> Option<Pane> {
        Some(Pane::new(session, id))
    }

    #[test]
    fn parses_zellij_env() {
        let vars = [
            ("HOME", "/h"),
            ("ZELLIJ_SESSION_NAME", "work"),
            ("ZELLIJ_PANE_ID", "7"),
        ];
        assert_eq!(pane_from_env(vars), pane("work", 7));
        assert_eq!(pane_from_env([("ZELLIJ_PANE_ID", "7")]), None);
        assert_eq!(
            pane_from_env([("ZELLIJ_SESSION_NAME", "w"), ("ZELLIJ_PANE_ID", "x")]),
            None
        );
    }

    #[test]
    fn finds_agents_and_their_panes() {
        let mut shell = proc(10, 1, &["/bin/zsh"]);
        shell.pane = pane("work", 3);
        let mut claude = proc(11, 10, &["claude"]);
        claude.cpu = 1.0;
        let mut tool = proc(12, 11, &["/bin/bash", "-c", "cargo test"]);
        tool.cpu = 40.0;
        let outside = proc(20, 1, &["aider"]);
        let unrelated = proc(30, 1, &["vim"]);

        let found = find_agents(&[shell, claude, tool, outside, unrelated], 999, 100);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].pid, 11);
        assert_eq!(found[0].spec.kind, "claude");
        assert_eq!(found[0].pane, pane("work", 3));
        assert_eq!(found[0].cpu, 41.0);
        assert_eq!(found[1].spec.kind, "aider");
        assert_eq!(found[1].pane, None);
    }

    #[test]
    fn skips_claude_daemon_machinery_and_detaches_its_sessions() {
        let claude = "/opt/claude/2.1.3/claude";
        let mut shell = proc(10, 1, &["/bin/zsh"]);
        shell.pane = pane("work", 0);
        let mut daemon = proc(11, 10, &[claude, "daemon", "run"]);
        daemon.pane = pane("work", 0);
        let host = proc(
            12,
            11,
            &["claude bg-pty-host", "--bg-pty-host", "/t/spare/a.sock"],
        );
        let mut spare = proc(
            13,
            12,
            &["claude bg-spare", "--bg-spare", "/t/spare/a.claim.sock"],
        );
        spare.cwd = Some(PathBuf::from("/tmp/cc-daemon-501/ee1a/spare"));
        let mut claimed = proc(
            14,
            12,
            &["claude bg-spare", "--bg-spare", "/t/spare/b.claim.sock"],
        );
        claimed.cwd = Some(PathBuf::from("/w/proj"));
        claimed.pane = pane("work", 0);
        let mut interactive = proc(15, 10, &["claude"]);
        interactive.cwd = Some(PathBuf::from("/w/proj"));

        // The kernel still names them `claude`; only argv was retitled.
        let mut procs = [shell, daemon, host, spare, claimed, interactive];
        for p in procs.iter_mut().skip(2).take(3) {
            p.name = "claude".into();
        }
        let found = find_agents(&procs, 999, 100);
        let pids: Vec<(u32, Option<Pane>)> =
            found.iter().map(|p| (p.pid, p.pane.clone())).collect();
        assert_eq!(pids, [(14, None), (15, pane("work", 0))]);
    }

    #[test]
    fn folds_same_kind_helpers_into_their_parent() {
        let launcher = proc(
            10,
            1,
            &["node", "/usr/lib/node_modules/@openai/codex/bin/codex"],
        );
        let native = proc(
            11,
            10,
            &["/usr/lib/node_modules/@openai/codex/vendor/codex", "--yolo"],
        );
        let found = find_agents(&[launcher, native], 999, 0);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pid, 10);
    }

    #[test]
    fn lists_agents_started_by_other_agents() {
        let outer = proc(10, 1, &["claude"]);
        let shell = proc(11, 10, &["/bin/zsh", "-c", "claude hi"]);
        let inner = proc(12, 11, &["claude", "hi"]);
        let direct = proc(13, 10, &["claude", "--input-format", "stream-json"]);
        let one_shot = proc(14, 10, &["claude", "-p", "summarise"]);
        let codex = proc(20, 1, &["codex"]);
        let sub_codex = proc(21, 20, &["codex", "exec", "x"]);
        let found = find_agents(
            &[outer, shell, inner, direct, one_shot, codex, sub_codex],
            999,
            0,
        );
        let pids: Vec<u32> = found.iter().map(|p| p.pid).collect();
        assert_eq!(pids, vec![10, 12, 13, 20, 21]);
    }

    #[test]
    fn skips_itself_and_survives_cycles() {
        let me = proc(5, 5, &["claude"]);
        let a = proc(6, 7, &["codex"]);
        let b = proc(7, 6, &["bash"]);
        let found = find_agents(&[me, a, b], 5, 0);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].pid, 6);
    }
}
