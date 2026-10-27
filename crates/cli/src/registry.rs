//! The daemon's view of all agents: transition tracking, `done` retention,
//! and wake-ups for waiters and subscribers.

use std::collections::BTreeMap;
use std::sync::mpsc::Sender;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use zj_herdr_core::{Agent, AgentState, Machine, MachineStatus, StateV1};

use crate::api::{self, Event};

/// Long enough for `wait <agent> done` and the plugin to see an exit.
pub const DONE_RETAIN_SECS: u64 = 60;

#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub agent: Agent,
    pub from: Option<AgentState>,
}

/// Turns successive scans of one machine into stable `since` times, `done`
/// entries for exited agents, and a list of changes.
#[derive(Debug, Default)]
pub struct Tracker {
    agents: BTreeMap<String, Agent>,
}

impl Tracker {
    pub fn update(&mut self, fresh: Vec<Agent>, now: u64) -> Vec<Change> {
        let mut changes = Vec::new();
        let mut next = BTreeMap::new();
        for mut agent in fresh {
            match self.agents.remove(&agent.id) {
                Some(prev) if prev.state == agent.state => agent.since = prev.since,
                prev => changes.push(Change {
                    agent: agent.clone(),
                    from: prev.map(|p| p.state),
                }),
            }
            next.insert(agent.id.clone(), agent);
        }
        for (id, mut gone) in std::mem::take(&mut self.agents) {
            if gone.state != AgentState::Done {
                let from = gone.state;
                gone.state = AgentState::Done;
                gone.since = now;
                changes.push(Change {
                    agent: gone.clone(),
                    from: Some(from),
                });
            }
            if now.saturating_sub(gone.since) < DONE_RETAIN_SECS {
                next.insert(id, gone);
            }
        }
        self.agents = next;
        changes
    }

    pub fn agents(&self) -> Vec<Agent> {
        self.agents.values().cloned().collect()
    }
}

#[derive(Default)]
struct Inner {
    trackers: BTreeMap<String, Tracker>,
    machines: Vec<Machine>,
    generated_at: u64,
    /// Bumped when an agent state or a machine status changes.
    changes: u64,
    subscribers: Vec<Sender<String>>,
}

/// Shared between the scan loop, the remote pollers and socket clients.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
    changed: Condvar,
}

impl Registry {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replace one machine's scan. Returns the changes it caused.
    pub fn publish(&self, mut machine: Machine, now: u64) -> Vec<Change> {
        let mut inner = self.lock();
        let tracker = inner.trackers.entry(machine.name.clone()).or_default();
        let changes = tracker.update(std::mem::take(&mut machine.agents), now);
        machine.agents = tracker.agents();
        let mut recovered = false;
        match inner.machines.iter_mut().find(|m| m.name == machine.name) {
            Some(slot) => {
                recovered = slot.status != machine.status;
                *slot = machine;
            }
            None => inner.machines.push(machine),
        }
        inner.generated_at = now;
        if !changes.is_empty() || recovered {
            inner.changes += 1;
        }
        if !changes.is_empty() {
            let lines: Vec<String> = changes
                .iter()
                .map(|c| {
                    api::to_line(&Event::Change {
                        agent: c.agent.clone(),
                        from: c.from,
                    })
                })
                .collect();
            inner
                .subscribers
                .retain(|tx| lines.iter().all(|l| tx.send(l.clone()).is_ok()));
        }
        self.changed.notify_all();
        changes
    }

    /// Mark a machine unreachable, keeping its last known agents.
    pub fn mark_unreachable(&self, template: Machine, error: String) {
        let mut inner = self.lock();
        let slot = match inner.machines.iter().position(|m| m.name == template.name) {
            Some(i) => &mut inner.machines[i],
            None => {
                inner.machines.push(template);
                inner.machines.last_mut().expect("just pushed")
            }
        };
        let fresh = slot.status != MachineStatus::Unreachable;
        slot.status = MachineStatus::Unreachable;
        slot.error = Some(error);
        if fresh {
            inner.changes += 1;
        }
        self.changed.notify_all();
    }

    /// Increases whenever something worth pushing to the plugin changed.
    pub fn change_count(&self) -> u64 {
        self.lock().changes
    }

    pub fn snapshot(&self) -> StateV1 {
        let inner = self.lock();
        StateV1::new(inner.generated_at, inner.machines.clone())
    }

    /// Register a subscriber; it first receives the current snapshot.
    pub fn subscribe(&self, tx: Sender<String>) {
        let mut inner = self.lock();
        let snapshot = StateV1::new(inner.generated_at, inner.machines.clone());
        if tx
            .send(api::to_line(&Event::Snapshot { state: snapshot }))
            .is_ok()
        {
            inner.subscribers.push(tx);
        }
    }

    /// Block until the agent matching `selector` is in `state`. Selectors
    /// like `dev:3` or a pid can match on several machines; agents on
    /// `local` win. The last known state of an agent on an unreachable
    /// machine is stale, so it never satisfies a wait.
    pub fn wait_for(
        &self,
        selector: &str,
        state: AgentState,
        timeout: Option<Duration>,
        local: &str,
    ) -> Result<Agent, String> {
        // A deadline past what `Instant` can hold is no deadline at all.
        let deadline = timeout.and_then(|t| Instant::now().checked_add(t));
        let mut inner = self.lock();
        loop {
            let mut matches = inner
                .machines
                .iter()
                .flat_map(|m| m.agents.iter().map(move |a| (a, m.status)))
                .filter(|(a, _)| a.matches(selector));
            let first = matches.next();
            let found = first
                .filter(|(a, _)| a.machine == local)
                .or_else(|| matches.find(|(a, _)| a.machine == local))
                .or(first);
            if let Some((agent, _)) =
                found.filter(|(a, status)| a.state == state && *status == MachineStatus::Ok)
            {
                return Ok(agent.clone());
            }
            inner = match deadline {
                None => self.changed.wait(inner).unwrap_or_else(|e| e.into_inner()),
                Some(d) => {
                    let left = d.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        let now = match found {
                            None => "not found".to_string(),
                            Some((a, MachineStatus::Unreachable)) => {
                                format!("was {} when {} became unreachable", a.state, a.machine)
                            }
                            Some((a, MachineStatus::Ok)) => format!("is {}", a.state),
                        };
                        return Err(format!(
                            "timed out waiting for {selector} to be {state} (it {now})"
                        ));
                    }
                    self.changed
                        .wait_timeout(inner, left)
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                }
            };
        }
    }

    pub fn find_all(&self, selector: &str) -> Vec<Agent> {
        let inner = self.lock();
        inner
            .machines
            .iter()
            .flat_map(|m| &m.agents)
            .filter(|a| a.matches(selector))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc};

    fn agent(pid: u32, state: AgentState, since: u64) -> Agent {
        Agent {
            id: Agent::make_id("m", pid),
            machine: "m".into(),
            kind: "claude".into(),
            pid,
            cwd: None,
            state,
            since,
            pane: None,
            session_id: None,
        }
    }

    fn machine(agents: Vec<Agent>) -> Machine {
        Machine {
            name: "m".into(),
            ssh: None,
            status: MachineStatus::Ok,
            error: None,
            agents,
        }
    }

    #[test]
    fn tracker_keeps_since_and_reports_changes() {
        let mut t = Tracker::default();
        let changes = t.update(vec![agent(1, AgentState::Working, 100)], 100);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].from, None);

        assert!(t
            .update(vec![agent(1, AgentState::Working, 105)], 105)
            .is_empty());
        assert_eq!(t.agents()[0].since, 100);

        let changes = t.update(vec![agent(1, AgentState::Blocked, 90)], 110);
        assert_eq!(changes[0].from, Some(AgentState::Working));
        assert_eq!(t.agents()[0].since, 90);
    }

    #[test]
    fn tracker_marks_exits_done_then_forgets() {
        let mut t = Tracker::default();
        t.update(vec![agent(1, AgentState::Idle, 0)], 0);
        let changes = t.update(vec![], 10);
        assert_eq!(changes[0].agent.state, AgentState::Done);
        assert_eq!(changes[0].from, Some(AgentState::Idle));
        assert!(t.update(vec![], 20).is_empty());
        assert_eq!(t.agents().len(), 1);
        t.update(vec![], 10 + DONE_RETAIN_SECS);
        assert!(t.agents().is_empty());
    }

    #[test]
    fn subscribers_get_snapshot_then_changes() {
        let reg = Registry::default();
        let (tx, rx) = mpsc::channel();
        reg.subscribe(tx);
        assert!(rx.recv().unwrap().starts_with("{\"event\":\"snapshot\""));
        reg.publish(machine(vec![agent(1, AgentState::Idle, 0)]), 1);
        let line = rx.recv().unwrap();
        assert!(
            line.contains("\"event\":\"change\"") && line.contains("\"from\":null"),
            "{line}"
        );
    }

    #[test]
    fn wait_returns_when_state_arrives() {
        let reg = Arc::new(Registry::default());
        reg.publish(machine(vec![agent(1, AgentState::Working, 0)]), 0);
        let waiter = {
            let reg = Arc::clone(&reg);
            std::thread::spawn(move || {
                reg.wait_for("1", AgentState::Blocked, Some(Duration::from_secs(5)), "m")
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        reg.publish(machine(vec![agent(1, AgentState::Blocked, 1)]), 1);
        assert_eq!(waiter.join().unwrap().unwrap().state, AgentState::Blocked);
    }

    #[test]
    fn wait_times_out() {
        let reg = Registry::default();
        reg.publish(machine(vec![agent(1, AgentState::Working, 0)]), 0);
        let err = reg
            .wait_for(
                "m/1",
                AgentState::Idle,
                Some(Duration::from_millis(30)),
                "m",
            )
            .unwrap_err();
        assert!(err.contains("is working"), "{err}");
    }

    #[test]
    fn wait_prefers_the_local_agent_for_shared_selectors() {
        let reg = Registry::default();
        let mut remote = agent(7, AgentState::Idle, 0);
        remote.machine = "far".into();
        remote.id = Agent::make_id("far", 7);
        reg.publish(
            Machine {
                name: "far".into(),
                ..machine(vec![remote])
            },
            0,
        );
        reg.publish(machine(vec![agent(7, AgentState::Working, 0)]), 0);
        // The remote pid 7 is idle, the local one is not: no early return.
        let err = reg
            .wait_for("7", AgentState::Idle, Some(Duration::from_millis(30)), "m")
            .unwrap_err();
        assert!(err.contains("is working"), "{err}");
    }

    #[test]
    fn wait_ignores_stale_state_on_unreachable_machines() {
        let reg = Arc::new(Registry::default());
        let far = |state| {
            let mut a = agent(7, state, 0);
            a.machine = "far".into();
            a.id = Agent::make_id("far", 7);
            Machine {
                name: "far".into(),
                ..machine(vec![a])
            }
        };
        reg.publish(far(AgentState::Idle), 0);
        reg.mark_unreachable(far(AgentState::Idle), "timeout".into());
        let err = reg
            .wait_for(
                "far/7",
                AgentState::Idle,
                Some(Duration::from_millis(30)),
                "m",
            )
            .unwrap_err();
        assert!(err.contains("unreachable"), "{err}");

        let waiter = {
            let reg = Arc::clone(&reg);
            std::thread::spawn(move || {
                reg.wait_for("far/7", AgentState::Idle, Some(Duration::from_secs(5)), "m")
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        reg.publish(far(AgentState::Idle), 1);
        assert_eq!(waiter.join().unwrap().unwrap().machine, "far");
    }

    #[test]
    fn wait_survives_huge_timeouts() {
        let reg = Registry::default();
        reg.publish(machine(vec![agent(1, AgentState::Idle, 0)]), 0);
        let got = reg.wait_for("m/1", AgentState::Idle, Some(Duration::MAX), "m");
        assert_eq!(got.unwrap().pid, 1);
    }

    #[test]
    fn unreachable_keeps_last_agents() {
        let reg = Registry::default();
        reg.publish(machine(vec![agent(1, AgentState::Idle, 0)]), 0);
        reg.mark_unreachable(machine(vec![]), "timeout".into());
        let snap = reg.snapshot();
        assert_eq!(snap.machines[0].status, MachineStatus::Unreachable);
        assert_eq!(snap.machines[0].agents.len(), 1);
    }
}
