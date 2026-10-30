//! The daemon's view of all agents: transition tracking, `done` retention,
//! and wake-ups for waiters and subscribers.

use std::collections::BTreeMap;
use std::sync::mpsc::Sender;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use zj_herdr_core::{Agent, AgentState, Machine, MachineStatus, StateV1};

use crate::api::{self, Event};

/// Long enough for `wait <agent> done` and the plugin to see an exit.
pub const DONE_RETAIN_SECS: u64 = 10;
/// An agent that exits having run less than this was a helper some hook or
/// plugin started; it is dropped at once instead of shown as `done`.
pub const HELPER_SECS: u64 = 30;

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
    /// Ids of `done` agents dropped by the last update.
    expired: Vec<String>,
}

impl Tracker {
    pub fn update(&mut self, fresh: Vec<Agent>, now: u64) -> Vec<Change> {
        let mut changes = Vec::new();
        let mut next = BTreeMap::new();
        for mut agent in fresh {
            match self.agents.remove(&agent.id) {
                Some(prev) if prev.state == agent.state => agent.since = prev.since,
                prev => {
                    // The scanner's `since` is the last transcript write; a
                    // transition we saw happened now.
                    if prev.is_some() {
                        agent.since = now;
                    }
                    changes.push(Change {
                        agent: agent.clone(),
                        from: prev.map(|p| p.state),
                    });
                }
            }
            next.insert(agent.id.clone(), agent);
        }
        for (id, mut gone) in std::mem::take(&mut self.agents) {
            let helper = gone.started != 0 && now.saturating_sub(gone.started) < HELPER_SECS;
            if gone.state != AgentState::Done && helper {
                self.expired.push(id);
                continue;
            }
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
            } else {
                self.expired.push(id);
            }
        }
        self.agents = next;
        changes
    }

    /// Ids dropped since the last call, for subscribers to forget them.
    pub fn take_expired(&mut self) -> Vec<String> {
        std::mem::take(&mut self.expired)
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
    /// Agents marked seen, with the `since` of the idle spell they saw.
    acks: BTreeMap<String, u64>,
    subscribers: Vec<Sender<String>>,
}

/// Mark `machine`'s agents seen per `acks`, forgetting the marks of its
/// agents that left the idle spell they were made in.
fn apply_acks(acks: &mut BTreeMap<String, u64>, machine: &mut Machine) {
    let prefix = format!("{}/", machine.name);
    acks.retain(|id, since| {
        !id.starts_with(&prefix)
            || machine
                .agents
                .iter()
                .any(|a| &a.id == id && a.since == *since && a.state == AgentState::Idle)
    });
    for a in &mut machine.agents {
        a.acked = acks.get(&a.id) == Some(&a.since);
    }
}

fn machine_line(m: &Machine) -> String {
    api::to_line(&Event::Machine {
        name: m.name.clone(),
        status: m.status,
        error: m.error.clone(),
    })
}

/// Send lines to every subscriber, dropping those that hung up.
fn broadcast(inner: &mut Inner, lines: &[String]) {
    if !lines.is_empty() {
        inner
            .subscribers
            .retain(|tx| lines.iter().all(|l| tx.send(l.clone()).is_ok()));
    }
}

/// Shared between the scan loop, the remote pollers and socket clients.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
    changed: Condvar,
}

impl Registry {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        crate::sync::lock(&self.inner)
    }

    /// Replace one machine's scan. Returns the changes it caused.
    pub fn publish(&self, mut machine: Machine, now: u64) -> Vec<Change> {
        let mut inner = self.lock();
        let tracker = inner.trackers.entry(machine.name.clone()).or_default();
        let changes = tracker.update(std::mem::take(&mut machine.agents), now);
        let expired = tracker.take_expired();
        machine.agents = tracker.agents();
        apply_acks(&mut inner.acks, &mut machine);
        let mut recovered = false;
        let mut lines = Vec::new();
        match inner.machines.iter_mut().find(|m| m.name == machine.name) {
            Some(slot) => {
                recovered = slot.status != machine.status;
                if recovered {
                    lines.push(machine_line(&machine));
                }
                *slot = machine;
            }
            None => inner.machines.push(machine),
        }
        inner.generated_at = now;
        if !changes.is_empty() || recovered || !expired.is_empty() {
            inner.changes += 1;
        }
        lines.extend(changes.iter().map(|c| {
            api::to_line(&Event::Change {
                agent: Box::new(c.agent.clone()),
                from: c.from,
            })
        }));
        lines.extend(
            expired
                .into_iter()
                .map(|id| api::to_line(&Event::Gone { id })),
        );
        broadcast(&mut inner, &lines);
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
            let line = machine_line(slot);
            inner.changes += 1;
            broadcast(&mut inner, &[line]);
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
        // A helper that exits is dropped without a `done` entry; one seen
        // while waiting and then gone has exited all the same.
        let mut seen: Option<Agent> = None;
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
            match (found, seen.take()) {
                (Some((agent, MachineStatus::Ok)), _) => seen = Some(agent.clone()),
                (None, Some(mut gone)) if state == AgentState::Done => {
                    gone.state = AgentState::Done;
                    return Ok(gone);
                }
                (_, before) => seen = before,
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

    /// Mark the idle agent `selector` names as seen, until it next changes
    /// state. Pane selectors mean this machine's agent.
    pub fn ack(&self, selector: &str, local: &str) -> Result<Agent, String> {
        let mut inner = self.lock();
        let matches: Vec<(usize, usize)> = inner
            .machines
            .iter()
            .enumerate()
            .flat_map(|(m, machine)| {
                machine
                    .agents
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| a.matches(selector))
                    .map(move |(i, a)| (m, i, a.machine == local))
            })
            .map(|(m, i, _)| (m, i))
            .collect();
        let local_first = matches
            .iter()
            .find(|(m, _)| inner.machines[*m].name == local)
            .or(matches.first())
            .copied();
        let Some((m, i)) = local_first else {
            return Err(format!("no agent matches {selector:?}"));
        };
        let agent = &mut inner.machines[m].agents[i];
        if agent.state != AgentState::Idle {
            return Err(format!("{} is {}, not idle", agent.id, agent.state));
        }
        agent.acked = true;
        let agent = agent.clone();
        inner.acks.insert(agent.id.clone(), agent.since);
        inner.changes += 1;
        self.changed.notify_all();
        Ok(agent)
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
    fn ack_marks_an_idle_agent_seen_until_it_changes_state() {
        let reg = Registry::default();
        reg.publish(
            machine(vec![
                agent(1, AgentState::Idle, 10),
                agent(2, AgentState::Working, 10),
            ]),
            100,
        );
        let before = reg.change_count();
        assert!(reg.ack("m/2", "m").unwrap_err().contains("not idle"));
        assert!(reg.ack("m/9", "m").is_err());
        assert!(reg.ack("m/1", "m").unwrap().acked);
        assert!(reg.change_count() > before, "the plugin is pushed the mark");
        let acked = |reg: &Registry| {
            reg.snapshot()
                .agents()
                .find(|a| a.id == "m/1")
                .unwrap()
                .acked
        };
        // Still idle in the next scan: still seen.
        reg.publish(machine(vec![agent(1, AgentState::Idle, 10)]), 101);
        assert!(acked(&reg));
        // Working, then idle again: a new idle spell needs you again.
        reg.publish(machine(vec![agent(1, AgentState::Working, 10)]), 102);
        reg.publish(machine(vec![agent(1, AgentState::Idle, 10)]), 103);
        assert!(!acked(&reg));
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

        // A transition dates from when it was seen, not the scanner's time.
        let changes = t.update(vec![agent(1, AgentState::Blocked, 90)], 110);
        assert_eq!(changes[0].from, Some(AgentState::Working));
        assert_eq!(changes[0].agent.since, 110);
        assert_eq!(t.agents()[0].since, 110);
    }

    #[test]
    fn subscribers_hear_when_a_done_agent_is_dropped() {
        let reg = Registry::default();
        reg.publish(machine(vec![agent(1, AgentState::Idle, 0)]), 0);
        reg.publish(machine(vec![]), 1);
        let (tx, rx) = mpsc::channel();
        reg.subscribe(tx);
        rx.recv().unwrap();
        reg.publish(machine(vec![]), 1 + DONE_RETAIN_SECS);
        let line = rx.recv().unwrap();
        assert!(
            line.contains("\"event\":\"gone\"") && line.contains("m/1"),
            "{line}"
        );
    }

    #[test]
    fn subscribers_hear_machines_go_and_come_back() {
        let reg = Registry::default();
        reg.publish(machine(vec![]), 0);
        let (tx, rx) = mpsc::channel();
        reg.subscribe(tx);
        rx.recv().unwrap();
        reg.mark_unreachable(machine(vec![]), "timeout".into());
        let line = rx.recv().unwrap();
        assert!(
            line.contains("\"event\":\"machine\"") && line.contains("\"unreachable\""),
            "{line}"
        );
        // Still unreachable: nothing new to say.
        reg.mark_unreachable(machine(vec![]), "timeout".into());
        assert!(rx.try_recv().is_err());
        reg.publish(machine(vec![]), 1);
        let line = rx.recv().unwrap();
        assert!(line.contains("\"status\":\"ok\""), "{line}");
    }

    #[test]
    fn tracker_marks_exits_done_then_forgets() {
        let mut t = Tracker::default();
        t.update(vec![agent(1, AgentState::Idle, 0)], 0);
        let changes = t.update(vec![], 10);
        assert_eq!(changes[0].agent.state, AgentState::Done);
        assert_eq!(changes[0].from, Some(AgentState::Idle));
        assert!(t.update(vec![], 10 + DONE_RETAIN_SECS - 1).is_empty());
        assert_eq!(t.agents().len(), 1);
        assert!(t.take_expired().is_empty());
        t.update(vec![], 10 + DONE_RETAIN_SECS);
        assert!(t.agents().is_empty());
        assert_eq!(t.take_expired(), vec![Agent::make_id("m", 1)]);
    }

    #[test]
    fn tracker_drops_short_lived_helpers_at_once() {
        let mut t = Tracker::default();
        let mut helper = agent(1, AgentState::Working, 100);
        helper.started = 95;
        let mut long = agent(2, AgentState::Idle, 100);
        long.started = 10;
        t.update(vec![helper, long], 100);
        let changes = t.update(vec![], 100 + HELPER_SECS - 6);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].agent.pid, 2);
        assert_eq!(changes[0].agent.state, AgentState::Done);
        assert_eq!(t.take_expired(), vec![Agent::make_id("m", 1)]);
        assert_eq!(t.agents().len(), 1);
    }

    #[test]
    fn wait_for_done_sees_a_helper_vanish() {
        let reg = Arc::new(Registry::default());
        let mut helper = agent(1, AgentState::Working, 0);
        helper.started = 100;
        reg.publish(machine(vec![helper]), 100);
        let waiter = {
            let reg = Arc::clone(&reg);
            std::thread::spawn(move || {
                reg.wait_for("m/1", AgentState::Done, Some(Duration::from_secs(5)), "m")
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        reg.publish(machine(vec![]), 105);
        let gone = waiter.join().unwrap().unwrap();
        assert_eq!((gone.pid, gone.state), (1, AgentState::Done));
        assert!(reg.find_all("m/1").is_empty());
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
    fn unreachable_unknown_machine_is_added() {
        let reg = Registry::default();
        reg.mark_unreachable(machine(vec![]), "no route".into());
        let snap = reg.snapshot();
        assert_eq!(snap.machines.len(), 1);
        assert_eq!(snap.machines[0].status, MachineStatus::Unreachable);
        assert_eq!(snap.machines[0].error.as_deref(), Some("no route"));
    }

    /// A registry whose mutex a panicking thread poisoned.
    fn poisoned() -> std::sync::Arc<Registry> {
        let reg = std::sync::Arc::new(Registry::default());
        let r = reg.clone();
        let _ = std::thread::spawn(move || {
            let _held = r.lock();
            panic!("poison the registry");
        })
        .join();
        assert!(reg.inner.is_poisoned());
        reg
    }

    #[test]
    fn waiting_survives_a_poisoned_registry() {
        let reg = poisoned();
        let waiter = {
            let reg = reg.clone();
            std::thread::spawn(move || reg.wait_for("m/1", AgentState::Idle, None, "m"))
        };
        std::thread::sleep(Duration::from_millis(50));
        reg.publish(machine(vec![agent(1, AgentState::Idle, 0)]), 0);
        assert_eq!(waiter.join().unwrap().unwrap().pid, 1);
        let err = reg
            .wait_for(
                "m/9",
                AgentState::Idle,
                Some(Duration::from_millis(30)),
                "m",
            )
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
    }

    #[test]
    fn wait_without_timeout_wakes_on_publish() {
        let reg = std::sync::Arc::new(Registry::default());
        let waiter = {
            let reg = reg.clone();
            std::thread::spawn(move || reg.wait_for("m/1", AgentState::Idle, None, "m"))
        };
        std::thread::sleep(Duration::from_millis(50));
        reg.publish(machine(vec![agent(1, AgentState::Idle, 0)]), 0);
        assert_eq!(waiter.join().unwrap().unwrap().pid, 1);
    }

    #[test]
    fn wait_timeout_for_a_missing_agent_says_so() {
        let reg = Registry::default();
        let err = reg
            .wait_for("m/9", AgentState::Idle, Some(Duration::ZERO), "m")
            .unwrap_err();
        assert!(err.contains("(it not found)"), "{err}");
    }

    #[test]
    fn wait_timeout_says_what_the_agent_was_doing() {
        let reg = Registry::default();
        reg.publish(machine(vec![agent(1, AgentState::Working, 0)]), 0);
        let none = Some(Duration::ZERO);
        let err = reg
            .wait_for("m/1", AgentState::Idle, none, "m")
            .unwrap_err();
        assert!(err.contains("(it is working)"), "{err}");
        reg.mark_unreachable(machine(vec![]), "timeout".into());
        let err = reg
            .wait_for("m/1", AgentState::Idle, none, "m")
            .unwrap_err();
        assert!(
            err.contains("was working when m became unreachable"),
            "{err}"
        );
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
