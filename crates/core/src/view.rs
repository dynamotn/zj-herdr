//! What the plugin shows, kept free of Zellij so it is tested natively:
//! the grouped agent list and the attention cycle.

use crate::{Agent, AgentState, Machine, MachineStatus, StateV1};

/// Label for agents running outside Zellij.
pub const NO_SESSION: &str = "(outside zellij)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row<'a> {
    Machine(&'a Machine),
    Session {
        name: &'a str,
        local: bool,
    },
    /// A tab of a local session; `state` is its most urgent agent's.
    Tab {
        session: &'a str,
        tab: &'a TabLayout,
        state: Option<AgentState>,
    },
    Agent {
        agent: &'a Agent,
        local: bool,
    },
}

/// A tab of a local Zellij session, as the plugin sees it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TabLayout {
    pub position: usize,
    pub name: String,
    pub active: bool,
    /// Terminal pane ids in this tab.
    pub panes: Vec<u32>,
}

/// The tabs of one local Zellij session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionLayout {
    pub name: String,
    pub tabs: Vec<TabLayout>,
}

fn session_of(agent: &Agent) -> &str {
    agent
        .pane
        .as_ref()
        .map_or(NO_SESSION, |p| p.session.as_str())
}

/// Blocked first, then idle, then the rest; longest-waiting first.
fn agent_key(a: &Agent) -> (u8, u64, u32) {
    (a.state.attention_rank(), a.since, a.pid)
}

fn is_local(m: &Machine) -> bool {
    m.ssh.is_none()
}

/// Machines (local first), then sessions, then agents, each group ordered
/// so whatever needs attention comes first.
pub fn rows(state: &StateV1) -> Vec<Row<'_>> {
    let mut machines: Vec<&Machine> = state.machines.iter().collect();
    machines.sort_by_key(|m| (!is_local(m), m.name.as_str()));
    let mut out = Vec::new();
    for m in machines {
        let local = is_local(m);
        out.push(Row::Machine(m));
        let mut agents: Vec<&Agent> = m.agents.iter().collect();
        agents.sort_by_key(|a| agent_key(a));
        let mut sessions: Vec<&str> = Vec::new();
        for a in &agents {
            if !sessions.contains(&session_of(a)) {
                sessions.push(session_of(a));
            }
        }
        // `agents` is sorted, so sessions already come most-urgent first.
        for s in sessions {
            out.push(Row::Session { name: s, local });
            out.extend(
                agents
                    .iter()
                    .filter(|a| session_of(a) == s)
                    .map(|a| Row::Agent { agent: a, local }),
            );
        }
    }
    out
}

/// Like [`rows`], but local sessions found in `layouts` list their agents
/// under the tab that holds each pane, in tab and pane order, as zj-radar
/// does. The `current` session comes first on the local machine and shows
/// every tab; other sessions show only tabs with agents. Remote machines
/// and agents outside Zellij keep the flat session grouping.
pub fn tree<'a>(
    state: &'a StateV1,
    current: Option<&str>,
    layouts: &'a [SessionLayout],
) -> Vec<Row<'a>> {
    let mut machines: Vec<&Machine> = state.machines.iter().collect();
    machines.sort_by_key(|m| (!is_local(m), m.name.as_str()));
    let mut out = Vec::new();
    for m in machines {
        let local = is_local(m);
        out.push(Row::Machine(m));
        let mut agents: Vec<&Agent> = m.agents.iter().collect();
        agents.sort_by_key(|a| agent_key(a));
        let mut sessions: Vec<&str> = Vec::new();
        if local {
            if let Some(cur) = layouts.iter().find(|l| Some(l.name.as_str()) == current) {
                sessions.push(&cur.name);
            }
        }
        for a in &agents {
            if !sessions.contains(&session_of(a)) {
                sessions.push(session_of(a));
            }
        }
        for s in sessions {
            out.push(Row::Session { name: s, local });
            let mut in_session: Vec<&Agent> = agents
                .iter()
                .copied()
                .filter(|a| session_of(a) == s)
                .collect();
            let layout = layouts
                .iter()
                .find(|l| local && s != NO_SESSION && l.name == s);
            if let Some(layout) = layout {
                let mut tabs: Vec<&TabLayout> = layout.tabs.iter().collect();
                tabs.sort_by_key(|t| t.position);
                for tab in tabs {
                    let mut here: Vec<&Agent> = in_session
                        .iter()
                        .copied()
                        .filter(|a| {
                            a.pane
                                .as_ref()
                                .is_some_and(|p| tab.panes.contains(&p.pane_id))
                        })
                        .collect();
                    if here.is_empty() && Some(s) != current {
                        continue;
                    }
                    in_session.retain(|a| !here.contains(a));
                    here.sort_by_key(|a| a.pane.as_ref().map(|p| p.pane_id));
                    let state = here
                        .iter()
                        .map(|a| a.state)
                        .min_by_key(|st| st.attention_rank());
                    out.push(Row::Tab {
                        session: s,
                        tab,
                        state,
                    });
                    out.extend(here.into_iter().map(|a| Row::Agent { agent: a, local }));
                }
            }
            // No layout yet, outside Zellij, remote, or a pane no tab lists.
            out.extend(
                in_session
                    .into_iter()
                    .map(|a| Row::Agent { agent: a, local }),
            );
        }
    }
    out
}

/// Agents the plugin can jump to: local, inside Zellij, blocked or idle.
/// Blocked come before idle, each longest-waiting first.
pub fn attention_order(state: &StateV1) -> Vec<&Agent> {
    let mut out: Vec<&Agent> = state
        .machines
        .iter()
        .filter(|m| is_local(m) && m.status == MachineStatus::Ok)
        .flat_map(|m| &m.agents)
        .filter(|a| a.pane.is_some() && matches!(a.state, AgentState::Blocked | AgentState::Idle))
        .collect();
    out.sort_by_key(|a| agent_key(a));
    out
}

/// The agent after (or before) `current` in the attention order, wrapping.
pub fn cycle<'a>(order: &[&'a Agent], current: Option<&str>, forward: bool) -> Option<&'a Agent> {
    if order.is_empty() {
        return None;
    }
    let len = order.len();
    let pos = current.and_then(|id| order.iter().position(|a| a.id == id));
    let next = match (pos, forward) {
        (Some(i), true) => (i + 1) % len,
        (Some(i), false) => (i + len - 1) % len,
        (None, true) => 0,
        (None, false) => len - 1,
    };
    Some(order[next])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Pane;

    fn agent(
        machine: &str,
        pid: u32,
        state: AgentState,
        since: u64,
        session: Option<&str>,
    ) -> Agent {
        Agent {
            id: Agent::make_id(machine, pid),
            machine: machine.into(),
            kind: "claude".into(),
            pid,
            cwd: None,
            state,
            since,
            pane: session.map(|s| Pane {
                session: s.into(),
                pane_id: pid,
            }),
            session_id: None,
        }
    }

    fn state() -> StateV1 {
        StateV1::new(
            0,
            vec![
                Machine {
                    name: "far".into(),
                    ssh: Some("far".into()),
                    status: MachineStatus::Ok,
                    error: None,
                    agents: vec![agent("far", 9, AgentState::Blocked, 1, Some("x"))],
                },
                Machine {
                    name: "box".into(),
                    ssh: None,
                    status: MachineStatus::Ok,
                    error: None,
                    agents: vec![
                        agent("box", 1, AgentState::Working, 5, Some("a")),
                        agent("box", 2, AgentState::Idle, 50, Some("b")),
                        agent("box", 3, AgentState::Blocked, 30, Some("a")),
                        agent("box", 4, AgentState::Idle, 10, None),
                        agent("box", 5, AgentState::Blocked, 20, Some("b")),
                    ],
                },
            ],
        )
    }

    fn describe(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                Row::Machine(m) => format!("M {}", m.name),
                Row::Session { name, .. } => format!("S {name}"),
                Row::Tab { tab, state, .. } => {
                    format!("T {} {}", tab.name, state.map_or("-", |s| s.as_str()))
                }
                Row::Agent { agent, .. } => format!("A {}", agent.pid),
            })
            .collect()
    }

    #[test]
    fn groups_by_machine_then_session_blocked_first() {
        let st = state();
        assert_eq!(
            describe(&rows(&st)),
            [
                "M box",
                "S b",
                "A 5",
                "A 2",
                "S a",
                "A 3",
                "A 1",
                &format!("S {NO_SESSION}"),
                "A 4",
                "M far",
                "S x",
                "A 9"
            ]
        );
    }

    fn tab(position: usize, name: &str, panes: &[u32]) -> TabLayout {
        TabLayout {
            position,
            name: name.into(),
            active: position == 0,
            panes: panes.to_vec(),
        }
    }

    #[test]
    fn tree_groups_local_agents_by_tab() {
        let st = state();
        let layouts = vec![
            SessionLayout {
                name: "a".into(),
                tabs: vec![
                    tab(1, "two", &[1]),
                    tab(0, "one", &[3, 7]),
                    tab(2, "empty", &[8]),
                ],
            },
            SessionLayout {
                name: "b".into(),
                // Pane 2 sits in no tab, say the layout is a beat behind.
                tabs: vec![tab(0, "x", &[5]), tab(1, "quiet", &[9])],
            },
        ];
        assert_eq!(
            describe(&tree(&st, Some("a"), &layouts)),
            [
                "M box",
                // The current session first, every tab shown.
                "S a",
                "T one blocked",
                "A 3",
                "T two working",
                "A 1",
                "T empty -",
                "S b",
                "T x blocked",
                "A 5",
                "A 2",
                &format!("S {NO_SESSION}"),
                "A 4",
                "M far",
                "S x",
                "A 9"
            ]
        );
    }

    #[test]
    fn tree_without_layouts_matches_rows() {
        let st = state();
        assert_eq!(describe(&tree(&st, Some("a"), &[])), describe(&rows(&st)));
    }

    #[test]
    fn attention_order_is_local_blocked_then_idle_with_panes() {
        let st = state();
        let pids: Vec<u32> = attention_order(&st).iter().map(|a| a.pid).collect();
        assert_eq!(pids, [5, 3, 2]);
    }

    #[test]
    fn cycle_wraps_both_ways() {
        let st = state();
        let order = attention_order(&st);
        let pid = |a: Option<&Agent>| a.map(|a| a.pid);
        assert_eq!(pid(cycle(&order, None, true)), Some(5));
        assert_eq!(pid(cycle(&order, None, false)), Some(2));
        assert_eq!(pid(cycle(&order, Some("box/5"), true)), Some(3));
        assert_eq!(pid(cycle(&order, Some("box/2"), true)), Some(5));
        assert_eq!(pid(cycle(&order, Some("box/5"), false)), Some(2));
        assert_eq!(pid(cycle(&order, Some("gone/1"), true)), Some(5));
        assert_eq!(pid(cycle(&[], None, true)), None);
    }
}
