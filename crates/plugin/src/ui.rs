//! Text rendering for the rail, independent of the Zellij host.

use std::collections::BTreeMap;
use std::ops::Range;

use zj_herdr_core::command::{self, Class};
use zj_herdr_core::view::{self, Row};
use zj_herdr_core::{
    fmt_elapsed, fmt_tokens, Agent, AgentState, MachineStatus, PluginCommand, StateV1, Task,
    TaskState,
};

use crate::theme::{self, Rgb, Theme};

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const ACCENT: &str = "\x1b[35m";
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// How long a row stays lit after it starts needing you.
const FLASH_SECS: u64 = 3;
/// Most task lines under an agent; past it, one fewer and `+N more`.
const TASK_LINES: usize = 3;
/// Cells of a context bar.
const BAR_CELLS: usize = 5;
/// Most entries the `earlier` list keeps.
pub const LEDGER_MAX: usize = 10;
/// Kind of a row for a terminal pane with no agent and no command known.
pub const PANE_KIND: &str = "pane";

/// How much room each tab takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Density {
    /// Each tab a tinted card, a blank line between cards.
    #[default]
    Cards,
    /// A blank line between tabs, no tint.
    Comfortable,
    /// Tabs flush.
    Compact,
}

impl Density {
    pub fn parse(text: &str) -> Option<Density> {
        match text {
            "cards" => Some(Density::Cards),
            "comfortable" => Some(Density::Comfortable),
            "compact" => Some(Density::Compact),
            _ => None,
        }
    }
}

/// Something that finished, for the `earlier` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// What finished: an agent id or a pane's command. A newer entry with
    /// the same key replaces the older one.
    pub key: String,
    pub mark: String,
    pub label: String,
    pub failed: bool,
    /// Unix seconds.
    pub at: u64,
}

/// One-cell mark for an agent kind or a command class, else the kind.
pub fn mark(kind: &str) -> &str {
    match kind {
        "claude" => "✳",
        "codex" => "❉",
        "opencode" => "✺",
        "gemini" => "✦",
        "pi" => "✴",
        PANE_KIND => "$",
        other => match Class::from_kind(other) {
            Some(class) => class.mark(),
            None => other,
        },
    }
}

/// `claude-opus-5-5` as `opus5.5`, `claude-sonnet-4-5-20250929` as
/// `sonnet4.5`; anything else without its date.
pub fn short_model(model: &str) -> String {
    let model = model.strip_prefix("claude-").unwrap_or(model);
    let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
    let parts: Vec<&str> = model
        .split('-')
        .filter(|p| !(p.len() == 8 && digits(p)))
        .collect();
    match parts.split_first() {
        Some((name, rest))
            if !rest.is_empty()
                && name.bytes().all(|b| b.is_ascii_alphabetic())
                && rest.iter().all(|p| digits(p)) =>
        {
            format!("{name}{}", rest.join("."))
        }
        _ => parts.join("-"),
    }
}

fn spinner(tick: u64) -> &'static str {
    SPINNER[(tick % SPINNER.len() as u64) as usize]
}

/// Glyph and colour of an agent's state; a command row is a spinner, or a
/// steady `▸` for a service.
fn look(agent: &Agent, tick: u64) -> (&'static str, &'static str) {
    if agent.kind == PANE_KIND {
        return ("·", DIM);
    }
    if let Some(class) = Class::from_kind(&agent.kind) {
        return (
            if class.is_steady() {
                "▸"
            } else {
                spinner(tick)
            },
            GREEN,
        );
    }
    match agent.state {
        AgentState::Working => (spinner(tick), GREEN),
        AgentState::Blocked => ("◆", RED),
        AgentState::Idle if agent.error => ("✗", RED),
        AgentState::Idle if agent.acked => ("○", DIM),
        AgentState::Idle => ("○", YELLOW),
        AgentState::Done => ("✓", DIM),
        AgentState::Unknown => ("?", DIM),
    }
}

/// Colour of a tab's roll-up state.
fn tab_color(state: AgentState) -> &'static str {
    match state {
        AgentState::Blocked => RED,
        AgentState::Idle => YELLOW,
        AgentState::Working => GREEN,
        AgentState::Done | AgentState::Unknown => DIM,
    }
}

/// Green below half, yellow below three quarters, red past.
fn heat(percent: usize) -> &'static str {
    match percent {
        0..50 => GREEN,
        50..75 => YELLOW,
        _ => RED,
    }
}

fn truncate(text: &str, cols: usize) -> String {
    if text.chars().count() <= cols {
        return text.to_string();
    }
    let mut out: String = text.chars().take(cols.saturating_sub(1)).collect();
    if cols > 0 {
        out.push('…');
    }
    out
}

/// Columns `text` takes on screen: its characters, escapes left out.
fn width(text: &str) -> usize {
    let mut esc = false;
    text.chars()
        .filter(|c| match (esc, c) {
            (false, '\x1b') => {
                esc = true;
                false
            }
            (true, 'm') => {
                esc = false;
                false
            }
            (esc, _) => !esc,
        })
        .count()
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
    /// The `✓` button of an idle agent: mark it seen.
    Ack(String),
    /// The `✕` button of an unreachable machine: hide it until it is back.
    Dismiss(String),
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
    // A background session is shown by a client in some pane: stopping it
    // to move it would take it from under that client.
    local
        && !agent.background
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

/// A line laid out as `left` (whose last part, `name`, gives way when
/// short of room) and `right` flush right, then buttons; each button's
/// columns become a hit before `row`.
struct Layout<'a> {
    left: String,
    name: &'a str,
    name_style: &'a str,
    right: String,
    buttons: Vec<(&'static str, &'static str, Target)>,
}

impl Layout<'_> {
    fn draw(self, cols: usize, row: Option<Target>) -> (String, Hits) {
        let buttons: usize = self.buttons.iter().map(|b| width(b.0) + 1).sum();
        let right = width(&self.right) + usize::from(!self.right.is_empty());
        let room = cols.saturating_sub(width(&self.left) + right + buttons);
        let name = truncate(self.name, room);
        let gap = room.saturating_sub(width(&name)) + usize::from(!self.right.is_empty());
        let mut text = format!(
            "{}{}{name}{RESET}{}{}",
            self.left,
            self.name_style,
            " ".repeat(gap),
            self.right
        );
        let mut col = width(&text);
        let mut hits = Hits::new();
        for (glyph, style, target) in self.buttons {
            text.push_str(&format!(" {style}{glyph}{RESET}"));
            col += 1;
            hits.push((col..col + width(glyph), target));
            col += width(glyph);
        }
        hits.extend(whole(row));
        (text, hits)
    }
}

/// `■■■□□ 62%`, cells and figure coloured by how full; `!` past three
/// quarters, `⚠` past nine tenths.
fn context_bar(percent: u8) -> String {
    let percent = usize::from(percent.min(100));
    let filled = (percent * BAR_CELLS + 50) / 100;
    let cells: String = (0..BAR_CELLS)
        .map(|i| {
            let style = if i < filled {
                heat((i + 1) * 100 / BAR_CELLS)
            } else {
                DIM
            };
            format!("{style}■{RESET}")
        })
        .collect();
    let warn = match percent {
        90.. => "⚠",
        75.. => "!",
        _ => "",
    };
    format!("{cells} {}{percent}%{warn}{RESET}", heat(percent))
}

/// The second line of an agent: model · tokens · context · place, keeping
/// the most telling fields that fit in `room` (tokens, then context, then
/// model, then place).
fn details(agent: &Agent, place: String, room: usize) -> String {
    let fields = [
        (
            2,
            agent
                .model
                .as_deref()
                .map(short_model)
                .map(|m| format!("{DIM}{m}{RESET}")),
        ),
        (
            0,
            agent
                .tokens
                .map(|t| format!("{DIM}{}{RESET}", fmt_tokens(t.total()))),
        ),
        (1, agent.context.map(context_bar)),
        (3, Some(format!("{DIM}{place}{RESET}"))),
    ];
    let sep = 3;
    let mut keep = [false; 4];
    let mut used = 0;
    for rank in 0..4 {
        for (i, (r, field)) in fields.iter().enumerate() {
            let Some(field) = field.as_ref().filter(|_| *r == rank) else {
                continue;
            };
            let need = width(field) + if used == 0 { 0 } else { sep };
            if used + need <= room {
                keep[i] = true;
                used += need;
            }
        }
    }
    fields
        .into_iter()
        .zip(keep)
        .filter_map(|((_, f), k)| f.filter(|_| k))
        .collect::<Vec<_>>()
        .join(&format!("{DIM} · {RESET}"))
}

/// Lines for an agent's background tasks: running first, then failed,
/// at most [`TASK_LINES`].
fn task_lines(tasks: &[Task], stem: &str, tick: u64, now: u64, cols: usize) -> Vec<String> {
    let rank = |t: &Task| match t.state {
        TaskState::Running => 0,
        TaskState::Failed => 1,
        _ => 2,
    };
    let mut tasks: Vec<&Task> = tasks.iter().collect();
    tasks.sort_by_key(|t| rank(t));
    let shown = if tasks.len() > TASK_LINES {
        TASK_LINES - 1
    } else {
        tasks.len()
    };
    // A failure is never folded away: it takes the last line shown.
    let failed = |t: &&Task| t.state == TaskState::Failed;
    if shown > 0 && !tasks[..shown].iter().any(failed) {
        if let Some(i) = tasks[shown..].iter().position(failed) {
            tasks.swap(shown - 1, shown + i);
        }
    }
    let mut lines: Vec<String> = tasks[..shown]
        .iter()
        .map(|t| {
            let class = (t.kind == "shell").then(|| command::classify(&t.label));
            let steady = class.is_some_and(Class::is_steady);
            let (glyph, style) = match t.state {
                TaskState::Running if steady => ("▸", GREEN),
                TaskState::Running => (spinner(tick), GREEN),
                TaskState::Completed => ("●", GREEN),
                TaskState::Failed => ("✗", RED),
                TaskState::Killed => ("○", DIM),
            };
            let age = match t.state {
                TaskState::Running if !steady && t.started > 0 => {
                    fmt_elapsed(now.saturating_sub(t.started))
                }
                _ => String::new(),
            };
            Layout {
                left: format!(
                    "{DIM}{stem}  ┊{RESET} {style}{glyph}{RESET} {} ",
                    class.map_or("✳", Class::mark)
                ),
                name: &t.label,
                name_style: if t.state == TaskState::Running {
                    ""
                } else {
                    DIM
                },
                right: format!("{DIM}{age}{RESET}"),
                buttons: vec![],
            }
            .draw(cols, None)
            .0
        })
        .collect();
    if tasks.len() > shown {
        let failed = tasks[shown..].iter().any(|t| t.state == TaskState::Failed);
        let style = if failed { RED } else { DIM };
        let more = format!("{stem}  ┊ +{} more", tasks.len() - shown);
        lines.push(format!("{style}{}{RESET}", truncate(&more, cols)));
    }
    lines
}

fn need_you(state: &StateV1) -> usize {
    state.agents().filter(|a| a.needs_you()).count()
}

/// Whether a row stands for a command or a bare pane, not an agent.
fn is_pane_row(agent: &Agent) -> bool {
    agent.kind == PANE_KIND || Class::from_kind(&agent.kind).is_some()
}

/// Agents, command and pane rows left out.
fn agents(state: &StateV1) -> impl Iterator<Item = &Agent> {
    state.agents().filter(|a| !is_pane_row(a))
}

/// `3 working · 2 need you`; the second part only when some do.
fn tally(state: &StateV1) -> String {
    let working = agents(state)
        .filter(|a| a.state == AgentState::Working)
        .count();
    match need_you(state) {
        0 => format!("{working} working"),
        need => format!("{working} working · {RED}{need} need you{RESET}"),
    }
}

/// `zj-herdr      ·3▲ 1!`: the agent count flush right, `▲` when tabs are
/// folded away, and `N!` in red when agents need you. Just the title when
/// too narrow.
fn header(state: &StateV1, cols: usize, folded: bool) -> String {
    let total = agents(state).count();
    let need = need_you(state);
    let more = if folded { "▲" } else { "" };
    let alert = if need > 0 {
        format!(" {need}!")
    } else {
        String::new()
    };
    let badge = format!("·{total}{more}{alert}");
    let title = "zj-herdr";
    let Some(gap) = cols
        .checked_sub(title.len() + badge.chars().count())
        .filter(|g| *g > 0)
    else {
        return format!("{BOLD}{}{RESET}", truncate(title, cols));
    };
    format!(
        "{BOLD}{title}{RESET}{}{DIM}·{total}{more}{RESET}{RED}{BOLD}{alert}{RESET}",
        " ".repeat(gap)
    )
}

/// The `═` rule under the header; while anything runs, an accent `◆`
/// sweeps along it.
fn rule(cols: usize, tick: u64, running: bool) -> String {
    let at = (running && cols > 0).then(|| (tick % cols as u64) as usize);
    let line: String = (0..cols)
        .map(|i| {
            if Some(i) == at {
                format!("{RESET}{ACCENT}◆{RESET}{DIM}")
            } else {
                "═".to_string()
            }
        })
        .collect();
    format!("{DIM}{line}{RESET}")
}

/// The footer bar: `↵ jump  n next  p prev`, as many as fit.
fn bar(cols: usize) -> (String, Hits) {
    let buttons = [
        ("↵", "jump", PluginCommand::Jump),
        ("n", "next", PluginCommand::AttentionNext),
        ("p", "prev", PluginCommand::AttentionPrev),
    ];
    let (mut text, mut hits, mut col) = (String::new(), Hits::new(), 0);
    for (key, name, cmd) in buttons {
        let gap = if col > 0 { 2 } else { 0 };
        let width = key.chars().count() + name.len() + 1;
        if col + gap + width > cols {
            break;
        }
        text.push_str(&" ".repeat(gap));
        text.push_str(&format!("{ACCENT}{BOLD}{key}{RESET} {name}"));
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
    /// Animation frame: spinners and the sweep move one step per frame.
    pub tick: u64,
    pub theme: Theme,
    pub density: Density,
    /// Recent completions, newest first.
    pub ledger: &'a [Finished],
    /// What command and pane rows show, by row id: the command line, or
    /// the pane's title.
    pub labels: Option<&'a BTreeMap<String, String>>,
}

/// The background a line is painted on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Surface {
    Rail,
    Idle,
    Busy,
    Active,
    Flash,
    Selected,
}

impl Surface {
    fn color(self, theme: &Theme) -> Rgb {
        match self {
            Surface::Rail => theme.rail,
            Surface::Idle => theme.idle,
            Surface::Busy => theme.busy,
            Surface::Active => theme.active,
            Surface::Flash => theme.flash,
            Surface::Selected => theme.selected,
        }
    }
}

struct Line {
    text: String,
    selected: bool,
    hits: Hits,
    /// Its own surface (flash, cursor) over its tab's.
    surface: Option<Surface>,
    /// The tab card it belongs to, an index into the groups.
    group: Option<usize>,
}

impl Line {
    fn new(text: String, hits: Hits, group: Option<usize>) -> Self {
        Line {
            text,
            selected: false,
            hits,
            surface: None,
            group,
        }
    }
}

/// A tab and the lines under it.
struct Group {
    surface: Surface,
    /// Holds the tab in view, the cursor, or an agent that runs or needs
    /// you: never folded away.
    keep: bool,
}

/// `text` cut to `cols` columns, escapes kept.
fn clip(text: &str, cols: usize) -> String {
    if width(text) <= cols {
        return text.to_string();
    }
    let (mut out, mut shown, mut esc) = (String::new(), 0, false);
    for c in text.chars() {
        match (esc, c) {
            (false, '\x1b') => esc = true,
            (true, 'm') => esc = false,
            (true, _) => {}
            (false, _) if shown == cols => continue,
            (false, _) => shown += 1,
        }
        out.push(c);
    }
    out.push_str(RESET);
    out
}

/// `text` on `color`, padded to `cols`, the colour restored after every
/// reset inside.
fn paint(text: &str, color: Rgb, cols: usize) -> String {
    let bg = theme::bg(color);
    let pad = " ".repeat(cols.saturating_sub(width(text)));
    format!(
        "{bg}{}{pad}{RESET}",
        text.replace(RESET, &format!("{RESET}{bg}"))
    )
}

/// The rail as lines, each with where a click on it lands: a header with
/// the counts over a `═` rule, then `tree` (from [`view::tree`]), scrolled
/// so the selected agent stays visible within `rows`. Tabs that are idle
/// fold into `+N idle` when the tree does not fit. Room left over fills
/// from the bottom: a footer (rule, tally, buttons) with three lines
/// spare, and above it the `earlier` list.
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
        tick,
        theme,
        density,
        ledger,
        labels,
    } = frame;
    let Some(state) = state else {
        let text = format!("{DIM}waiting for zj-herdr daemon…{RESET}");
        return (vec![text], vec![Hits::new()]);
    };
    let spaced = density != Density::Compact;
    let mut body: Vec<Line> = Vec::new();
    let mut groups: Vec<Group> = Vec::new();
    let ssh_of = |name: &str| {
        state
            .machines
            .iter()
            .find(|m| m.name == name)
            .and_then(|m| m.ssh.clone())
    };
    let mut group: Option<usize> = None;
    // A blank line after each tab card, part of the card so it folds too.
    let close = |body: &mut Vec<Line>, group: &mut Option<usize>| {
        if let Some(g) = group.take() {
            if spaced {
                body.push(Line::new(String::new(), Hits::new(), Some(g)));
            }
        }
    };
    for (i, row) in tree.iter().enumerate() {
        match row {
            Row::Machine(m) => {
                close(&mut body, &mut group);
                let down = m.status == MachineStatus::Unreachable;
                let label = if down {
                    format!(
                        "{} (unreachable: {})",
                        m.name,
                        m.error.as_deref().unwrap_or("?")
                    )
                } else {
                    m.name.clone()
                };
                let buttons = if down {
                    vec![("✕", DIM, Target::Dismiss(m.name.clone()))]
                } else {
                    vec![]
                };
                let (text, hits) = Layout {
                    left: String::new(),
                    name: &label,
                    name_style: if down { RED } else { BOLD },
                    right: String::new(),
                    buttons,
                }
                .draw(cols, None);
                body.push(Line::new(text, hits, None));
            }
            Row::Session { name, local } => {
                close(&mut body, &mut group);
                let here = current == Some(*name);
                let text = truncate(&format!("  {name}"), cols);
                let target = (*local && !here && *name != view::NO_SESSION)
                    .then(|| Target::Session(name.to_string()));
                let style = if here { BOLD } else { DIM };
                body.push(Line::new(
                    format!("{style}{text}{RESET}"),
                    whole(target),
                    None,
                ));
            }
            Row::Tab {
                session,
                tab,
                state: tab_state,
            } => {
                close(&mut body, &mut group);
                let active = tab.active && current == Some(*session);
                let surface = match tab_state {
                    _ if active => Surface::Active,
                    Some(AgentState::Working) => Surface::Busy,
                    _ => Surface::Idle,
                };
                groups.push(Group {
                    surface,
                    keep: active,
                });
                group = Some(groups.len() - 1);
                let spine = if active { "▌" } else { " " };
                let (glyph, style) = match tab_state {
                    Some(AgentState::Working) => (spinner(tick), GREEN),
                    Some(AgentState::Unknown) | None => (" ", DIM),
                    Some(s) => (s.glyph(), tab_color(*s)),
                };
                let (text, hits) = Layout {
                    left: format!(
                        "{ACCENT}{spine}{RESET}{style}{glyph}{RESET} {DIM}{}{RESET} ",
                        tab.position + 1
                    ),
                    name: &tab.name,
                    name_style: if active { BOLD } else { "" },
                    right: String::new(),
                    buttons: vec![],
                }
                .draw(
                    cols,
                    Some(Target::Tab {
                        session: session.to_string(),
                        position: tab.position,
                    }),
                );
                // Tab lines start two columns in, under the session name.
                body.push(Line::new(format!("  {text}"), shift(hits, 2), group));
            }
            Row::Agent { agent, local } => {
                let in_tab = group.is_some();
                let more = in_tab && matches!(tree.get(i + 1), Some(Row::Agent { .. }));
                let is_sel = selected == Some(agent.id.as_str());
                let branch = match (in_tab, more) {
                    (false, _) => "   ",
                    (true, true) => " ├ ",
                    (true, false) => " └ ",
                };
                let stem = if more { "  │ " } else { "    " };
                let marker = if is_sel {
                    format!("{ACCENT}►{RESET}")
                } else {
                    " ".to_string()
                };
                let (glyph, style) = look(agent, tick);
                let class = Class::from_kind(&agent.kind);
                let bare = agent.kind == PANE_KIND;
                let name = if is_pane_row(agent) {
                    labels
                        .and_then(|l| l.get(&agent.id))
                        .map_or(agent.kind.as_str(), String::as_str)
                } else {
                    agent.cwd.as_deref().map(view::basename).unwrap_or("-")
                };
                let steady = class.is_some_and(Class::is_steady);
                let elapsed = if steady || bare {
                    String::new()
                } else {
                    fmt_elapsed(now.saturating_sub(agent.since))
                };
                let mut buttons = Vec::new();
                if class.is_none() && agent.state == AgentState::Idle && !agent.acked {
                    buttons.push(("✓", GREEN, Target::Ack(agent.id.clone())));
                }
                if adoptable(agent, *local, current) {
                    buttons.push(("⇲", ACCENT, Target::Adopt(agent.id.clone())));
                }
                let target = Some(Target::Agent(agent.id.clone()));
                let (text, hits) = Layout {
                    left: format!(
                        "{marker}{DIM}{branch}{RESET}{style}{glyph}{RESET} {} ",
                        mark(&agent.kind)
                    ),
                    name,
                    name_style: if bare { DIM } else { BOLD },
                    right: format!("{DIM}{elapsed}{RESET}"),
                    buttons,
                }
                .draw(cols, target.clone());
                let g = group;
                if let Some(g) = g {
                    let urgent = agent.needs_you() || agent.state == AgentState::Working;
                    groups[g].keep |= urgent || is_sel;
                }
                let lit = agent.needs_you() && now.saturating_sub(agent.since) < FLASH_SECS;
                let mut line = Line::new(text, hits, g);
                line.selected = is_sel;
                line.surface = if is_sel {
                    Some(Surface::Selected)
                } else if lit {
                    Some(Surface::Flash)
                } else {
                    None
                };
                body.push(line);
                if is_pane_row(agent) {
                    continue;
                }
                let place = match (&agent.pane, *local) {
                    (Some(p), true) => format!("#{}", p.pane_id),
                    (_, false) => format!(
                        "ssh:{}",
                        ssh_of(&agent.machine).as_deref().unwrap_or(&agent.machine)
                    ),
                    (None, true) => format!("pid {}", agent.pid),
                };
                // A local pane id alone says nothing the tree does not: the
                // second line needs model, tokens or context, or a place
                // outside this machine's panes.
                let known =
                    agent.model.is_some() || agent.tokens.is_some() || agent.context.is_some();
                if known || !(*local && agent.pane.is_some()) {
                    let lead = format!("{stem}  ");
                    let info = details(agent, place, cols.saturating_sub(width(&lead)));
                    let mut second = Line::new(
                        format!("{DIM}{lead}{RESET}{info}"),
                        whole(target.clone()),
                        g,
                    );
                    second.surface = body.last().and_then(|l| l.surface);
                    body.push(second);
                }
                if let Some(waiting) = agent
                    .waiting
                    .as_deref()
                    .filter(|_| agent.state == AgentState::Blocked)
                {
                    let text = truncate(&format!("{stem}↳ {waiting}"), cols);
                    body.push(Line::new(
                        format!("{DIM}{text}{RESET}"),
                        whole(target.clone()),
                        g,
                    ));
                }
                for text in task_lines(&agent.tasks, stem, tick, now, cols) {
                    body.push(Line::new(text, whole(target.clone()), g));
                }
            }
            Row::FoldedTabs { count } => {
                close(&mut body, &mut group);
                let noun = if *count == 1 { "tab" } else { "tabs" };
                let text = truncate(&format!("   +{count} {noun}"), cols);
                body.push(Line::new(format!("{DIM}{text}{RESET}"), Hits::new(), None));
            }
        }
    }
    close(&mut body, &mut group);
    // A trailing blank line only wastes a row.
    if body.last().is_some_and(|l| l.text.is_empty()) {
        body.pop();
    }
    if state.agents().next().is_none() {
        body.push(Line::new(
            format!("{DIM}  no agents{RESET}"),
            Hits::new(),
            None,
        ));
    }

    let mut head: Vec<(String, Hits)> = Vec::new();
    if restore > 0 {
        let noun = if restore == 1 { "agent" } else { "agents" };
        let text = truncate(&format!("↻ restore {restore} {noun}"), cols);
        head.push((format!("{BOLD}{text}{RESET}"), whole(Some(Target::Restore))));
    }
    if let Some(e) = error {
        head.push((format!("{RED}{}{RESET}", truncate(e, cols)), Hits::new()));
    }
    // Title and rule go above these.
    let head_len = head.len() + 2;

    // Too long: fold idle tabs, the last first, into one `+N idle` line
    // where the first of them was.
    let room = rows.saturating_sub(head_len);
    let mut folded = 0;
    let mut fold_at = None;
    for g in (0..groups.len()).rev() {
        // Once anything folds, the `+N idle` line needs a row too.
        let fits = body.len() + usize::from(folded > 0) <= room || groups[g].keep;
        // Every group owns its tab line, so a group to fold always has a start.
        let Some(start) = body
            .iter()
            .position(|l| l.group == Some(g))
            .filter(|_| !fits)
        else {
            continue;
        };
        body.retain(|l| l.group != Some(g));
        folded += 1;
        fold_at = Some(start);
    }
    if let Some(at) = fold_at {
        let noun = format!("+{folded} idle ▾");
        body.insert(
            at,
            Line::new(
                format!("{DIM}   {}{RESET}", truncate(&noun, cols)),
                Hits::new(),
                None,
            ),
        );
    }

    let running = state.agents().any(|a| a.state == AgentState::Working);
    let mut head_lines = vec![
        (header(state, cols, folded > 0), Hits::new()),
        (rule(cols, tick, running), Hits::new()),
    ];
    head_lines.extend(head);
    let head = head_lines;

    let spare = rows.saturating_sub(head.len() + body.len());
    if spare >= 3 {
        // The `earlier` list: a title, entries and a blank line above the
        // footer, when at least one entry fits.
        let fits = spare
            .saturating_sub(3 + 2)
            .min(ledger.len())
            .min(LEDGER_MAX);
        let earlier = if fits > 0 { fits + 2 } else { 0 };
        body.extend((3 + earlier..spare).map(|_| Line::new(String::new(), Hits::new(), None)));
        if fits > 0 {
            let title = truncate("─ earlier ", cols);
            let fill = "─".repeat(cols.saturating_sub(width(&title)));
            body.push(Line::new(
                format!("{DIM}{title}{fill}{RESET}"),
                Hits::new(),
                None,
            ));
            for done in &ledger[..fits] {
                let (glyph, style) = if done.failed {
                    ("✗", RED)
                } else {
                    ("●", GREEN)
                };
                let (text, _) = Layout {
                    left: format!(" {style}{glyph}{RESET} {DIM}{} ", done.mark),
                    name: &done.label,
                    name_style: DIM,
                    right: format!("{DIM}{}{RESET}", fmt_elapsed(now.saturating_sub(done.at))),
                    buttons: vec![],
                }
                .draw(cols, None);
                body.push(Line::new(text, Hits::new(), None));
            }
            body.push(Line::new(String::new(), Hits::new(), None));
        }
        let rule = format!("{DIM}{}{RESET}", "─".repeat(cols));
        body.push(Line::new(rule, Hits::new(), None));
        body.push(Line::new(tally(state), Hits::new(), None));
        let (text, hits) = bar(cols);
        body.push(Line::new(text, hits, None));
    }

    let room = rows.saturating_sub(head.len());
    let sel_line = body.iter().position(|l| l.selected).unwrap_or(0);
    let start = (sel_line + 1).saturating_sub(room);
    let paint_line = |text: String, surface: Surface| {
        let text = clip(&text, cols);
        match density {
            Density::Cards => paint(&text, surface.color(&theme), cols),
            _ if matches!(surface, Surface::Selected | Surface::Flash) => {
                paint(&text, surface.color(&theme), cols)
            }
            _ => text,
        }
    };
    let head = head
        .into_iter()
        .map(|(text, hits)| (paint_line(text, Surface::Rail), hits));
    let body = body.into_iter().skip(start).take(room).map(|l| {
        let surface = l
            .surface
            .or(l.group.map(|g| groups[g].surface))
            .unwrap_or(Surface::Rail);
        (paint_line(l.text, surface), l.hits)
    });
    head.chain(body).take(rows).unzip()
}

/// `hits` moved `by` columns to the right.
fn shift(hits: Hits, by: usize) -> Hits {
    hits.into_iter()
        .map(|(r, t)| {
            let end = if r.end == usize::MAX {
                r.end
            } else {
                r.end + by
            };
            (r.start + by..end, t)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zj_herdr_core::{Machine, Pane, Tokens};

    fn agent(pid: u32, state: AgentState) -> Agent {
        Agent {
            id: format!("box/{pid}"),
            machine: "box".into(),
            kind: "claude".into(),
            pid,
            cwd: Some(format!("/w/proj{pid}")),
            state,
            since: 100,
            pane: Some(Pane::new("dev", pid)),
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

    /// `n` agents on `box`, the first blocked with 4.8M tokens, the rest
    /// working, and an unreachable `far`.
    fn state(n: u32) -> StateV1 {
        let agents = (1..=n)
            .map(|pid| {
                let mut a = agent(
                    pid,
                    if pid == 1 {
                        AgentState::Blocked
                    } else {
                        AgentState::Working
                    },
                );
                if pid == 1 {
                    a.tokens = Some(Tokens {
                        input: 4_800_000,
                        ..Tokens::default()
                    });
                }
                a
            })
            .collect();
        let remote = Machine {
            name: "far".into(),
            ssh: Some("far-host".into()),
            status: MachineStatus::Unreachable,
            error: Some("timeout".into()),
            agents: vec![],
        };
        let local = Machine {
            name: "box".into(),
            ssh: None,
            status: MachineStatus::Ok,
            error: None,
            agents,
        };
        StateV1::new(0, vec![local, remote])
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

    /// A compact frame of an instance in session `dev`.
    fn frame(st: &StateV1) -> Frame<'_> {
        Frame {
            state: Some(st),
            current: Some("dev"),
            now: 160,
            density: Density::Compact,
            ..Frame::default()
        }
    }

    /// `render` over the flat tree (no layouts), as plain text.
    fn draw(f: Frame, size: (usize, usize)) -> Vec<String> {
        let tree = view::tree(f.state.unwrap(), None, &[]);
        plain(&render(f, &tree, size).0)
    }

    fn code_tab(panes: Vec<u32>) -> Vec<view::SessionLayout> {
        vec![view::SessionLayout {
            name: "dev".into(),
            tabs: vec![view::TabLayout {
                position: 0,
                name: "code".into(),
                active: true,
                panes,
            }],
        }]
    }

    #[test]
    fn header_counts_agents_and_who_needs_you_over_a_sweeping_rule() {
        let st = state(2);
        let lines = draw(
            Frame {
                tick: 3,
                ..frame(&st)
            },
            (6, 30),
        );
        assert_eq!(lines[0], format!("zj-herdr{}·2 1!", " ".repeat(17)));
        assert_eq!(lines[1], format!("═══◆{}", "═".repeat(26)), "the sweep");
        assert_eq!(draw(frame(&st), (6, 12))[0], "zj-herdr", "too narrow");
        // Nothing runs: a still rule; nobody needs you: no alert.
        let mut st = state(2);
        st.machines[0].agents[0].state = AgentState::Done;
        st.machines[0].agents[1].state = AgentState::Idle;
        st.machines[0].agents[1].acked = true;
        let lines = draw(frame(&st), (6, 30));
        assert!(lines[0].ends_with(" ·2"), "{lines:?}");
        assert_eq!(lines[1], "═".repeat(30));
    }

    #[test]
    fn agent_rows_lead_with_the_project_and_put_details_below() {
        let mut st = state(2);
        let a = &mut st.machines[0].agents[0];
        a.model = Some("claude-opus-5-5".into());
        a.context = Some(80);
        let (lines, hits) = render(frame(&st), &view::tree(&st, None, &[]), (12, 40));
        let lines = plain(&lines);
        assert_eq!(lines[2], "box");
        assert_eq!(lines[3], "  dev");
        assert_eq!(lines[4], format!("    ◆ ✳ proj1{}1m", " ".repeat(25)));
        assert_eq!(lines[5], "      opus5.5 · 4.8M · ■■■■■ 80%! · #1");
        assert_eq!(lines[6], format!("    ⠋ ✳ proj2{}1m", " ".repeat(25)));
        assert!(
            lines[7].starts_with("far"),
            "a pane id alone gets no line: {lines:?}"
        );
        assert_eq!(hit(&hits[5], 0), Some(&Target::Agent("box/1".into())));
        // Narrow: details give way, tokens last.
        let lines = draw(frame(&st), (12, 16));
        assert_eq!(lines[5], "      4.8M · #1");
        assert_eq!(lines[4], "    ◆ ✳ proj1 1m");
    }

    #[test]
    fn context_bars_heat_up_and_warn() {
        let bar = |p| plain(&[context_bar(p)])[0].clone();
        assert_eq!(bar(0), "□□□□□ 0%".replace('□', "■"));
        assert_eq!(bar(62), "■■■■■ 62%");
        assert!(bar(76).ends_with("76%!"));
        assert!(bar(95).ends_with("95%⚠"));
        assert!(context_bar(95).contains(RED));
        assert!(context_bar(10).starts_with(&format!("{GREEN}■")));
    }

    #[test]
    fn models_are_shortened() {
        assert_eq!(short_model("claude-opus-5-5"), "opus5.5");
        assert_eq!(short_model("claude-sonnet-4-5-20250929"), "sonnet4.5");
        assert_eq!(short_model("claude-3-5-sonnet-20241022"), "3-5-sonnet");
        assert_eq!(short_model("gpt-5-codex"), "gpt-5-codex");
    }

    #[test]
    fn rows_end_in_their_buttons() {
        let mut st = state(3);
        st.machines[0].agents[1].state = AgentState::Idle;
        st.machines[0].agents[2].state = AgentState::Idle;
        st.machines[0].agents[2].pane = None;
        st.machines[0].agents[2].session_id = Some("s".into());
        let tree = view::tree(&st, None, &[]);
        let (lines, hits) = render(frame(&st), &tree, (20, 40));
        let lines = plain(&lines);
        let row = |needle: &str| lines.iter().position(|l| l.contains(needle)).unwrap();
        // Idle in a pane: `✓` marks it seen.
        let idle = row("proj2");
        assert!(lines[idle].ends_with("1m ✓"), "{lines:?}");
        assert_eq!(hit(&hits[idle], 39), Some(&Target::Ack("box/2".into())));
        assert_eq!(hit(&hits[idle], 0), Some(&Target::Agent("box/2".into())));
        // Idle outside Zellij: seen, or moved here.
        let outside = row("proj3");
        assert!(lines[outside].ends_with("1m ✓ ⇲"), "{lines:?}");
        assert_eq!(hit(&hits[outside], 37), Some(&Target::Ack("box/3".into())));
        assert_eq!(
            hit(&hits[outside], 39),
            Some(&Target::Adopt("box/3".into()))
        );
        assert_eq!(
            hit(&hits[outside], 38),
            Some(&Target::Agent("box/3".into()))
        );
        // Unreachable machine: `✕` hides it.
        let far = row("far");
        assert!(lines[far].starts_with("far (unreachable: timeout)") && lines[far].ends_with('✕'));
        assert_eq!(hit(&hits[far], 39), Some(&Target::Dismiss("far".into())));
        // A background session is never moved: a client shows it.
        st.machines[0].agents[2].background = true;
        let tree = view::tree(&st, None, &[]);
        let lines = plain(&render(frame(&st), &tree, (20, 40)).0);
        assert!(lines[row("proj3")].ends_with("1m ✓"), "{lines:?}");
        st.machines[0].agents[2].background = false;
        // Seen: no `✓`; not in a session: nowhere to move it.
        st.machines[0].agents[1].acked = true;
        let tree = view::tree(&st, None, &[]);
        let lines = plain(
            &render(
                Frame {
                    current: None,
                    ..frame(&st)
                },
                &tree,
                (20, 40),
            )
            .0,
        );
        assert!(lines[row("proj2")].ends_with("1m"), "{lines:?}");
        assert!(lines[row("proj3")].ends_with("1m ✓"), "{lines:?}");
    }

    #[test]
    fn state_glyphs_tell_errors_and_seen_agents_apart() {
        let mut st = state(3);
        for a in &mut st.machines[0].agents {
            a.state = AgentState::Idle;
        }
        st.machines[0].agents[0].error = true;
        st.machines[0].agents[1].acked = true;
        let lines = draw(frame(&st), (20, 40));
        let glyph = |p: &str| lines.iter().find(|l| l.contains(p)).unwrap().chars().nth(4);
        assert_eq!(glyph("proj1"), Some('✗'));
        assert_eq!(glyph("proj2"), Some('○'));
        assert_eq!(
            lines[0],
            format!("zj-herdr{}·3 2!", " ".repeat(27)),
            "seen is not counted"
        );
    }

    #[test]
    fn the_selected_row_is_marked_and_lit() {
        let st = state(2);
        let tree = view::tree(&st, None, &[]);
        let f = Frame {
            selected: Some("box/2"),
            ..frame(&st)
        };
        let (raw, _) = render(f, &tree, (12, 40));
        let lines = plain(&raw);
        assert!(lines[6].starts_with("►   ⠋ ✳ proj2"), "{lines:?}");
        let lit = theme::bg(Theme::default().selected);
        assert!(raw[6].starts_with(&lit));
        assert!(!raw[4].contains(&lit));
    }

    #[test]
    fn a_row_that_just_started_needing_you_flashes() {
        let st = state(1);
        let tree = view::tree(&st, None, &[]);
        let flash = theme::bg(Theme::default().flash);
        let (raw, _) = render(
            Frame {
                now: 101,
                ..frame(&st)
            },
            &tree,
            (8, 30),
        );
        assert!(raw[4].starts_with(&flash), "{raw:?}");
        let (raw, _) = render(
            Frame {
                now: 160,
                ..frame(&st)
            },
            &tree,
            (8, 30),
        );
        assert!(!raw[4].contains(&flash));
    }

    #[test]
    fn tabs_branch_their_agents_and_fold_the_rest() {
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
        let (lines, hits) = render(frame(&st), &tree, (20, 40));
        let lines = plain(&lines);
        assert_eq!(lines[4], "  ▌◆ 1 code");
        assert!(lines[5].starts_with("  ├ ◆ ✳ proj1"));
        assert_eq!(lines[6], "  │   4.8M · #1");
        assert!(lines[7].starts_with("  └ ⠋ ✳ proj2"));
        assert_eq!(lines[8], "   ⠋ 2 notes");
        assert_eq!(lines[10], "   +1 tab");
        assert_eq!(
            hit(&hits[4], 5),
            Some(&Target::Tab {
                session: "dev".into(),
                position: 0
            })
        );
        assert_eq!(hit(&hits[2], 0), None, "the current session is no target");
        assert_eq!(lines.len(), hits.len());
    }

    #[test]
    fn blocked_agents_say_what_they_wait_on() {
        let mut st = state(2);
        for a in &mut st.machines[0].agents {
            a.state = AgentState::Blocked;
            a.waiting = Some(format!("Bash: make {}", a.pid));
        }
        let layouts = code_tab(vec![1, 2]);
        let tree = view::tree(&st, Some("dev"), &layouts);
        let (lines, hits) = render(frame(&st), &tree, (20, 40));
        let lines = plain(&lines);
        assert_eq!(lines[7], "  │ ↳ Bash: make 1");
        assert_eq!(lines[9], "    ↳ Bash: make 2");
        assert_eq!(hit(&hits[7], 0), Some(&Target::Agent("box/1".into())));
        assert_eq!(selectable(&tree), ["box/1", "box/2"]);
    }

    #[test]
    fn background_tasks_hang_under_their_agent() {
        let mut st = state(1);
        let task = |label: &str, state| Task {
            kind: "shell".into(),
            label: label.into(),
            state,
            started: 40,
            ended: 0,
        };
        st.machines[0].agents[0].tasks = vec![
            task("make", TaskState::Completed),
            task("npm run dev", TaskState::Running),
            Task {
                kind: "agent".into(),
                ..task("Review diff", TaskState::Running)
            },
        ];
        let layouts = code_tab(vec![1]);
        let tree = view::tree(&st, Some("dev"), &layouts);
        let lines = plain(&render(frame(&st), &tree, (20, 40)).0);
        assert_eq!(lines[7], "      ┊ ▸ ❯ npm run dev");
        assert_eq!(
            lines[8],
            format!("      ┊ ⠋ ✳ Review diff{}2m", " ".repeat(15))
        );
        assert_eq!(lines[9], "      ┊ ● ⚙ make");
        // Past three, the rest fold, in red when one failed.
        st.machines[0].agents[0]
            .tasks
            .push(task("cargo test", TaskState::Failed));
        let layouts = code_tab(vec![1]);
        let tree = view::tree(&st, Some("dev"), &layouts);
        let (raw, _) = render(frame(&st), &tree, (20, 40));
        let lines = plain(&raw);
        assert_eq!(lines[7], "      ┊ ▸ ❯ npm run dev");
        assert_eq!(lines[8], "      ┊ ✗ ⚗ cargo test");
        assert_eq!(lines[9], "      ┊ +2 more");
        assert!(!raw[9].contains(RED), "no failure folded away");
    }

    #[test]
    fn commands_show_their_command_line() {
        let mut st = state(1);
        let mut build = agent(9, AgentState::Working);
        build.kind = "test".into();
        build.id = "cmd:dev:9".into();
        let mut server = agent(8, AgentState::Working);
        server.kind = "server".into();
        server.id = "cmd:dev:8".into();
        st.machines[0].agents.extend([build, server]);
        let mut shell = agent(7, AgentState::Unknown);
        shell.kind = PANE_KIND.into();
        shell.id = "pane:dev:7".into();
        st.machines[0].agents.push(shell);
        let labels = BTreeMap::from([
            ("cmd:dev:9".to_string(), "cargo test".to_string()),
            ("cmd:dev:8".to_string(), "npm run dev".to_string()),
            ("pane:dev:7".to_string(), "~/w".to_string()),
        ]);
        let layouts = code_tab(vec![1, 7, 8, 9]);
        let tree = view::tree(&st, Some("dev"), &layouts);
        let f = Frame {
            labels: Some(&labels),
            ..frame(&st)
        };
        let lines = plain(&render(f, &tree, (20, 40)).0);
        assert!(
            lines.contains(&"  ├ ▸ ❯ npm run dev".to_string()),
            "{lines:?}"
        );
        assert!(
            lines.contains(&format!("  └ ⠋ ⚗ cargo test{}1m", " ".repeat(20))),
            "{lines:?}"
        );
        assert!(
            lines[0].contains("·1 1!"),
            "commands and panes are not counted: {lines:?}"
        );
        assert!(lines.contains(&"  ├ · $ ~/w".to_string()), "{lines:?}");
    }

    #[test]
    fn spare_room_fills_with_the_earlier_list_and_a_footer() {
        let st = state(2);
        let tree = view::tree(&st, None, &[]);
        let (lines, hits) = render(frame(&st), &tree, (12, 30));
        let lines = plain(&lines);
        assert_eq!(lines.len(), 12);
        assert_eq!(lines[7], "far (unreachable: timeout)   ✕");
        assert_eq!(lines[9], "─".repeat(30));
        assert_eq!(lines[10], "1 working · 1 need you");
        assert_eq!(lines[11], "↵ jump  n next  p prev");
        let click = |col| hit(&hits[11], col).cloned();
        assert_eq!(click(0), Some(Target::Command(PluginCommand::Jump)));
        assert_eq!(click(7), None, "between buttons");
        assert_eq!(
            click(8),
            Some(Target::Command(PluginCommand::AttentionNext))
        );
        assert_eq!(
            click(21),
            Some(Target::Command(PluginCommand::AttentionPrev))
        );
        assert_eq!(draw(frame(&st), (10, 80)).len(), 8, "no room: no footer");
        // The earlier list sits above the footer, newest first.
        let ledger = [
            Finished {
                key: "cmd:3".into(),
                mark: "⚗".into(),
                label: "cargo test".into(),
                failed: true,
                at: 100,
            },
            Finished {
                key: "box/1".into(),
                mark: "✳".into(),
                label: "api".into(),
                failed: false,
                at: 40,
            },
        ];
        let f = Frame {
            ledger: &ledger,
            ..frame(&st)
        };
        let lines = draw(f, (20, 30));
        assert_eq!(lines[13], format!("─ earlier {}", "─".repeat(20)));
        assert_eq!(lines[14], format!(" ✗ ⚗ cargo test{}1m", " ".repeat(13)));
        assert_eq!(lines[15], format!(" ● ✳ api{}2m", " ".repeat(20)));
        assert_eq!(lines[16], "");
        assert_eq!(lines[19], "↵ jump  n next  p prev");
        // A narrow rail drops the buttons that do not fit.
        assert_eq!(draw(frame(&st), (12, 15))[11], "↵ jump  n next");
    }

    #[test]
    fn idle_tabs_fold_when_the_tree_overflows() {
        let mut st = state(1);
        st.machines.truncate(1);
        let mut layouts = code_tab(vec![1]);
        for pos in 1..6u32 {
            let mut a = agent(10 + pos, AgentState::Idle);
            a.acked = true;
            st.machines[0].agents.push(a);
            layouts[0].tabs.push(view::TabLayout {
                position: pos as usize,
                name: format!("t{pos}"),
                active: false,
                panes: vec![10 + pos],
            });
        }
        let tree = view::tree(&st, Some("dev"), &layouts);
        let lines = plain(&render(frame(&st), &tree, (12, 30)).0);
        assert!(lines[0].contains("·6▲"), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("+") && l.contains("idle ▾")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("proj1")),
            "the blocked tab stays"
        );
        assert!(lines.len() <= 12);
    }

    #[test]
    fn cards_paint_every_line_and_space_the_tabs() {
        let st = state(2);
        let layouts = code_tab(vec![1, 2]);
        let tree = view::tree(&st, Some("dev"), &layouts);
        let f = Frame {
            density: Density::Cards,
            ..frame(&st)
        };
        let (raw, _) = render(f, &tree, (20, 30));
        let theme = Theme::default();
        assert!(raw[0].starts_with(&theme::bg(theme.rail)));
        assert!(
            raw[4].starts_with(&theme::bg(theme.active)),
            "the tab in view"
        );
        assert!(raw.iter().all(|l| width(l) == 30), "padded to the width");
        let lines = plain(&raw);
        assert_eq!(lines[8], "", "a gap after the card");
        let f = Frame {
            density: Density::Comfortable,
            ..frame(&st)
        };
        let (raw, _) = render(f, &tree, (20, 30));
        assert!(!raw[4].contains("\x1b[48;2"), "comfortable is not tinted");
        assert_eq!(plain(&raw)[8], "");
    }

    #[test]
    fn scrolls_to_keep_selection_visible() {
        let st = state(30);
        let lines = draw(
            Frame {
                selected: Some("box/30"),
                ..frame(&st)
            },
            (6, 80),
        );
        assert_eq!(lines.len(), 6);
        assert!(lines.iter().any(|l| l.contains("proj30")), "{lines:?}");
    }

    #[test]
    fn every_line_fits_the_width() {
        let mut st = state(3);
        st.machines[0].agents[0].model = Some("claude-opus-5-5".into());
        st.machines[0].agents[0].context = Some(91);
        st.machines[0].agents[0].waiting = Some("Bash: a very long command line indeed".into());
        for cols in [8, 12, 20, 32] {
            let f = Frame {
                error: Some("a very long error message"),
                restore: 2,
                selected: Some("box/2"),
                ..frame(&st)
            };
            for line in draw(f, (30, cols)) {
                assert!(line.chars().count() <= cols, "{cols}: {line:?}");
            }
        }
    }

    #[test]
    fn restore_banner_and_errors_sit_under_the_rule() {
        let st = state(2);
        let tree = view::tree(&st, None, &[]);
        let f = Frame {
            restore: 3,
            error: Some("oops"),
            ..frame(&st)
        };
        let (lines, hits) = render(f, &tree, (8, 80));
        let lines = plain(&lines);
        assert_eq!(lines[2], "↻ restore 3 agents");
        assert_eq!(hit(&hits[2], 0), Some(&Target::Restore));
        assert_eq!(lines[3], "oops");
        assert_eq!(lines[4], "box");
    }

    #[test]
    fn small_helpers_cover_every_variant() {
        assert_eq!(mark("zzz"), "zzz", "an unknown kind stands for itself");
        assert_eq!(tab_color(AgentState::Working), GREEN);
        assert_eq!(tab_color(AgentState::Done), DIM);
        assert_eq!(tab_color(AgentState::Unknown), DIM);
        let theme = Theme::default();
        assert_eq!(Surface::Busy.color(&theme), theme.busy);
        let hits = vec![(2..5, Target::Restore), (0..usize::MAX, Target::Restore)];
        let shifted = shift(hits, 3);
        assert_eq!(shifted[0].0, 5..8);
        assert_eq!(shifted[1].0, 3..usize::MAX);
    }

    #[test]
    fn states_without_a_glyph_story_still_draw() {
        let mut st = state(2);
        st.machines.truncate(1);
        st.machines[0].agents[0].state = AgentState::Unknown;
        st.machines[0].agents[1].state = AgentState::Done;
        let lines = draw(frame(&st), (20, 40));
        let glyph = |p: &str| lines.iter().find(|l| l.contains(p)).unwrap().chars().nth(4);
        assert_eq!(glyph("proj1"), Some('?'));
        assert_eq!(glyph("proj2"), Some('✓'));
        assert_eq!(tally(&st), "0 working");
    }

    #[test]
    fn killed_tasks_are_dim_rings() {
        let mut st = state(1);
        st.machines[0].agents[0].tasks = vec![Task {
            kind: "shell".into(),
            label: "make".into(),
            state: TaskState::Killed,
            started: 40,
            ended: 0,
        }];
        let layouts = code_tab(vec![1]);
        let tree = view::tree(&st, Some("dev"), &layouts);
        let lines = plain(&render(frame(&st), &tree, (20, 40)).0);
        assert!(lines.iter().any(|l| l.contains("┊ ○ ⚙ make")), "{lines:?}");
    }

    #[test]
    fn working_tabs_are_busy_and_the_last_card_has_no_trailing_gap() {
        let mut st = state(1);
        st.machines.truncate(1);
        st.machines[0].agents[0].state = AgentState::Working;
        let layouts = code_tab(vec![1]);
        let tree = view::tree(&st, Some("other"), &layouts);
        let f = Frame {
            current: Some("other"),
            density: Density::Cards,
            ..frame(&st)
        };
        let (raw, _) = render(f, &tree, (20, 30));
        let theme = Theme::default();
        assert!(raw.iter().any(|l| l.starts_with(&theme::bg(theme.busy))));
        assert!(!plain(&raw[..raw.len().min(6)]).last().unwrap().is_empty());
    }

    #[test]
    fn an_empty_state_says_there_are_no_agents() {
        let st = StateV1::new(0, vec![]);
        let lines = draw(frame(&st), (10, 30));
        assert!(lines.iter().any(|l| l == "  no agents"), "{lines:?}");
    }

    #[test]
    fn waits_for_the_daemon() {
        let lines = plain(&render(Frame::default(), &[], (5, 30)).0);
        assert_eq!(lines, ["waiting for zj-herdr daemon…"]);
    }

    #[test]
    fn densities_parse() {
        assert_eq!(Density::parse("cards"), Some(Density::Cards));
        assert_eq!(Density::parse("compact"), Some(Density::Compact));
        assert_eq!(Density::parse("roomy"), None);
        assert_eq!(Density::default(), Density::Cards);
    }
}
