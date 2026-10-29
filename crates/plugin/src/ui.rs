//! Text rendering for the picker, independent of the Zellij host.

use std::ops::Range;

use zj_herdr_core::view::{self, Row};
use zj_herdr_core::{
    fmt_elapsed, fmt_tokens, Agent, AgentState, MachineStatus, PluginCommand, StateV1,
};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const INVERSE: &str = "\x1b[7m";
const RED: &str = "\x1b[31m";
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// A state's glyph; working agents spin, one frame per second.
fn glyph(state: AgentState, now: u64) -> &'static str {
    match state {
        AgentState::Working => SPINNER[(now % 10) as usize],
        s => s.glyph(),
    }
}

/// One-cell mark for the agent kinds that have one, else the kind itself.
fn mark(kind: &str) -> &str {
    match kind {
        "claude" => "✳",
        "codex" => "❉",
        "opencode" => "✺",
        "gemini" => "✦",
        "pi" => "✴",
        other => other,
    }
}

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
    /// The `⇲` button of an agent outside Zellij.
    Adopt(String),
    /// A button of the footer bar.
    Command(PluginCommand),
}

/// Where a click on one line lands: column ranges and their targets,
/// buttons before the row itself, so the first match wins.
pub type Hits = Vec<(Range<usize>, Target)>;

/// What a click at column `col` of a line does.
pub fn hit(hits: &[(Range<usize>, Target)], col: usize) -> Option<&Target> {
    hits.iter().find(|(r, _)| r.contains(&col)).map(|(_, t)| t)
}

/// The whole line as one click target.
fn whole(target: Option<Target>) -> Hits {
    target.map(|t| vec![(0..usize::MAX, t)]).unwrap_or_default()
}

/// Whether `agent` can be moved into session `current`: a local agent
/// outside Zellij with a session to resume.
pub fn adoptable(agent: &Agent, local: bool, current: Option<&str>) -> bool {
    local
        && current.is_some()
        && agent.pane.is_none()
        && agent.session_id.is_some()
        && agent.state != AgentState::Done
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
    // The coloured glyph carries the state and the mark the kind, leaving
    // room in a 32-column rail for the token total and the cwd.
    format!(
        "{branch}{} {} {:<3} {:>4} {} {}",
        glyph(agent.state, now),
        mark(&agent.kind),
        fmt_elapsed(now.saturating_sub(agent.since)),
        tokens.as_deref().unwrap_or("-"),
        place,
        cwd
    )
}

/// Agents that need you: blocked and idle ones.
fn need_you(state: &StateV1) -> usize {
    state
        .agents()
        .filter(|a| matches!(a.state, AgentState::Blocked | AgentState::Idle))
        .count()
}

/// `3 working · 2 need you`; the second part only when some do.
fn tally(state: &StateV1) -> String {
    let working = state
        .agents()
        .filter(|a| a.state == AgentState::Working)
        .count();
    match need_you(state) {
        0 => format!("{working} working"),
        need => format!("{working} working · {need} need you"),
    }
}

/// `zj-herdr` with the agent count flush right, and `N!` in red when
/// agents need you: `zj-herdr      ·3 1!`. Just the title when too narrow.
fn header(state: &StateV1, cols: usize) -> String {
    let total = state.agents().count();
    let need = need_you(state);
    let alert = if need > 0 {
        format!(" {need}!")
    } else {
        String::new()
    };
    let badge = format!("·{total}{alert}");
    let title = "zj-herdr";
    let Some(gap) = cols
        .checked_sub(title.len() + badge.chars().count())
        .filter(|g| *g > 0)
    else {
        return format!("{BOLD}{}{RESET}", truncate(title, cols));
    };
    format!(
        "{BOLD}{title}{RESET}{}{DIM}·{total}{RESET}{RED}{BOLD}{alert}{RESET}",
        " ".repeat(gap)
    )
}

/// The footer bar: `[↵ jump] [n next] [p prev]`, as many as fit.
fn bar(cols: usize) -> (String, Hits) {
    let buttons = [
        ("↵", "jump", PluginCommand::Jump),
        ("n", "next", PluginCommand::AttentionNext),
        ("p", "prev", PluginCommand::AttentionPrev),
    ];
    let (mut text, mut hits, mut col) = (String::new(), Hits::new(), 0);
    for (key, name, cmd) in buttons {
        let gap = usize::from(col > 0);
        let width = key.chars().count() + name.len() + 3;
        if col + gap + width > cols {
            break;
        }
        text.push_str(&" ".repeat(gap));
        text.push_str(&format!(
            "{DIM}[{RESET}{BOLD}{key}{RESET} {name}{DIM}]{RESET}"
        ));
        col += gap;
        hits.push((col..col + width, Target::Command(cmd)));
        col += width;
    }
    (text, hits)
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
    /// Agents `zj-herdr restore` would reopen in this session.
    pub restore: usize,
    /// Unix seconds.
    pub now: u64,
}

/// The picker as lines, each with where a click on it lands: a header with
/// the counts over a `═` rule, then `tree` (from [`view::tree`]), scrolled
/// so the selected agent stays visible within `rows`. When the tree leaves
/// three lines spare, a footer fills the bottom: a rule, the tally and the
/// button bar.
pub fn render(
    frame: Frame,
    tree: &[Row],
    (rows, cols): (usize, usize),
) -> (Vec<String>, Vec<Hits>) {
    let Frame {
        state,
        current,
        selected,
        error,
        restore,
        now,
    } = frame;
    let mut head = Vec::new();
    let mut body: Vec<(String, bool, Hits)> = Vec::new();
    match state {
        None => head.push(format!("{DIM}waiting for zj-herdr daemon…{RESET}")),
        Some(state) => {
            head.push(header(state, cols));
            head.push(format!("{DIM}{}{RESET}", "═".repeat(cols)));
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
                            Hits::new(),
                        ));
                    }
                    Row::Session { name, local } => {
                        in_tab = false;
                        let here = current == Some(*name);
                        let text = truncate(&format!("  {name}"), cols);
                        let target = (*local && !here && *name != view::NO_SESSION)
                            .then(|| Target::Session(name.to_string()));
                        let style = if here { BOLD } else { "" };
                        body.push((format!("{style}{text}{RESET}"), false, whole(target)));
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
                        let glyph = state.map_or(" ", |s| glyph(s, now));
                        let text = truncate(
                            &format!("  {spine}{glyph} {} {}", tab.position + 1, tab.name),
                            cols,
                        );
                        let style = state.map_or(DIM, color);
                        let target = Target::Tab {
                            session: session.to_string(),
                            position: tab.position,
                        };
                        body.push((format!("{style}{text}{RESET}"), false, whole(Some(target))));
                    }
                    Row::Agent { agent, local } => {
                        let more = in_tab && matches!(tree.get(i + 1), Some(Row::Agent { .. }));
                        let branch = match (in_tab, more) {
                            (false, _) => "    ",
                            (true, true) => "  ├ ",
                            (true, false) => "  └ ",
                        };
                        let ssh = ssh_of(&agent.machine);
                        let text = agent_text(agent, branch, *local, ssh.as_deref(), now);
                        let row = whole(Some(Target::Agent(agent.id.clone())));
                        // An agent outside Zellij ends in a `⇲` button that
                        // moves it here.
                        let (text, hits) = if adoptable(agent, *local, current) && cols > 2 {
                            let width = cols - 2;
                            let text = format!("{:<width$} ⇲", truncate(&text, width));
                            let button = (cols - 1..cols, Target::Adopt(agent.id.clone()));
                            (text, [vec![button], row].concat())
                        } else {
                            (truncate(&text, cols), row)
                        };
                        let is_sel = selected == Some(agent.id.as_str());
                        let styled = if is_sel {
                            format!("{INVERSE}{text:<cols$}{RESET}")
                        } else {
                            format!("{}{text}{RESET}", color(agent.state))
                        };
                        body.push((styled, is_sel, hits));
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
                                whole(Some(Target::Agent(agent.id.clone()))),
                            ));
                        }
                    }
                    Row::FoldedTabs { count } => {
                        in_tab = false;
                        let noun = if *count == 1 { "tab" } else { "tabs" };
                        let text = truncate(&format!("   +{count} {noun}"), cols);
                        body.push((format!("{DIM}{text}{RESET}"), false, Hits::new()));
                    }
                }
            }
            if state.agents().next().is_none() {
                body.push((format!("{DIM}  no agents{RESET}"), false, Hits::new()));
            }
        }
    }
    let mut head: Vec<(String, Hits)> = head.into_iter().map(|l| (l, Hits::new())).collect();
    if state.is_some() && restore > 0 {
        let noun = if restore == 1 { "agent" } else { "agents" };
        let text = truncate(&format!("↻ restore {restore} {noun}"), cols);
        head.push((format!("{BOLD}{text}{RESET}"), whole(Some(Target::Restore))));
    }
    if let Some(e) = error {
        head.push((format!("{RED}{}{RESET}", truncate(e, cols)), Hits::new()));
    }
    if let Some(state) = state {
        // With three lines to spare, a footer fills the bottom of the
        // rail: a rule, the tally and the button bar.
        let spare = rows.saturating_sub(head.len() + body.len());
        if spare >= 3 {
            body.extend((3..spare).map(|_| (String::new(), false, Hits::new())));
            let rule = format!("{DIM}{}{RESET}", "─".repeat(cols));
            body.push((rule, false, Hits::new()));
            body.push((truncate(&tally(state), cols), false, Hits::new()));
            let (text, hits) = bar(cols);
            body.push((text, false, hits));
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

    /// What a click at column 0 of each line does.
    fn at_start(hits: &[Hits]) -> Vec<Option<Target>> {
        hits.iter().map(|h| hit(h, 0).cloned()).collect()
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
        let (lines, hits) = render(frame, &tree, (7, 80));
        let lines = plain(&lines);
        assert_eq!(lines[1], "═".repeat(80));
        assert_eq!(lines[2], "↻ restore 3 agents");
        assert_eq!(at_start(&hits)[2], Some(Target::Restore));
        assert_eq!(lines[3], "oops");
        assert_eq!(lines[4], "box");
        // Nothing to restore, no banner.
        let lines = plain(&render(dev_frame(&st), &tree, (20, 80)).0);
        assert_eq!(lines[2], "box");
    }

    #[test]
    fn header_shows_the_count_and_who_needs_you() {
        let st = state(2);
        let lines = draw(&st, None, None, 160, (6, 30));
        assert_eq!(lines[0], format!("zj-herdr{}·2 1!", " ".repeat(17)));
        assert_eq!(lines[1], "═".repeat(30));
        let lines = draw(&st, None, None, 160, (6, 12));
        assert_eq!(lines[0], "zj-herdr", "too narrow for the badge");
        let mut st = state(2);
        st.machines[0].agents[0].state = AgentState::Done;
        let lines = draw(&st, None, None, 160, (6, 30));
        assert!(lines[0].ends_with(" ·2"), "{lines:?}");
    }

    #[test]
    fn agents_outside_zellij_get_an_adopt_button() {
        let mut st = state(2);
        st.machines[0].agents[1].pane = None;
        st.machines[0].agents[1].session_id = Some("s".into());
        let tree = view::tree(&st, None, &[]);
        let (lines, hits) = render(dev_frame(&st), &tree, (20, 40));
        let lines = plain(&lines);
        let row = lines.iter().position(|l| l.ends_with('⇲')).unwrap();
        assert_eq!(lines[row].chars().count(), 40);
        assert!(lines[row].contains("pid 2"), "{lines:?}");
        assert_eq!(hit(&hits[row], 39), Some(&Target::Adopt("box/2".into())));
        assert_eq!(hit(&hits[row], 0), Some(&Target::Agent("box/2".into())));
        assert_eq!(lines.iter().filter(|l| l.ends_with('⇲')).count(), 1);
        // No session to resume, or not in a session: no button.
        let lines = plain(
            &render(
                Frame {
                    current: None,
                    ..dev_frame(&st)
                },
                &tree,
                (20, 40),
            )
            .0,
        );
        assert!(!lines.iter().any(|l| l.ends_with('⇲')));
        st.machines[0].agents[1].session_id = None;
        let tree = view::tree(&st, None, &[]);
        let lines = plain(&render(dev_frame(&st), &tree, (20, 40)).0);
        assert!(!lines.iter().any(|l| l.ends_with('⇲')));
    }

    #[test]
    fn renders_summary_groups_and_rows() {
        let st = state(2);
        let lines = draw(&st, Some("box/1"), None, 160, (7, 80));
        assert_eq!(lines[2], "box");
        assert_eq!(lines[3], "  dev");
        assert_eq!(lines[4], "    ◆ ✳ 1m  4.8M #1 proj1");
        assert_eq!(lines[5], "    ⠋ ✳ 1m     - #2 proj2");
        assert_eq!(lines[6], "far (unreachable: timeout)");
        // Working agents spin, one frame a second.
        let lines = draw(&st, None, None, 161, (7, 80));
        assert_eq!(lines[5], "    ⠙ ✳ 1m     - #2 proj2");
    }

    #[test]
    fn spare_room_fills_a_footer_with_the_tally_and_buttons() {
        let st = state(2);
        // Header, rule and five tree lines leave exactly three spare in ten.
        let (lines, hits) = render(
            Frame {
                state: Some(&st),
                now: 160,
                ..Frame::default()
            },
            &view::tree(&st, None, &[]),
            (10, 30),
        );
        let lines = plain(&lines);
        assert_eq!(lines.len(), 10);
        assert_eq!(lines[6], "far (unreachable: timeout)");
        assert_eq!(lines[7], "─".repeat(30));
        assert_eq!(lines[8], "1 working · 1 need you");
        assert_eq!(lines[9], "[↵ jump] [n next] [p prev]");
        let click = |col| hit(&hits[9], col).cloned();
        assert_eq!(click(0), Some(Target::Command(PluginCommand::Jump)));
        assert_eq!(click(8), None, "the gap between buttons");
        assert_eq!(
            click(9),
            Some(Target::Command(PluginCommand::AttentionNext))
        );
        assert_eq!(
            click(25),
            Some(Target::Command(PluginCommand::AttentionPrev))
        );
        assert_eq!(click(26), None);
        // More room pads above the rule, so the bar stays on the last line.
        let lines = draw(&st, None, None, 160, (14, 30));
        assert_eq!(lines[7..11], ["", "", "", ""]);
        assert_eq!(lines[13], "[↵ jump] [n next] [p prev]");
        // A narrow rail drops the buttons that do not fit.
        let lines = draw(&st, None, None, 160, (10, 20));
        assert_eq!(lines[9], "[↵ jump] [n next]");
        // One line short: no footer.
        assert_eq!(draw(&st, None, None, 160, (9, 80)).len(), 7);
        // Nobody needs you: just the working count.
        let mut st = state(2);
        st.machines[0].agents[0].state = AgentState::Done;
        let lines = draw(&st, None, None, 160, (12, 80));
        assert_eq!(lines[10], "1 working");
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
        let (lines, targets) = (plain(&lines), at_start(&targets));
        assert_eq!(lines[3], "  dev");
        assert_eq!(lines[4], "  ▌◆ 1 code");
        assert_eq!(lines[5], "  ├ ◆ ✳ 1m  4.8M #1 proj1");
        assert_eq!(lines[6], "  └ ⠋ ✳ 1m     - #2 proj2");
        assert_eq!(lines[7], "   ⠋ 2 notes");
        assert_eq!(lines[9], "   +2 tabs", "tabs without agents fold");
        assert_eq!(targets[9], None);
        assert_eq!(lines[10], "far (unreachable: timeout)");
        assert_eq!(targets[3], None, "the current session is no target");
        assert_eq!(
            targets[4],
            Some(Target::Tab {
                session: "dev".into(),
                position: 0
            })
        );
        assert_eq!(targets[6], Some(Target::Agent("box/2".into())));
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
        let (lines, targets) = (plain(&lines), at_start(&targets));
        assert_eq!(lines[5], "  ├ ◆ ✳ 1m  4.8M #1 proj1");
        assert_eq!(lines[6], "  │ ↳ Bash: make 1");
        assert_eq!(lines[7], "  ├ ◆ ✳ 1m     - #2 proj2");
        assert_eq!(lines[8], "  │ ↳ Bash: make 2");
        assert_eq!(lines[9], "  └ ⠋ ✳ 1m     - #3 proj3");
        assert_eq!(lines[10], "far (unreachable: timeout)");
        assert_eq!(targets[6], Some(Target::Agent("box/1".into())));
        assert_eq!(selectable(&tree), ["box/1", "box/2", "box/3"]);

        // The last agent of a tab, and one outside any tab.
        st.machines[0].agents[2].state = AgentState::Blocked;
        let tree = view::tree(&st, Some("dev"), &layouts);
        let lines = plain(&render(dev_frame(&st), &tree, (20, 80)).0);
        assert_eq!(lines[10], "    ↳ Bash: make 3");
        let lines = draw(&st, None, None, 160, (20, 80));
        assert_eq!(lines[5], "    ↳ Bash: make 1");
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
        assert_eq!(lines[5], "  └ ◆ ✳ 1m  4.8M #12 zj-herdr");
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
