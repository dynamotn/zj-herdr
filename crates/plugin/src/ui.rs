//! Text rendering for the picker, independent of the Zellij host.

use zj_herdr_core::view::{self, Row};
use zj_herdr_core::{fmt_elapsed, fmt_tokens, Agent, AgentState, MachineStatus, StateV1};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const INVERSE: &str = "\x1b[7m";

fn color(state: AgentState) -> &'static str {
    match state {
        AgentState::Blocked => "\x1b[31m",
        AgentState::Idle => "\x1b[33m",
        AgentState::Working => "\x1b[32m",
        AgentState::Done | AgentState::Unknown => DIM,
    }
}

fn truncate(text: &str, cols: usize) -> String {
    if text.chars().count() <= cols {
        return text.to_string();
    }
    let mut out: String = text.chars().take(cols.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// What a click on a rendered line does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Agent(String),
    Tab {
        session: String,
        position: usize,
    },
    Session(String),
    /// The `↻ restore N agents` banner.
    Restore,
}

/// Ids of the agent rows, in display order: what the cursor moves over.
pub fn selectable(tree: &[Row]) -> Vec<String> {
    tree.iter()
        .filter_map(|r| match r {
            Row::Agent { agent, .. } => Some(agent.id.clone()),
            _ => None,
        })
        .collect()
}

fn agent_text(agent: &Agent, branch: &str, local: bool, ssh: Option<&str>, now: u64) -> String {
    let place = match (&agent.pane, local) {
        (Some(p), true) => format!("#{}", p.pane_id),
        (_, false) => format!("ssh:{}", ssh.unwrap_or(&agent.machine)),
        (None, true) => format!("pid {}", agent.pid),
    };
    let cwd = agent.cwd.as_deref().map(view::basename).unwrap_or("-");
    let tokens = agent.tokens.map(|t| fmt_tokens(t.total()));
    // The coloured glyph carries the state, leaving room in a 32-column
    // rail for the token total and some of the cwd.
    format!(
        "{branch}{} {:<6} {:<3} {:>4} {} {}",
        agent.state.glyph(),
        agent.kind,
        fmt_elapsed(now.saturating_sub(agent.since)),
        tokens.as_deref().unwrap_or("-"),
        place,
        cwd
    )
}

/// `3 working · 2 need you`, where blocked and idle agents need you; the
/// second part only when some do.
fn tally(state: &StateV1) -> String {
    let count = |f: fn(AgentState) -> bool| state.agents().filter(|a| f(a.state)).count();
    let working = count(|s| s == AgentState::Working);
    let need = count(|s| matches!(s, AgentState::Blocked | AgentState::Idle));
    if need > 0 {
        format!("{working} working · {need} need you")
    } else {
        format!("{working} working")
    }
}

/// What [`render`] draws besides the tree.
#[derive(Debug, Clone, Copy, Default)]
pub struct Frame<'a> {
    pub state: Option<&'a StateV1>,
    /// The session this instance runs in.
    pub current: Option<&'a str>,
    /// Id of the agent under the cursor.
    pub selected: Option<&'a str>,
    pub error: Option<&'a str>,
    /// Id of the agent waiting for a second click to move here.
    pub armed: Option<&'a str>,
    /// Agents `zj-herdr restore` would reopen in this session.
    pub restore: usize,
    /// Unix seconds.
    pub now: u64,
}

/// The picker as lines, each with what a click on it does: a summary,
/// then `tree` (from [`view::tree`]), scrolled so the selected agent stays
/// visible within `rows`. When the tree leaves two lines spare, the counts
/// move from the header to a footer on the last line.
pub fn render(
    frame: Frame,
    tree: &[Row],
    (rows, cols): (usize, usize),
) -> (Vec<String>, Vec<Option<Target>>) {
    let Frame {
        state,
        current,
        selected,
        error,
        armed,
        restore,
        now,
    } = frame;
    let mut head = Vec::new();
    let mut body: Vec<(String, bool, Option<Target>)> = Vec::new();
    match state {
        None => head.push(format!("{DIM}waiting for zj-herdr daemon…{RESET}")),
        Some(state) => {
            let count = |s: AgentState| state.agents().filter(|a| a.state == s).count();
            head.push(truncate(
                &format!(
                    "zj-herdr  {} blocked  {} idle  {} working",
                    count(AgentState::Blocked),
                    count(AgentState::Idle),
                    count(AgentState::Working)
                ),
                cols,
            ));
            let ssh_of = |name: &str| {
                state
                    .machines
                    .iter()
                    .find(|m| m.name == name)
                    .and_then(|m| m.ssh.clone())
            };
            let mut in_tab = false;
            for (i, row) in tree.iter().enumerate() {
                match row {
                    Row::Machine(m) => {
                        in_tab = false;
                        let mut text = m.name.clone();
                        if m.status == MachineStatus::Unreachable {
                            text.push_str(&format!(
                                " (unreachable: {})",
                                m.error.as_deref().unwrap_or("?")
                            ));
                        }
                        body.push((
                            format!("{BOLD}{}{RESET}", truncate(&text, cols)),
                            false,
                            None,
                        ));
                    }
                    Row::Session { name, local } => {
                        in_tab = false;
                        let here = current == Some(*name);
                        let text = truncate(&format!("  {name}"), cols);
                        let target = (*local && !here && *name != view::NO_SESSION)
                            .then(|| Target::Session(name.to_string()));
                        let style = if here { BOLD } else { "" };
                        body.push((format!("{style}{text}{RESET}"), false, target));
                    }
                    Row::Tab {
                        session,
                        tab,
                        state,
                    } => {
                        in_tab = true;
                        let spine = if tab.active && current == Some(*session) {
                            "▌"
                        } else {
                            " "
                        };
                        let glyph = state.map_or(" ", |s| s.glyph());
                        let text = truncate(
                            &format!("  {spine}{glyph} {} {}", tab.position + 1, tab.name),
                            cols,
                        );
                        let style = state.map_or(DIM, color);
                        let target = Target::Tab {
                            session: session.to_string(),
                            position: tab.position,
                        };
                        body.push((format!("{style}{text}{RESET}"), false, Some(target)));
                    }
                    Row::Agent { agent, local } => {
                        let more = in_tab && matches!(tree.get(i + 1), Some(Row::Agent { .. }));
                        let branch = match (in_tab, more) {
                            (false, _) => "    ",
                            (true, true) => "  ├ ",
                            (true, false) => "  └ ",
                        };
                        let ssh = ssh_of(&agent.machine);
                        let text = if armed == Some(agent.id.as_str()) {
                            format!("{branch}{} click again to move here", agent.state.glyph())
                        } else {
                            agent_text(agent, branch, *local, ssh.as_deref(), now)
                        };
                        let text = truncate(&text, cols);
                        let is_sel = selected == Some(agent.id.as_str());
                        let styled = if is_sel {
                            format!("{INVERSE}{text:<cols$}{RESET}")
                        } else {
                            format!("{}{text}{RESET}", color(agent.state))
                        };
                        body.push((styled, is_sel, Some(Target::Agent(agent.id.clone()))));
                        if let Some(waiting) = agent
                            .waiting
                            .as_deref()
                            .filter(|_| agent.state == AgentState::Blocked)
                        {
                            let stem = if more { "  │ " } else { "    " };
                            let text = truncate(&format!("{stem}↳ {waiting}"), cols);
                            body.push((
                                format!("{DIM}{text}{RESET}"),
                                false,
                                Some(Target::Agent(agent.id.clone())),
                            ));
                        }
                    }
                    Row::FoldedTabs { count } => {
                        in_tab = false;
                        let noun = if *count == 1 { "tab" } else { "tabs" };
                        let text = truncate(&format!("   +{count} {noun}"), cols);
                        body.push((format!("{DIM}{text}{RESET}"), false, None));
                    }
                }
            }
            if state.agents().next().is_none() {
                body.push((format!("{DIM}  no agents{RESET}"), false, None));
            }
        }
    }
    let mut head: Vec<(String, Option<Target>)> = head.into_iter().map(|l| (l, None)).collect();
    if state.is_some() && restore > 0 {
        let noun = if restore == 1 { "agent" } else { "agents" };
        let text = truncate(&format!("↻ restore {restore} {noun}"), cols);
        head.push((format!("{BOLD}{text}{RESET}"), Some(Target::Restore)));
    }
    if let Some(e) = error {
        head.push((format!("\x1b[31m{}{RESET}", truncate(e, cols)), None));
    }
    if let Some(state) = state {
        // With two lines to spare, the counts move to a footer at the
        // bottom of the rail, under a rule.
        let spare = rows.saturating_sub(head.len() + body.len());
        if spare >= 2 {
            head[0].0 = truncate("zj-herdr", cols);
            body.extend((2..spare).map(|_| (String::new(), false, None)));
            body.push((format!("{DIM}{}{RESET}", "─".repeat(cols)), false, None));
            body.push((truncate(&tally(state), cols), false, None));
        }
    }

    let room = rows.saturating_sub(head.len());
    let sel_line = body.iter().position(|(_, s, _)| *s).unwrap_or(0);
    let start = (sel_line + 1).saturating_sub(room);
    head.into_iter()
        .chain(
            body.into_iter()
                .skip(start)
                .take(room)
                .map(|(l, _, t)| (l, t)),
        )
        .take(rows)
        .unzip()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zj_herdr_core::{Machine, Pane, Tokens};

    fn state(n: u32) -> StateV1 {
        let agents = (1..=n)
            .map(|pid| Agent {
                id: format!("box/{pid}"),
                machine: "box".into(),
                kind: "claude".into(),
                pid,
                cwd: Some(format!("/w/proj{pid}")),
                state: if pid == 1 {
                    AgentState::Blocked
                } else {
                    AgentState::Working
                },
                since: 100,
                pane: Some(Pane::new("dev", pid)),
                session_id: None,
                waiting: None,
                started: 0,
                tokens: (pid == 1).then_some(Tokens {
                    input: 4_800_000,
                    output: 70_000,
                    ..Tokens::default()
                }),
            })
            .collect();
        let remote = Machine {
            name: "far".into(),
            ssh: Some("far-host".into()),
            status: MachineStatus::Unreachable,
            error: Some("timeout".into()),
            agents: vec![],
        };
        StateV1::new(
            0,
            vec![
                Machine {
                    name: "box".into(),
                    ssh: None,
                    status: MachineStatus::Ok,
                    error: None,
                    agents,
                },
                remote,
            ],
        )
    }

    fn plain(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                let mut out = String::new();
                let mut esc = false;
                for c in l.chars() {
                    match (esc, c) {
                        (false, '\x1b') => esc = true,
                        (true, 'm') => esc = false,
                        (false, c) => out.push(c),
                        _ => {}
                    }
                }
                out.trim_end().to_string()
            })
            .collect()
    }

    /// `render` over the flat tree (no layouts), as plain text.
    fn draw(
        st: &StateV1,
        selected: Option<&str>,
        error: Option<&str>,
        now: u64,
        size: (usize, usize),
    ) -> Vec<String> {
        let tree = view::tree(st, None, &[]);
        let frame = Frame {
            state: Some(st),
            selected,
            error,
            now,
            ..Frame::default()
        };
        plain(&render(frame, &tree, size).0)
    }

    /// The frame of an instance in session `dev`.
    fn dev_frame(st: &StateV1) -> Frame<'_> {
        Frame {
            state: Some(st),
            current: Some("dev"),
            now: 160,
            ..Frame::default()
        }
    }

    #[test]
    fn restore_banner_sits_under_the_header() {
        let st = state(2);
        let tree = view::tree(&st, None, &[]);
        let frame = Frame {
            restore: 3,
            error: Some("oops"),
            ..dev_frame(&st)
        };
        let (lines, targets) = render(frame, &tree, (7, 80));
        let lines = plain(&lines);
        assert!(lines[0].starts_with("zj-herdr  1 blocked"));
        assert_eq!(lines[1], "↻ restore 3 agents");
        assert_eq!(targets[1], Some(Target::Restore));
        assert_eq!(lines[2], "oops");
        assert_eq!(lines[3], "box");
        let frame = Frame {
            restore: 1,
            ..dev_frame(&st)
        };
        let lines = plain(&render(frame, &tree, (20, 80)).0);
        assert_eq!(lines[0], "zj-herdr");
        assert_eq!(lines[1], "↻ restore 1 agent");
        assert_eq!(lines[19], "1 working · 1 need you");
        // Nothing to restore, no banner.
        let lines = plain(&render(dev_frame(&st), &tree, (20, 80)).0);
        assert_eq!(lines[1], "box");
    }

    #[test]
    fn an_armed_agent_asks_for_a_second_click() {
        let mut st = state(2);
        st.machines[0].agents[1].pane = None;
        let tree = view::tree(&st, None, &[]);
        let frame = Frame {
            armed: Some("box/2"),
            ..dev_frame(&st)
        };
        let (lines, targets) = render(frame, &tree, (6, 80));
        let lines = plain(&lines);
        assert_eq!(lines[3], "    ! claude 1m  4.8M #1 proj1");
        assert_eq!(lines[5], "    ● click again to move here");
        assert_eq!(targets[5], Some(Target::Agent("box/2".into())));
    }

    #[test]
    fn renders_summary_groups_and_rows() {
        let st = state(2);
        let lines = draw(&st, Some("box/1"), None, 160, (6, 80));
        assert_eq!(lines[0], "zj-herdr  1 blocked  0 idle  1 working");
        assert_eq!(lines[1], "box");
        assert_eq!(lines[2], "  dev");
        assert_eq!(lines[3], "    ! claude 1m  4.8M #1 proj1");
        assert_eq!(lines[5], "far (unreachable: timeout)");
    }

    #[test]
    fn spare_room_moves_the_counts_to_a_footer() {
        let st = state(2);
        // Header and five tree lines leave exactly two spare in eight rows.
        let lines = draw(&st, None, None, 160, (8, 30));
        assert_eq!(lines.len(), 8);
        assert_eq!(lines[0], "zj-herdr");
        assert_eq!(lines[5], "far (unreachable: timeout)");
        assert_eq!(lines[6], "─".repeat(30));
        assert_eq!(lines[7], "1 working · 1 need you");
        // More room pads above the rule, so the tally stays on the last line.
        let lines = draw(&st, None, None, 160, (12, 30));
        assert_eq!(lines.len(), 12);
        assert_eq!(lines[6..10], ["", "", "", ""]);
        assert_eq!(lines[11], "1 working · 1 need you");
        // One line short: no footer, the header keeps the counts.
        let lines = draw(&st, None, None, 160, (7, 80));
        assert_eq!(lines.len(), 6);
        assert!(lines[0].starts_with("zj-herdr  1 blocked"));
        // Nobody needs you: just the working count.
        let mut st = state(2);
        st.machines[0].agents[0].state = AgentState::Done;
        let lines = draw(&st, None, None, 160, (12, 80));
        assert_eq!(lines[11], "1 working");
    }

    #[test]
    fn renders_tabs_with_branches_and_click_targets() {
        let st = state(3);
        let layouts = vec![view::SessionLayout {
            name: "dev".into(),
            tabs: vec![
                view::TabLayout {
                    position: 0,
                    name: "code".into(),
                    active: true,
                    panes: vec![1, 2],
                },
                view::TabLayout {
                    position: 1,
                    name: "notes".into(),
                    active: false,
                    panes: vec![3],
                },
                view::TabLayout {
                    position: 2,
                    name: "shell".into(),
                    active: false,
                    panes: vec![],
                },
                view::TabLayout {
                    position: 3,
                    name: "logs".into(),
                    active: false,
                    panes: vec![9],
                },
            ],
        }];
        let tree = view::tree(&st, Some("dev"), &layouts);
        let (lines, targets) = render(dev_frame(&st), &tree, (20, 80));
        let lines = plain(&lines);
        assert_eq!(lines[2], "  dev");
        assert_eq!(lines[3], "  ▌! 1 code");
        assert_eq!(lines[4], "  ├ ! claude 1m  4.8M #1 proj1");
        assert_eq!(lines[5], "  └ ● claude 1m     - #2 proj2");
        assert_eq!(lines[6], "   ● 2 notes");
        assert_eq!(lines[8], "   +2 tabs", "tabs without agents fold");
        assert_eq!(targets[8], None);
        assert_eq!(lines[9], "far (unreachable: timeout)");
        assert_eq!(targets[2], None, "the current session is no target");
        assert_eq!(
            targets[3],
            Some(Target::Tab {
                session: "dev".into(),
                position: 0
            })
        );
        assert_eq!(targets[5], Some(Target::Agent("box/2".into())));
        assert_eq!(lines.len(), targets.len());
    }

    #[test]
    fn blocked_agents_show_what_they_wait_on() {
        let mut st = state(3);
        for a in &mut st.machines[0].agents {
            a.state = AgentState::Blocked;
            a.waiting = Some(format!("Bash: make {}", a.pid));
        }
        // Not blocked any more: a stale reason is not shown.
        st.machines[0].agents[2].state = AgentState::Working;
        let layouts = vec![view::SessionLayout {
            name: "dev".into(),
            tabs: vec![view::TabLayout {
                position: 0,
                name: "code".into(),
                active: true,
                panes: vec![1, 2, 3],
            }],
        }];
        let tree = view::tree(&st, Some("dev"), &layouts);
        let (lines, targets) = render(dev_frame(&st), &tree, (20, 80));
        let lines = plain(&lines);
        assert_eq!(lines[4], "  ├ ! claude 1m  4.8M #1 proj1");
        assert_eq!(lines[5], "  │ ↳ Bash: make 1");
        assert_eq!(lines[6], "  ├ ! claude 1m     - #2 proj2");
        assert_eq!(lines[7], "  │ ↳ Bash: make 2");
        assert_eq!(lines[8], "  └ ● claude 1m     - #3 proj3");
        assert_eq!(lines[9], "far (unreachable: timeout)");
        assert_eq!(targets[5], Some(Target::Agent("box/1".into())));
        assert_eq!(selectable(&tree), ["box/1", "box/2", "box/3"]);

        // The last agent of a tab, and one outside any tab.
        st.machines[0].agents[2].state = AgentState::Blocked;
        let tree = view::tree(&st, Some("dev"), &layouts);
        let lines = plain(&render(dev_frame(&st), &tree, (20, 80)).0);
        assert_eq!(lines[9], "    ↳ Bash: make 3");
        let lines = draw(&st, None, None, 160, (20, 80));
        assert_eq!(lines[4], "    ↳ Bash: make 1");
    }

    #[test]
    fn scrolls_to_keep_selection_visible() {
        let st = state(30);
        let lines = draw(&st, Some("box/30"), None, 160, (6, 80));
        assert_eq!(lines.len(), 6);
        assert!(lines.last().unwrap().contains("proj30"), "{lines:?}");
    }

    #[test]
    fn truncates_to_width() {
        let st = state(1);
        for line in draw(&st, None, Some("a very long error message"), 0, (10, 12)) {
            assert!(line.chars().count() <= 12, "{line:?}");
        }
    }

    #[test]
    fn agent_rows_fit_the_rail_with_tokens_and_cwd() {
        let mut st = state(1);
        st.machines[0].agents[0].pane = Some(Pane::new("dev", 12));
        st.machines[0].agents[0].cwd = Some("/w/zj-herdr".into());
        let layouts = vec![view::SessionLayout {
            name: "dev".into(),
            tabs: vec![view::TabLayout {
                position: 0,
                name: "code".into(),
                active: true,
                panes: vec![12],
            }],
        }];
        let tree = view::tree(&st, Some("dev"), &layouts);
        let lines = plain(&render(dev_frame(&st), &tree, (20, 32)).0);
        assert_eq!(lines[4], "  └ ! claude 1m  4.8M #12 zj-he…");
        assert_eq!(lines[4].chars().count(), 32);
    }

    #[test]
    fn selection_follows_display_order() {
        let st = state(3);
        assert_eq!(
            selectable(&view::tree(&st, None, &[])),
            ["box/1", "box/2", "box/3"]
        );
    }
}
