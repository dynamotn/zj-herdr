//! One scan of the local machine: processes in, agents out.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use zj_herdr_core::{Agent, AgentState, Machine, MachineStatus};

use crate::detect::{self, Detector, Roots, CPU_ACTIVE_PERCENT};
use crate::procs::{self, ProcInfo, ProcessTable, RawProc};

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Short host name, used as the local machine name.
pub fn local_machine_name() -> String {
    short_host(sysinfo::System::host_name())
}

fn short_host(host: Option<String>) -> String {
    host.and_then(|h| h.split('.').next().map(str::to_owned))
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "local".to_string())
}

/// Build the agent record for a detected process; `None` for a spare
/// Claude Code started ahead of time, which is nobody's session yet.
pub fn to_agent(machine: &str, detectors: &[Box<dyn Detector>], proc: &ProcInfo) -> Option<Agent> {
    let status = detect::detect(detectors, proc);
    if status.spare {
        return None;
    }
    let since = match status.state {
        AgentState::Working => proc.sampled_at,
        _ => status.last_activity.unwrap_or(proc.start_time),
    };
    Some(Agent {
        id: Agent::make_id(machine, proc.pid),
        machine: machine.to_string(),
        kind: proc.spec.kind.to_string(),
        pid: proc.pid,
        cwd: proc.cwd.as_ref().map(|c| c.to_string_lossy().into_owned()),
        state: status.state,
        since,
        pane: proc.pane.clone(),
        session_id: status.session_id,
        waiting: status.waiting,
        started: proc.start_time,
        tokens: status.tokens,
        error: status.error,
        acked: false,
        tasks: status.tasks,
        model: status.model,
        context: status.context,
        background: status.background,
        title: status.title,
    })
}

/// Scans the local machine; keeps CPU history between calls.
pub struct Scanner {
    pub machine: String,
    table: Option<ProcessTable>,
    detectors: Vec<Box<dyn Detector>>,
    /// Last time each pid's process tree was seen busy.
    busy_at: HashMap<u32, u64>,
}

impl Scanner {
    pub fn new(machine: String, roots: &Roots) -> Self {
        Self::with_detectors(machine, roots.detectors())
    }

    pub fn with_detectors(machine: String, detectors: Vec<Box<dyn Detector>>) -> Self {
        Scanner {
            machine,
            table: None,
            detectors,
            busy_at: HashMap::new(),
        }
    }

    /// One-shot scan for `zj-herdr scan`: waits one CPU sampling interval so
    /// the CPU figures mean something.
    pub fn scan_once(machine: String, roots: &Roots) -> Machine {
        let mut scanner = Scanner::new(machine, roots);
        scanner.table = Some(ProcessTable::new());
        std::thread::sleep(
            sysinfo::MINIMUM_CPU_UPDATE_INTERVAL.max(std::time::Duration::from_millis(250)),
        );
        scanner.scan()
    }

    pub fn scan(&mut self) -> Machine {
        let raw = self.table.get_or_insert_with(ProcessTable::new).sample();
        self.scan_from(&raw, now_secs())
    }

    /// Scan a given process table (the testable half of [`Scanner::scan`]).
    pub fn scan_from(&mut self, raw: &[RawProc], now: u64) -> Machine {
        let mut found = procs::find_agents(raw, std::process::id(), now);
        self.busy_at
            .retain(|pid, _| found.iter().any(|p| p.pid == *pid));
        for p in &mut found {
            if p.cpu >= CPU_ACTIVE_PERCENT {
                self.busy_at.insert(p.pid, now);
            }
            p.cpu_quiet_for = self.busy_at.get(&p.pid).map(|t| now.saturating_sub(*t));
        }
        detect::prepare(&self.detectors, &found);
        Machine {
            name: self.machine.clone(),
            ssh: None,
            status: MachineStatus::Ok,
            error: None,
            agents: found
                .iter()
                .filter_map(|p| to_agent(&self.machine, &self.detectors, p))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_machine_name_is_the_short_host_name() {
        assert_eq!(short_host(Some("box.example.org".into())), "box");
        assert_eq!(short_host(Some(String::new())), "local");
        assert_eq!(short_host(Some(".hidden".into())), "local");
        assert_eq!(short_host(None), "local");
    }
}
