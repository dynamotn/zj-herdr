//! What the plugin shows, kept free of Zellij so it is tested natively:
//! the grouped agent list and the attention cycle.

use std::collections::BTreeMap;

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
    /// Tabs of the current session with no agent, other than the active
    /// one, folded into one line after the session's tabs.
    FoldedTabs {
        count: usize,
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
/// its active tab and every tab with agents, the others folded into one
/// [`Row::FoldedTabs`]; other sessions show only tabs with agents. Remote machines
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
                let mut folded = 0;
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
                    if here.is_empty() && !tab.active {
                        folded += 1;
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
                if folded > 0 {
                    out.push(Row::FoldedTabs { count: folded });
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

/// `layouts`, plus a layout built from the agents' own `pane.tab` (as the
/// daemon saw it) for every local session that `layouts` lacks or knows no
/// tabs of: Zellij's `SessionUpdate` may leave other sessions out.
pub fn with_agent_tabs(state: &StateV1, mut layouts: Vec<SessionLayout>) -> Vec<SessionLayout> {
    let known: Vec<String> = layouts
        .iter()
        .filter(|l| !l.tabs.is_empty())
        .map(|l| l.name.clone())
        .collect();
    let mut built: Vec<SessionLayout> = Vec::new();
    let panes = state
        .machines
        .iter()
        .filter(|m| is_local(m))
        .flat_map(|m| &m.agents)
        .filter_map(|a| a.pane.as_ref());
    for pane in panes {
        let Some(tab) = &pane.tab else {
            continue;
        };
        if known.contains(&pane.session) {
            continue;
        }
        let layout = match built.iter_mut().position(|l| l.name == pane.session) {
            Some(i) => &mut built[i],
            None => {
                built.push(SessionLayout {
                    name: pane.session.clone(),
                    tabs: vec![],
                });
                built.last_mut().expect("just pushed")
            }
        };
        let slot = match layout.tabs.iter().position(|t| t.position == tab.position) {
            Some(i) => &mut layout.tabs[i],
            None => {
                layout.tabs.push(TabLayout {
                    position: tab.position,
                    name: tab.name.clone(),
                    active: false,
                    panes: vec![],
                });
                layout.tabs.last_mut().expect("just pushed")
            }
        };
        slot.panes.push(pane.pane_id);
    }
    layouts.retain(|l| !built.iter().any(|b| b.name == l.name));
    layouts.extend(built);
    layouts
}

/// The last component of a path: `proj` for `/w/proj/`.
pub fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

/// Zellij's name for a tab nobody named: `Tab #<n>`.
pub fn is_default_tab_name(name: &str) -> bool {
    name.strip_prefix("Tab #")
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// Tabs of `layout` (the current session) to rename after their agent's
/// directory, as `(position, name)`: a tab still named `Tab #<n>`, or
/// named what `given` (by position) says this instance last named it,
/// that holds a running local agent with a cwd, takes the basename of the
/// first such agent's cwd in pane order. A tab the user renamed is left
/// alone, and so is one whose rename is already on its way.
pub fn tab_renames(
    state: &StateV1,
    layout: &SessionLayout,
    given: &BTreeMap<usize, String>,
) -> Vec<(usize, String)> {
    let mut agents: Vec<&Agent> = state
        .machines
        .iter()
        .filter(|m| is_local(m))
        .flat_map(|m| &m.agents)
        .filter(|a| a.state != AgentState::Done && a.cwd.is_some())
        .filter(|a| a.pane.as_ref().is_some_and(|p| p.session == layout.name))
        .collect();
    agents.sort_by_key(|a| a.pane.as_ref().map(|p| p.pane_id));
    let mut out = Vec::new();
    for tab in &layout.tabs {
        let ours = given.get(&tab.position);
        if !is_default_tab_name(&tab.name) && ours != Some(&tab.name) {
            continue;
        }
        let first = agents.iter().find(|a| {
            a.pane
                .as_ref()
                .is_some_and(|p| tab.panes.contains(&p.pane_id))
        });
        let Some(name) = first
            .and_then(|a| a.cwd.as_deref())
            .map(basename)
            .filter(|n| !n.is_empty())
        else {
            continue;
        };
        if name != tab.name && ours.map(String::as_str) != Some(name) {
            out.push((tab.position, name.to_string()));
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
            pane: session.map(|s| Pane::new(s, pid)),
            session_id: None,
            waiting: None,
            started: 0,
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
                Row::FoldedTabs { count } => format!("F {count}"),
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
                    tab(3, "spare", &[]),
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
                // The current session first, tabs without agents folded.
                "S a",
                "T one blocked",
                "A 3",
                "T two working",
                "A 1",
                "F 2",
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

        // The active tab always shows, agents or not.
        let mut layouts = layouts;
        layouts[0].tabs[1].active = false;
        layouts[0].tabs[3].active = true;
        assert_eq!(
            describe(&tree(&st, Some("a"), &layouts))[2..8],
            [
                "T one blocked",
                "A 3",
                "T two working",
                "A 1",
                "T spare -",
                "F 1"
            ]
        );
    }

    #[test]
    fn sessions_zellij_left_out_get_tabs_from_the_daemon() {
        let mut st = state();
        let set_tab = |st: &mut StateV1, machine: usize, pid: u32, position, name: &str| {
            let a = st.machines[machine]
                .agents
                .iter_mut()
                .find(|a| a.pid == pid)
                .unwrap();
            a.pane.as_mut().unwrap().tab = Some(crate::PaneTab {
                position,
                name: name.into(),
            });
        };
        set_tab(&mut st, 1, 2, 1, "docs");
        set_tab(&mut st, 1, 5, 0, "api");
        set_tab(&mut st, 1, 3, 0, "stale");
        set_tab(&mut st, 0, 9, 0, "remote");
        // `a` is known with its tabs; `b` came back from SessionUpdate empty.
        let given = vec![
            SessionLayout {
                name: "a".into(),
                tabs: vec![tab(0, "one", &[3, 1])],
            },
            SessionLayout {
                name: "b".into(),
                tabs: vec![],
            },
        ];
        let layouts = with_agent_tabs(&st, given);
        assert_eq!(layouts.len(), 2);
        assert_eq!(layouts[0].tabs[0].name, "one", "Zellij's own answer wins");
        let b = &layouts[1];
        assert_eq!(b.name, "b");
        assert_eq!(
            b.tabs
                .iter()
                .map(|t| (t.position, t.name.as_str(), t.panes.clone()))
                .collect::<Vec<_>>(),
            [(1, "docs", vec![2]), (0, "api", vec![5])]
        );
        assert!(
            layouts.iter().all(|l| l.name != "x"),
            "remote sessions get none"
        );
        assert_eq!(
            describe(&tree(&st, Some("a"), &layouts))[..8],
            [
                "M box",
                "S a",
                "T one blocked",
                "A 1",
                "A 3",
                "S b",
                "T api blocked",
                "A 5"
            ]
        );
    }

    #[test]
    fn default_tab_names() {
        assert!(is_default_tab_name("Tab #1"));
        assert!(is_default_tab_name("Tab #12"));
        assert!(!is_default_tab_name("Tab #"));
        assert!(!is_default_tab_name("Tab #2 api"));
        assert!(!is_default_tab_name("api"));
    }

    #[test]
    fn tabs_take_their_first_agents_directory() {
        let mut st = state();
        for a in &mut st.machines[1].agents {
            a.cwd = Some(format!("/w/proj{}/", a.pid));
        }
        // The first agent in pane order (pid 1) has exited.
        st.machines[1].agents[0].state = AgentState::Done;
        let layout = SessionLayout {
            name: "a".into(),
            tabs: vec![
                tab(0, "Tab #1", &[3, 1]),
                tab(1, "mine", &[1]),
                tab(2, "Tab #3", &[8]),
                tab(3, "proj3", &[3]),
                tab(4, "old", &[3]),
                tab(5, "Tab #6", &[3]),
            ],
        };
        let given = BTreeMap::from([(4, "old".to_string()), (5, "proj3".to_string())]);
        assert_eq!(
            tab_renames(&st, &layout, &given),
            // 0: default name; 4: the name this instance gave it. Not 1
            // (renamed by the user), 2 (no agent), 3 (already right) or 5
            // (its rename is on the way).
            [(0, "proj3".to_string()), (4, "proj3".to_string())]
        );
        // Agents of other sessions do not count.
        let other = SessionLayout {
            name: "b".into(),
            ..layout.clone()
        };
        assert!(tab_renames(&st, &other, &BTreeMap::new()).is_empty());
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
