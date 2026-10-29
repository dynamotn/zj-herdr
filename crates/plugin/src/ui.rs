//! Text rendering for the picker, independent of the Zellij host.

use zj_herdr_core::view::{self, Row};
use zj_herdr_core::{fmt_elapsed, Agent, AgentState, MachineStatus, StateV1};

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

fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

/// What a click on a rendered line does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Agent(String),
    Tab { session: String, position: usize },
    Session(String),
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
    let cwd = agent.cwd.as_deref().map(basename).unwrap_or("-");
    format!(
        "{branch}{} {:<8} {:<7} {:<6} {} {}",
        agent.state.glyph(),
        agent.state.as_str(),
        agent.kind,
        fmt_elapsed(now.saturating_sub(agent.since)),
        place,
        cwd
    )
}

/// The picker as lines, each with what a click on it does: a summary,
/// then `tree` (from [`view::tree`]), scrolled so the selected agent stays
/// visible within `rows`. `current` is the session this instance runs in.
pub fn render(
    state: Option<&StateV1>,
    tree: &[Row],
    current: Option<&str>,
    selected: Option<&str>,
    error: Option<&str>,
    now: u64,
    (rows, cols): (usize, usize),
) -> (Vec<String>, Vec<Option<Target>>) {
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
                        let branch = if !in_tab {
                            "    "
                        } else if matches!(tree.get(i + 1), Some(Row::Agent { .. })) {
                            "  ├ "
                        } else {
                            "  └ "
                        };
                        let ssh = ssh_of(&agent.machine);
                        let text = truncate(
                            &agent_text(agent, branch, *local, ssh.as_deref(), now),
                            cols,
                        );
                        let is_sel = selected == Some(agent.id.as_str());
                        let styled = if is_sel {
                            format!("{INVERSE}{text:<cols$}{RESET}")
                        } else {
                            format!("{}{text}{RESET}", color(agent.state))
                        };
                        body.push((styled, is_sel, Some(Target::Agent(agent.id.clone()))));
                    }
                }
            }
            if state.agents().next().is_none() {
                body.push((format!("{DIM}  no agents{RESET}"), false, None));
            }
        }
    }
    if let Some(e) = error {
        head.push(format!("\x1b[31m{}{RESET}", truncate(e, cols)));
    }

    let room = rows.saturating_sub(head.len());
    let sel_line = body.iter().position(|(_, s, _)| *s).unwrap_or(0);
    let start = (sel_line + 1).saturating_sub(room);
    head.into_iter()
        .map(|l| (l, None))
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
    use zj_herdr_core::{Machine, Pane};

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
        plain(&render(Some(st), &tree, None, selected, error, now, size).0)
    }

    #[test]
    fn renders_summary_groups_and_rows() {
        let st = state(2);
        let lines = draw(&st, Some("box/1"), None, 160, (20, 80));
        assert_eq!(lines[0], "zj-herdr  1 blocked  0 idle  1 working");
        assert_eq!(lines[1], "box");
        assert_eq!(lines[2], "  dev");
        assert_eq!(lines[3], "    ! blocked  claude  1m     #1 proj1");
        assert_eq!(lines[5], "far (unreachable: timeout)");
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
            ],
        }];
        let tree = view::tree(&st, Some("dev"), &layouts);
        let (lines, targets) = render(Some(&st), &tree, Some("dev"), None, None, 160, (20, 80));
        let lines = plain(&lines);
        assert_eq!(lines[2], "  dev");
        assert_eq!(lines[3], "  ▌! 1 code");
        assert_eq!(lines[4], "  ├ ! blocked  claude  1m     #1 proj1");
        assert_eq!(lines[5], "  └ ● working  claude  1m     #2 proj2");
        assert_eq!(lines[6], "   ● 2 notes");
        assert_eq!(lines[8], "     3 shell");
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
    fn selection_follows_display_order() {
        let st = state(3);
        assert_eq!(
            selectable(&view::tree(&st, None, &[])),
            ["box/1", "box/2", "box/3"]
        );
    }
}
