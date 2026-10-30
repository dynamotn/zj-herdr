//! The rail's behaviour, apart from Zellij: events and pipe messages in,
//! what to draw and what to ask of Zellij out. Everything Zellij does for
//! it goes through [`Host`], so the whole of it runs in native tests.

use std::collections::{BTreeMap, BTreeSet};

use zellij_tile::prelude::*;
use zj_herdr_core::view::{SessionLayout, TabLayout};
use zj_herdr_core::{
    view, Agent, MachineStatus, PluginCommand, RestorePreview, StateV1, CMD_SCHEMA, PROMPT_ACK,
    PROMPT_NACK, STATE_SCHEMA,
};

use crate::activity::{self, Activity, SeenPane};
use crate::theme::Theme;
use crate::ui::{self, Density, Target};

/// What the rail asks of Zellij.
pub trait Host {
    /// Unix seconds.
    fn now(&self) -> u64;
    fn hide_self(&mut self);
    fn show_self(&mut self);
    fn run_command(&mut self, argv: &[&str], context: BTreeMap<String, String>);
    /// Type `text` into terminal pane `pane`.
    fn write_to_pane(&mut self, pane: u32, text: &str);
    /// Write to the CLI pipe `pipe`.
    fn pipe_output(&mut self, pipe: &str, text: &str);
    fn unblock_pipe(&mut self, pipe: &str);
    fn block_pipe(&mut self, pipe: &str);
    fn set_selectable(&mut self, selectable: bool);
    fn rename_tab(&mut self, tab_id: u64, name: &str);
    /// Focus tab `position`, counted from 1.
    fn switch_tab(&mut self, position: u32);
    fn switch_session(&mut self, name: &str);
    fn switch_session_with_focus(
        &mut self,
        name: &str,
        tab: Option<usize>,
        pane: Option<(u32, bool)>,
    );
    fn focus_pane(&mut self, pane: u32);
    fn set_timeout(&mut self, secs: f64);
}

/// One animation frame: spinners and the sweep step four times a second.
const TICK_SECS: f64 = 0.25;
/// Marks the `run_command` of `zj-herdr ack`.
const ACK_CONTEXT: &str = "zj_herdr_ack";
/// Marks the `run_command` that fetches the first snapshot.
const FETCH_CONTEXT: &str = "zj_herdr_fetch";
/// Marks the `run_command` of `zj-herdr adopt`.
const ADOPT_CONTEXT: &str = "zj_herdr_adopt";
/// Marks the `run_command` of `zj-herdr restore --dry-run --json`.
const RESTORE_CHECK_CONTEXT: &str = "zj_herdr_restore_check";
/// Marks the `run_command` of `zj-herdr restore`.
const RESTORE_CONTEXT: &str = "zj_herdr_restore";
/// `sh -c` script for `start_daemon`; `$1` is the zj-herdr binary.
const START_DAEMON: &str = r#"d="${XDG_STATE_HOME:-$HOME/.local/state}/zj-herdr"
mkdir -p "$d" && nohup "$1" daemon >>"$d/daemon.log" 2>&1 </dev/null &"#;

#[derive(Default)]
pub struct App<H> {
    pub host: H,
    /// The daemon's last snapshot.
    raw: Option<StateV1>,
    /// What the rail shows: `raw` without dismissed machines, with this
    /// session's commands.
    state: Option<StateV1>,
    /// Commands in this session's panes, and what finished lately.
    activity: Activity,
    /// What command and pane rows show, by row id.
    labels: BTreeMap<String, String>,
    /// Unreachable machines hidden with `✕` until they are back.
    dismissed: BTreeSet<String>,
    density: Density,
    /// Card colours, from the terminal's own.
    theme: Theme,
    /// Animation frames so far.
    tick: u64,
    /// Id of the agent under the cursor.
    selected: Option<String>,
    /// Id of the agent the last attention jump went to.
    last_jump: Option<String>,
    /// The Zellij session this instance runs in.
    session: Option<String>,
    /// Panes of this session, by tab position, from the last `PaneUpdate`.
    panes: Option<PaneManifest>,
    /// Tabs of this session, from the last `TabUpdate`.
    tabs: Vec<TabInfo>,
    /// Every session's tabs and panes, from the last `SessionUpdate`.
    sessions: Vec<SessionInfo>,
    /// Where a click on each rendered line lands.
    targets: Vec<ui::Hits>,
    /// The last value sent with `set_selectable`.
    selectable: Option<bool>,
    error: Option<String>,
    visible: bool,
    /// Pinned instances stay open after a jump.
    pinned: bool,
    /// Only one instance per session should type prompts.
    deliver_prompts: bool,
    /// Binary used to fetch the first snapshot.
    bin: String,
    /// Start `zj-herdr daemon` once permitted, unless one already runs.
    start_daemon: bool,
    /// Attention jump requested before any state arrived.
    pending: Option<PluginCommand>,
    /// Hide once `pending` has run: it came down a pipe, not from the user.
    hide_after_pending: bool,
    /// Zellij has granted the permissions asked for in `load`.
    permitted: bool,
    /// Zellij refused them; no later answer will come.
    denied: bool,
    /// Prompts that arrived before the permissions did, with the CLI pipe
    /// each one keeps blocked until it is answered.
    queued_prompts: Vec<(Option<String>, u32, String)>,
    /// A key was pressed since the picker was last shown: the user is
    /// using it, so commands from pipes leave it open.
    in_use: bool,
    /// This instance's plugin id, to find its own pane in `PaneUpdate`.
    plugin_id: u32,
    /// Name tabs of this session after their agent's directory.
    rename_tabs: bool,
    /// The name this instance last gave each tab, by tab id.
    given_names: BTreeMap<usize, String>,
    /// Agents `zj-herdr restore` would reopen in this session, from the
    /// last dry run.
    restorable: usize,
    /// A dry run was started since permissions and the session were known.
    restore_checked: bool,
}

impl<H: Host> App<H> {
    /// Read the plugin's configuration; `plugin_id` is this instance's.
    pub fn configure(&mut self, config: &BTreeMap<String, String>, plugin_id: u32) {
        self.pinned = config.get("pinned").is_some_and(|v| v == "true");
        // Off by default: every picker pane would otherwise type each prompt.
        self.deliver_prompts = config.get("deliver_prompts").is_some_and(|v| v == "true");
        self.bin = config
            .get("zj_herdr_bin")
            .cloned()
            .unwrap_or_else(|| "zj-herdr".into());
        self.start_daemon = config.get("start_daemon").is_some_and(|v| v == "true");
        self.rename_tabs = config.get("rename_tabs").is_none_or(|v| v != "false");
        self.density = config
            .get("density")
            .and_then(|d| Density::parse(d))
            .unwrap_or_default();
        self.plugin_id = plugin_id;
        self.host.set_timeout(TICK_SECS);
    }

    pub fn update(&mut self, event: Event) -> bool {
        match event {
            Event::ModeUpdate(info) => {
                self.session = info.session_name;
                if !self.restore_checked {
                    self.check_restore();
                }
                false
            }
            Event::CommandChanged(PaneId::Terminal(pane), argv, _, _) => {
                self.activity.command(pane, &argv, self.host.now());
                self.refresh_view();
                self.visible
            }
            Event::PaneUpdate(manifest) => {
                self.track_panes(&manifest);
                self.panes = Some(manifest);
                self.refresh_view();
                // Zellij sends no `PermissionRequestResult` for a grant it
                // has cached; this event needs ReadApplicationState, so it
                // proves the grant.
                if !self.permitted {
                    self.granted();
                }
                self.sync_selectable();
                self.name_tabs_after_agents();
                self.visible
            }
            Event::TabUpdate(tabs) => {
                self.tabs = tabs;
                self.name_tabs_after_agents();
                self.visible
            }
            Event::SessionUpdate(sessions, _) => {
                self.sessions = sessions;
                self.refresh_view();
                self.visible
            }
            Event::Mouse(Mouse::LeftClick(line, col)) => {
                let target = usize::try_from(line)
                    .ok()
                    .and_then(|l| ui::hit(self.targets.get(l)?, col).cloned());
                target.is_some_and(|t| self.on_click(t))
            }
            // A pinned rail never gets keys: the wheel moves its cursor, and
            // the view follows the cursor.
            Event::Mouse(Mouse::ScrollDown(_)) => self.move_cursor(1),
            Event::Mouse(Mouse::ScrollUp(_)) => self.move_cursor(-1),
            Event::Visible(visible) => {
                self.visible = visible;
                if visible {
                    self.check_restore();
                } else {
                    self.in_use = false;
                }
                visible
            }
            Event::Timer(_) => {
                self.host.set_timeout(TICK_SECS);
                self.tick += 1;
                // Commands become rows once they have run a while.
                self.refresh_view();
                self.visible
            }
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                if !self.permitted {
                    self.granted();
                }
                true
            }
            Event::PermissionRequestResult(PermissionStatus::Denied) => {
                self.denied = true;
                self.sync_selectable();
                for (pipe, _, _) in std::mem::take(&mut self.queued_prompts) {
                    if let Some(id) = pipe {
                        self.nack_denied(&id);
                    }
                }
                true
            }
            Event::RunCommandResult(_, stdout, _, context)
                if context.contains_key(FETCH_CONTEXT) =>
            {
                // A failed fetch is fine: the daemon's next push fills in.
                if let Ok(state) = StateV1::from_json(&String::from_utf8_lossy(&stdout)) {
                    if self.raw.is_none() {
                        self.set_state(state);
                    }
                }
                true
            }
            Event::RunCommandResult(code, _, stderr, context)
                if context.contains_key(ADOPT_CONTEXT) =>
            {
                if code != Some(0) {
                    self.error = Some(command_error(&stderr, "zj-herdr adopt failed"));
                }
                true
            }
            Event::RunCommandResult(code, _, stderr, context)
                if context.contains_key(ACK_CONTEXT) =>
            {
                if code != Some(0) {
                    self.error = Some(command_error(&stderr, "zj-herdr ack failed"));
                }
                true
            }
            Event::RunCommandResult(code, stdout, stderr, context) => {
                if let Some(session) = context.get(RESTORE_CHECK_CONTEXT) {
                    // An answer for a session this instance has since left
                    // says nothing about this one.
                    if Some(session) == self.session.as_ref() {
                        self.restorable = restore_count(&stdout);
                    }
                    return true;
                }
                if context.contains_key(RESTORE_CONTEXT) && code != Some(0) {
                    self.error = Some(command_error(&stderr, "zj-herdr restore failed"));
                    return true;
                }
                false
            }
            Event::Key(key) => self.on_key(key),
            _ => false,
        }
    }

    fn granted(&mut self) {
        self.permitted = true;
        self.sync_selectable();
        self.name_tabs_after_agents();
        if self.start_daemon {
            // Detached, so it outlives Zellij and is never a zombie of
            // its server; a second daemon exits at once (socket in use).
            self.host.run_command(
                &["sh", "-c", START_DAEMON, "sh", self.bin.as_str()],
                BTreeMap::new(),
            );
        }
        // Do not wait up to a heartbeat for the first push.
        let context = BTreeMap::from([(FETCH_CONTEXT.to_string(), String::new())]);
        self.host
            .run_command(&[self.bin.as_str(), "list", "--json"], context);
        self.check_restore();
        for (pipe, pane_id, text) in std::mem::take(&mut self.queued_prompts) {
            self.prompt(pipe, pane_id, text);
        }
    }

    pub fn pipe(&mut self, msg: PipeMessage) -> bool {
        let cli = match &msg.source {
            PipeSource::Cli(id) => Some(id.clone()),
            _ => None,
        };
        let payload = msg.payload.as_deref().unwrap_or_default();
        // `zellij pipe -- PAYLOAD` with stdin closed also delivers an empty
        // message once its input ends: nothing to parse, nothing wrong.
        if payload.trim().is_empty() {
            if let Some(id) = &cli {
                self.host.unblock_pipe(id);
            }
            return false;
        }
        let redraw = match msg.name.as_str() {
            STATE_SCHEMA => match StateV1::from_json(payload) {
                Ok(state) => {
                    self.set_state(state);
                    true
                }
                Err(e) => {
                    self.error = Some(e.to_string());
                    true
                }
            },
            CMD_SCHEMA => match PluginCommand::parse(payload) {
                // Answers (and unblocks) its CLI pipe itself, maybe later.
                Ok(PluginCommand::Prompt { pane_id, text }) => {
                    self.prompt(cli, pane_id, text);
                    return false;
                }
                // Every rail of the session gets these; the one in view acts.
                Ok(
                    PluginCommand::SelectNext
                    | PluginCommand::SelectPrev
                    | PluginCommand::Jump
                    | PluginCommand::SessionNext
                    | PluginCommand::SessionPrev,
                ) if !acts_on_rail_command(self.own_tab_active()) => false,
                Ok(cmd) => {
                    let redraw = self.on_command(cmd.clone());
                    if self.pending.is_some() {
                        self.hide_after_pending = true;
                    } else if !keeps_open(&cmd) {
                        self.hide_unless_in_use();
                    }
                    redraw
                }
                Err(e) => {
                    if let Some(id) = &cli {
                        self.host.pipe_output(id, &format!("{PROMPT_NACK} {e}\n"));
                    }
                    self.error = Some(e);
                    true
                }
            },
            _ => false,
        };
        if let Some(id) = &cli {
            self.host.unblock_pipe(id);
        }
        redraw
    }

    /// The rail for a pane of `rows` by `cols`.
    pub fn render(&mut self, rows: usize, cols: usize) -> String {
        let layouts = self.layouts();
        let tree = self.tree(&layouts);
        let now = self.host.now();
        let frame = ui::Frame {
            state: self.state.as_ref(),
            current: self.session.as_deref(),
            selected: self.selected.as_deref(),
            error: self.error.as_deref(),
            restore: self.restorable,
            now,
            tick: self.tick,
            theme: self.theme,
            density: self.density,
            ledger: &self.activity.ledger,
            labels: Some(&self.labels),
        };
        let (lines, targets) = ui::render(frame, &tree, (rows, cols));
        self.targets = targets;
        lines.join("\n")
    }

    fn nack_denied(&mut self, pipe: &str) {
        self.host.pipe_output(
            pipe,
            &format!("{PROMPT_NACK} the plugin was denied its permissions\n"),
        );
        self.host.unblock_pipe(pipe);
    }

    fn set_state(&mut self, state: StateV1) {
        if let Some(old) = &self.raw {
            self.activity.agents_changed(old, &state, self.host.now());
        }
        // A dismissed machine that is back (or gone) is shown again later.
        self.dismissed.retain(|name| {
            state
                .machines
                .iter()
                .any(|m| &m.name == name && m.status == MachineStatus::Unreachable)
        });
        self.raw = Some(state);
        self.refresh_view();
        self.error = None;
        self.name_tabs_after_agents();
        let ids = self.selectable_ids();
        if self.selected.as_ref().is_none_or(|s| !ids.contains(s)) {
            self.selected = ids.first().cloned();
        }
        if let Some(cmd) = self.pending.take() {
            self.on_command(cmd);
            if std::mem::take(&mut self.hide_after_pending) {
                self.hide_unless_in_use();
            }
        }
    }

    /// Zellij launches the plugin, floating, to deliver a pipe message when
    /// it is not loaded. Put it away again afterwards unless it is pinned or
    /// the user is using it. While permissions are pending it stays up:
    /// that is where Zellij asks for them.
    fn hide_unless_in_use(&mut self) {
        if !self.pinned && !self.in_use && self.permitted {
            self.host.hide_self();
        }
    }

    /// Type a prompt from the daemon and answer on its CLI pipe, if any.
    fn prompt(&mut self, pipe: Option<String>, pane_id: u32, text: String) {
        if self.denied {
            if let Some(id) = &pipe {
                self.nack_denied(id);
            }
            return;
        }
        if !self.permitted {
            // Keep the CLI waiting until the permissions are answered.
            if let Some(id) = &pipe {
                self.host.block_pipe(id);
            }
            self.queued_prompts.push((pipe, pane_id, text));
            return;
        }
        let reply = if !self.deliver_prompts {
            format!("{PROMPT_NACK} this instance has deliver_prompts \"false\"\n")
        } else if self.panes.is_some() && !self.has_terminal(pane_id) {
            // Typing into a pane that closed (say, while permissions were
            // pending) would report a delivery that never happened.
            format!("{PROMPT_NACK} pane {pane_id} is gone\n")
        } else {
            self.host.write_to_pane(pane_id, &text);
            format!("{PROMPT_ACK}\n")
        };
        if let Some(id) = pipe {
            self.host.pipe_output(&id, &reply);
            self.host.unblock_pipe(&id);
        }
        self.hide_unless_in_use();
    }

    /// Rebuild what the rail shows from the daemon's snapshot.
    fn refresh_view(&mut self) {
        let Some(mut state) = self.raw.clone() else {
            return;
        };
        state.machines.retain(|m| !self.dismissed.contains(&m.name));
        let now = self.host.now();
        // Every terminal pane gets a row: this session's from `PaneUpdate`,
        // the others' from `SessionUpdate`.
        let mut panes = Vec::new();
        if let (Some(session), Some(manifest)) = (&self.session, &self.panes) {
            panes.extend(seen_panes(session, manifest));
        }
        for s in &self.sessions {
            if !s.is_current_session && Some(&s.name) != self.session.as_ref() {
                panes.extend(seen_panes(&s.name, &s.panes));
            }
        }
        activity::attach_background(&mut state, &panes);
        let mut labels = BTreeMap::new();
        if let Some(session) = &self.session {
            labels = self.activity.labels(session, now);
            self.activity.add_to(&mut state, session, now);
        }
        activity::add_panes(&mut state, &panes, &mut labels);
        self.labels = labels;
        self.state = Some(state);
    }

    /// Follow this session's panes: the terminal's colours for the cards,
    /// the exit of a command pane, and panes that closed.
    fn track_panes(&mut self, manifest: &PaneManifest) {
        let terminals = || manifest.panes.values().flatten().filter(|p| !p.is_plugin);
        if let Some(p) = terminals().find(|p| p.default_bg.is_some() && p.default_fg.is_some()) {
            self.theme = Theme::from_hex(p.default_bg.as_deref(), p.default_fg.as_deref());
        }
        let now = self.host.now();
        let exited: Vec<(u32, bool)> = terminals()
            .filter(|p| p.exited)
            .map(|p| (p.id, p.exit_status.is_some_and(|c| c != 0)))
            .collect();
        for (id, failed) in exited {
            self.activity.finish(id, failed, now);
        }
        self.activity
            .keep_panes(|id| terminals().any(|p| p.id == id), now);
    }

    fn has_terminal(&self, pane_id: u32) -> bool {
        self.panes.as_ref().is_some_and(|m| {
            m.panes
                .values()
                .flatten()
                .any(|p| !p.is_plugin && !p.exited && p.id == pane_id)
        })
    }

    /// The focused terminal pane of the active tab.
    fn focused_terminal(&self) -> Option<u32> {
        let tab = self.tabs.iter().find(|t| t.active)?.position;
        self.panes
            .as_ref()?
            .panes
            .get(&tab)?
            .iter()
            .find(|p| p.is_focused && !p.is_plugin)
            .map(|p| p.id)
    }

    /// Where attention cycling continues from: the agent in the focused
    /// pane, which survives a session switch (each session has its own
    /// plugin instance), else this instance's last jump.
    fn cycle_from(&self) -> Option<String> {
        let focused = self.focused_terminal();
        let session = self.session.as_deref();
        self.state
            .as_ref()
            .and_then(|s| {
                s.agents().find(|a| {
                    a.pane.as_ref().is_some_and(|p| {
                        Some(p.session.as_str()) == session && Some(p.pane_id) == focused
                    })
                })
            })
            .map(|a| a.id.clone())
            .or_else(|| self.last_jump.clone())
    }

    fn find(&self, id: &str) -> Option<Agent> {
        self.state.as_ref()?.agents().find(|a| a.id == id).cloned()
    }

    /// Tabs and terminal panes of every local session this instance knows:
    /// its own from `TabUpdate`/`PaneUpdate`, the rest from `SessionUpdate`,
    /// and sessions that update left out from the daemon's `pane.tab`.
    fn layouts(&self) -> Vec<SessionLayout> {
        let mut out: Vec<SessionLayout> = self
            .sessions
            .iter()
            .filter(|s| !s.is_current_session && Some(&s.name) != self.session.as_ref())
            .map(|s| layout(&s.name, &s.tabs, Some(&s.panes)))
            .collect();
        if let Some(name) = &self.session {
            out.push(layout(name, &self.tabs, self.panes.as_ref()));
        }
        match &self.state {
            Some(state) => view::with_agent_tabs(state, out),
            None => out,
        }
    }

    fn tree<'a>(&'a self, layouts: &'a [SessionLayout]) -> Vec<view::Row<'a>> {
        self.state
            .as_ref()
            .map(|s| view::tree(s, self.session.as_deref(), layouts))
            .unwrap_or_default()
    }

    fn selectable_ids(&self) -> Vec<String> {
        ui::selectable(&self.tree(&self.layouts()))
    }

    /// A pinned rail must not be selectable once its permissions are
    /// answered: Zellij closes a tab (and the session with its last tab)
    /// only when no selectable pane is left, so a selectable rail would
    /// keep it open after its last terminal pane exits. It stays
    /// selectable until its tab has a terminal, so the tab is never
    /// closed while the layout is still opening, and never after: the last
    /// terminal's exit must leave nothing selectable for Zellij to close it.
    fn sync_selectable(&mut self) {
        if !self.pinned || self.selectable == Some(false) {
            return;
        }
        let tab_has_terminal = self.panes.as_ref().is_some_and(|m| {
            own_tab(m, self.plugin_id)
                .and_then(|tab| m.panes.get(&tab))
                .is_some_and(|ps| ps.iter().any(|p| !p.is_plugin))
        });
        let want = !((self.permitted || self.denied) && tab_has_terminal);
        if self.selectable != Some(want) {
            self.selectable = Some(want);
            self.host.set_selectable(want);
        }
    }

    /// Whether the tab holding this instance is the active one; `None`
    /// until Zellij has said where the instance is.
    fn own_tab_active(&self) -> Option<bool> {
        let tab = own_tab(self.panes.as_ref()?, self.plugin_id)?;
        Some(self.tabs.iter().any(|t| t.position == tab && t.active))
    }

    /// Name this session's tabs after their agents' directories. There is
    /// one rail per tab, so only the instance in the active tab does it.
    fn name_tabs_after_agents(&mut self) {
        if !self.rename_tabs || !self.permitted || self.own_tab_active() != Some(true) {
            return;
        }
        let (Some(state), Some(session)) = (&self.state, &self.session) else {
            return;
        };
        let layout = layout(session, &self.tabs, self.panes.as_ref());
        let given = given_by_position(&self.tabs, &self.given_names);
        for (position, name) in view::tab_renames(state, &layout, &given) {
            // The layout is built from `tabs`, so exactly one tab matches.
            for tab in self.tabs.iter().filter(|t| t.position == position) {
                self.host.rename_tab(tab.tab_id as u64, &name);
                self.given_names.insert(tab.tab_id, name.clone());
            }
        }
    }

    /// Ask `zj-herdr restore --dry-run --json` how many agents a restore
    /// would reopen here, for the banner. Needs the permissions and the
    /// session name.
    fn check_restore(&mut self) {
        let Some(session) = self.session.clone().filter(|_| self.permitted) else {
            return;
        };
        self.restore_checked = true;
        let context = BTreeMap::from([(RESTORE_CHECK_CONTEXT.to_string(), session.clone())]);
        let flag = format!("--session={session}");
        self.host.run_command(
            &[self.bin.as_str(), "restore", &flag, "--dry-run", "--json"],
            context,
        );
    }

    /// Reopen this session's recorded agents. `restore` skips agent
    /// sessions already running, so another rail's banner clicked after
    /// this one opens nothing twice.
    fn restore(&mut self) {
        let Some(session) = self.session.clone() else {
            return;
        };
        self.restorable = 0;
        let context = BTreeMap::from([(RESTORE_CONTEXT.to_string(), session.clone())]);
        let flag = format!("--session={session}");
        self.host
            .run_command(&[self.bin.as_str(), "restore", &flag], context);
    }

    /// Whether `agent` can be moved into this session: a local agent
    /// outside Zellij with a session to resume.
    fn adoptable(&self, agent: &Agent) -> bool {
        let local = self
            .state
            .as_ref()
            .and_then(|s| s.machines.iter().find(|m| m.name == agent.machine))
            .is_some_and(|m| m.ssh.is_none());
        ui::adoptable(agent, local, self.session.as_deref())
    }

    /// Select agent `id` and jump to its pane; with `adopt`, move an agent
    /// outside Zellij here instead.
    fn activate(&mut self, id: String, adopt: bool) {
        let Some(agent) = self.find(&id) else {
            return;
        };
        self.selected = Some(id.clone());
        // `adoptable` holds only with a session, so the filter keeps it.
        let Some(session) = self
            .session
            .clone()
            .filter(|_| adopt && self.adoptable(&agent))
        else {
            return self.jump(&agent);
        };
        let context = BTreeMap::from([(ADOPT_CONTEXT.to_string(), id.clone())]);
        // Ids and names come from pipes: never let one read as a flag.
        let session = format!("--session={session}");
        self.host
            .run_command(&[self.bin.as_str(), "adopt", &session, "--", &id], context);
    }

    fn on_click(&mut self, target: Target) -> bool {
        match target {
            // Only the `⇲` button adopts: a stray click on the row must not.
            Target::Agent(id) => self.activate(id, false),
            Target::Adopt(id) => self.activate(id, true),
            Target::Ack(id) => {
                let context = BTreeMap::from([(ACK_CONTEXT.to_string(), id.clone())]);
                self.host
                    .run_command(&[self.bin.as_str(), "ack", "--", &id], context);
            }
            Target::Dismiss(name) => {
                self.dismissed.insert(name);
                self.refresh_view();
            }
            Target::Command(cmd) => return self.on_command(cmd),
            Target::Tab { session, position } => {
                if !self.pinned {
                    self.host.hide_self();
                }
                if self.session.as_deref() == Some(session.as_str()) {
                    self.host.switch_tab(position as u32 + 1);
                } else {
                    self.host
                        .switch_session_with_focus(&session, Some(position), None);
                }
            }
            Target::Session(name) => self.host.switch_session(&name),
            Target::Restore => self.restore(),
        }
        true
    }

    fn move_cursor(&mut self, delta: isize) -> bool {
        if self.state.is_none() {
            return false;
        }
        let ids = self.selectable_ids();
        if ids.is_empty() {
            return false;
        }
        let pos = self
            .selected
            .as_ref()
            .and_then(|s| ids.iter().position(|i| i == s))
            .unwrap_or(0);
        let next = (pos as isize + delta).rem_euclid(ids.len() as isize) as usize;
        self.selected = Some(ids[next].clone());
        true
    }

    fn on_key(&mut self, key: KeyWithModifier) -> bool {
        self.in_use = true;
        match key.bare_key {
            BareKey::Down | BareKey::Char('j') => self.move_cursor(1),
            BareKey::Up | BareKey::Char('k') => self.move_cursor(-1),
            BareKey::Enter => self.on_command(PluginCommand::Jump),
            BareKey::Char('n') => self.on_command(PluginCommand::AttentionNext),
            BareKey::Char('p') => self.on_command(PluginCommand::AttentionPrev),
            BareKey::Esc | BareKey::Char('q') => {
                self.host.hide_self();
                false
            }
            _ => false,
        }
    }

    fn on_command(&mut self, cmd: PluginCommand) -> bool {
        match cmd {
            PluginCommand::AttentionNext | PluginCommand::AttentionPrev => {
                let from = self.cycle_from();
                let Some(state) = &self.state else {
                    self.pending = Some(cmd);
                    return false;
                };
                let forward = cmd == PluginCommand::AttentionNext;
                let order = view::attention_order(state);
                if let Some(agent) = view::cycle(&order, from.as_deref(), forward).cloned() {
                    self.selected = Some(agent.id.clone());
                    self.jump(&agent);
                }
                true
            }
            PluginCommand::SelectNext => self.move_cursor(1),
            PluginCommand::SelectPrev => self.move_cursor(-1),
            PluginCommand::Jump => {
                if let Some(id) = self.selected.clone() {
                    // Adopting stops the agent where it runs: only `⇲` does it.
                    self.activate(id, false);
                }
                true
            }
            PluginCommand::SessionNext | PluginCommand::SessionPrev => {
                let forward = cmd == PluginCommand::SessionNext;
                let names = self.sessions.iter().map(|s| s.name.as_str());
                if let Some(next) = next_session(names, self.session.as_deref(), forward) {
                    self.host.switch_session(&next);
                }
                false
            }
            PluginCommand::Toggle => {
                if self.visible {
                    self.host.hide_self();
                } else {
                    self.host.show_self();
                }
                false
            }
            PluginCommand::Prompt { pane_id, text } => {
                self.prompt(None, pane_id, text);
                false
            }
        }
    }

    /// Focus the agent's pane, switching session when needed. Remote agents
    /// and agents outside Zellij have nowhere to jump to.
    fn jump(&mut self, agent: &Agent) {
        let local = self
            .state
            .as_ref()
            .and_then(|s| s.machines.iter().find(|m| m.name == agent.machine))
            .is_some_and(|m| m.ssh.is_none());
        let Some(pane) = agent.pane.as_ref().filter(|_| local) else {
            return;
        };
        self.last_jump = Some(agent.id.clone());
        if !self.pinned {
            self.host.hide_self();
        }
        if self.session.as_deref() == Some(pane.session.as_str()) {
            self.host.focus_pane(pane.pane_id);
        } else {
            self.host
                .switch_session_with_focus(&pane.session, None, Some((pane.pane_id, false)));
        }
    }
}

/// The last line a failed command wrote, without control characters.
fn command_error(stderr: &[u8], fallback: &str) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let reason = stderr.lines().map(str::trim).rfind(|l| !l.is_empty());
    reason
        .unwrap_or(fallback)
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

/// Agents to reopen, from `zj-herdr restore --dry-run --json`; 0 when the
/// output is not that (an older zj-herdr, or the command failed).
fn restore_count(stdout: &[u8]) -> usize {
    RestorePreview::from_json(&String::from_utf8_lossy(stdout)).map_or(0, |p| p.count)
}

/// The live terminal panes of session `session`, hidden ones left out.
fn seen_panes(session: &str, manifest: &PaneManifest) -> Vec<SeenPane> {
    manifest
        .panes
        .values()
        .flatten()
        .filter(|p| !p.is_plugin && !p.exited && !p.is_suppressed)
        .map(|p| SeenPane {
            session: session.to_string(),
            id: p.id,
            title: p.title.clone(),
        })
        .collect()
}

/// The session after (or before) `current` by name, wrapping; `None`
/// when there is no other.
fn next_session<'a>(
    names: impl Iterator<Item = &'a str>,
    current: Option<&'a str>,
    forward: bool,
) -> Option<String> {
    let mut names: Vec<&str> = names.chain(current).collect();
    names.sort_unstable();
    names.dedup();
    let len = names.len();
    let at = current.and_then(|c| names.iter().position(|n| *n == c))?;
    let next = if forward {
        (at + 1) % len
    } else {
        (at + len - 1) % len
    };
    (next != at).then(|| names[next].to_string())
}

/// Whether a piped command leaves the picker up.
fn keeps_open(cmd: &PluginCommand) -> bool {
    matches!(
        cmd,
        PluginCommand::Toggle | PluginCommand::SelectNext | PluginCommand::SelectPrev
    )
}

/// `select-next`, `select-prev` and `jump` reach every rail of a session;
/// only the one in the active tab acts. An instance that does not know its
/// tab yet (one Zellij just launched to deliver the pipe) acts too.
fn acts_on_rail_command(own_tab_active: Option<bool>) -> bool {
    own_tab_active != Some(false)
}

/// Position of the tab that holds plugin pane `plugin_id`.
fn own_tab(manifest: &PaneManifest, plugin_id: u32) -> Option<usize> {
    manifest
        .panes
        .iter()
        .find(|(_, ps)| ps.iter().any(|p| p.is_plugin && p.id == plugin_id))
        .map(|(tab, _)| *tab)
}

/// Names given by tab id, re-keyed by the tabs' current positions.
fn given_by_position(tabs: &[TabInfo], given: &BTreeMap<usize, String>) -> BTreeMap<usize, String> {
    tabs.iter()
        .filter_map(|t| Some((t.position, given.get(&t.tab_id)?.clone())))
        .collect()
}

/// A [`SessionLayout`] from Zellij's tabs and panes, keyed by tab position.
fn layout(name: &str, tabs: &[TabInfo], panes: Option<&PaneManifest>) -> SessionLayout {
    let tabs = tabs
        .iter()
        .map(|t| TabLayout {
            position: t.position,
            name: t.name.clone(),
            active: t.active,
            panes: panes
                .and_then(|m| m.panes.get(&t.position))
                .map(|ps| ps.iter().filter(|p| !p.is_plugin).map(|p| p.id).collect())
                .unwrap_or_default(),
        })
        .collect();
    SessionLayout {
        name: name.to_string(),
        tabs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_cycling_goes_by_name_and_wraps() {
        let names = || ["web", "api", "dev"].into_iter();
        assert_eq!(
            next_session(names(), Some("dev"), true).as_deref(),
            Some("web")
        );
        assert_eq!(
            next_session(names(), Some("web"), true).as_deref(),
            Some("api")
        );
        assert_eq!(
            next_session(names(), Some("api"), false).as_deref(),
            Some("web")
        );
        assert_eq!(next_session(["dev"].into_iter(), Some("dev"), true), None);
        assert_eq!(next_session(names(), None, true), None);
    }

    #[test]
    fn only_cursor_and_toggle_commands_keep_the_picker_up() {
        assert!(!keeps_open(&PluginCommand::Jump));
        assert!(keeps_open(&PluginCommand::SelectNext));
        assert!(!keeps_open(&PluginCommand::AttentionNext));
    }

    fn pane(id: u32, is_plugin: bool) -> PaneInfo {
        PaneInfo {
            id,
            is_plugin,
            ..PaneInfo::default()
        }
    }

    fn tab(position: usize, tab_id: usize) -> TabInfo {
        TabInfo {
            position,
            tab_id,
            ..TabInfo::default()
        }
    }

    #[test]
    fn finds_its_own_tab_by_plugin_id() {
        let manifest = PaneManifest {
            panes: [
                (0, vec![pane(7, false), pane(1, true)]),
                (1, vec![pane(2, true), pane(7, true)]),
            ]
            .into(),
        };
        // Terminal 7 and plugin 7 are different panes.
        assert_eq!(own_tab(&manifest, 7), Some(1));
        assert_eq!(own_tab(&manifest, 1), Some(0));
        assert_eq!(own_tab(&manifest, 9), None);
    }

    #[test]
    fn reads_the_restore_count_and_command_errors() {
        assert_eq!(restore_count(br#"{"count":2,"panes":[]}"#), 2);
        assert_eq!(restore_count(b"would reopen dev:1 /w claude"), 0);
        assert_eq!(restore_count(b""), 0);
        assert_eq!(
            command_error(b"zj-herdr: skip x\nzj-herdr: \x1b[31mboom\n\n", "failed"),
            "zj-herdr: [31mboom"
        );
        assert_eq!(command_error(b"", "failed"), "failed");
    }

    #[test]
    fn only_the_rail_in_view_acts_on_rail_commands() {
        assert!(acts_on_rail_command(Some(true)));
        assert!(!acts_on_rail_command(Some(false)));
        assert!(acts_on_rail_command(None));
    }

    #[test]
    fn given_names_follow_their_tab_when_tabs_move() {
        let given = BTreeMap::from([(10, "api".to_string()), (11, "web".to_string())]);
        // Tab 10 was closed; tab 11 moved from position 1 to 0.
        let tabs = [tab(0, 11), tab(1, 12)];
        assert_eq!(
            given_by_position(&tabs, &given),
            BTreeMap::from([(0, "web".to_string())])
        );
    }

    #[test]
    fn layouts_list_terminal_panes_by_tab() {
        let manifest = PaneManifest {
            panes: [(0, vec![pane(3, false), pane(1, true)])].into(),
        };
        let l = layout("dev", &[tab(0, 5), tab(1, 6)], Some(&manifest));
        assert_eq!(l.tabs[0].panes, [3]);
        assert!(l.tabs[1].panes.is_empty());
    }
}

/// The whole rail driven through events and pipes, with Zellij faked.
#[cfg(test)]
mod app_tests {
    use super::*;
    use std::time::Duration;
    use zj_herdr_core::{AgentState, Machine, Pane};

    #[derive(Default)]
    struct Fake {
        calls: Vec<String>,
    }

    const NOW: u64 = 1_000;

    impl Host for Fake {
        fn now(&self) -> u64 {
            NOW
        }
        fn hide_self(&mut self) {
            self.calls.push("hide".into());
        }
        fn show_self(&mut self) {
            self.calls.push("show".into());
        }
        fn run_command(&mut self, argv: &[&str], context: BTreeMap<String, String>) {
            let keys: Vec<&String> = context.keys().collect();
            self.calls.push(format!("run {} {keys:?}", argv.join(" ")));
        }
        fn write_to_pane(&mut self, pane: u32, text: &str) {
            self.calls.push(format!("write {pane} {text}"));
        }
        fn pipe_output(&mut self, pipe: &str, text: &str) {
            self.calls.push(format!("out {pipe} {}", text.trim_end()));
        }
        fn unblock_pipe(&mut self, pipe: &str) {
            self.calls.push(format!("unblock {pipe}"));
        }
        fn block_pipe(&mut self, pipe: &str) {
            self.calls.push(format!("block {pipe}"));
        }
        fn set_selectable(&mut self, selectable: bool) {
            self.calls.push(format!("selectable {selectable}"));
        }
        fn rename_tab(&mut self, tab_id: u64, name: &str) {
            self.calls.push(format!("rename {tab_id} {name}"));
        }
        fn switch_tab(&mut self, position: u32) {
            self.calls.push(format!("tab {position}"));
        }
        fn switch_session(&mut self, name: &str) {
            self.calls.push(format!("session {name}"));
        }
        fn switch_session_with_focus(
            &mut self,
            name: &str,
            tab: Option<usize>,
            pane: Option<(u32, bool)>,
        ) {
            self.calls.push(format!("session {name} {tab:?} {pane:?}"));
        }
        fn focus_pane(&mut self, pane: u32) {
            self.calls.push(format!("focus {pane}"));
        }
        fn set_timeout(&mut self, secs: f64) {
            self.calls.push(format!("timeout {secs}"));
        }
    }

    fn app(config: &[(&str, &str)]) -> App<Fake> {
        let mut app = App::<Fake>::default();
        let config = config
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        app.configure(&config, 50);
        app.host.calls.clear();
        app
    }

    /// `app` with permissions granted, in session `dev`, calls cleared.
    fn ready(config: &[(&str, &str)]) -> App<Fake> {
        let mut app = app(config);
        app.update(Event::ModeUpdate(ModeInfo {
            session_name: Some("dev".into()),
            ..ModeInfo::default()
        }));
        app.update(Event::PermissionRequestResult(PermissionStatus::Granted));
        app.host.calls.clear();
        app
    }

    fn calls(app: &mut App<Fake>) -> Vec<String> {
        std::mem::take(&mut app.host.calls)
    }

    fn agent(pid: u32, state: AgentState, pane: Option<Pane>) -> Agent {
        Agent {
            id: format!("box/{pid}"),
            machine: "box".into(),
            kind: "claude".into(),
            pid,
            cwd: Some(format!("/w/p{pid}")),
            state,
            since: NOW - 30,
            pane,
            session_id: Some(format!("s{pid}")),
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

    fn state(agents: Vec<Agent>) -> StateV1 {
        let far = Machine {
            name: "far".into(),
            ssh: Some("far".into()),
            status: MachineStatus::Unreachable,
            error: Some("timeout".into()),
            agents: vec![agent(9, AgentState::Idle, Some(Pane::new("dev", 9)))],
        };
        StateV1::new(
            NOW,
            vec![
                Machine {
                    name: "box".into(),
                    ssh: None,
                    status: MachineStatus::Ok,
                    error: None,
                    agents,
                },
                far,
            ],
        )
    }

    fn pipe(name: &str, payload: &str, cli: Option<&str>) -> PipeMessage {
        PipeMessage {
            source: cli.map_or(PipeSource::Keybind, |c| PipeSource::Cli(c.into())),
            name: name.into(),
            payload: Some(payload.into()),
            args: BTreeMap::new(),
            is_private: false,
        }
    }

    fn send_state(app: &mut App<Fake>, st: &StateV1) {
        assert!(app.pipe(pipe(STATE_SCHEMA, &st.to_json(), None)));
    }

    fn pane_info(id: u32, is_plugin: bool) -> PaneInfo {
        PaneInfo {
            id,
            is_plugin,
            title: format!("t{id}"),
            ..PaneInfo::default()
        }
    }

    /// Tab 0 (active, id 10) holds this plugin (50) and terminals 1, 2;
    /// tab 1 (id 11) holds terminal 3.
    fn layout_events(app: &mut App<Fake>) {
        let mut focused = pane_info(1, false);
        focused.is_focused = true;
        focused.default_bg = Some("#101010".into());
        focused.default_fg = Some("#e0e0e0".into());
        app.update(Event::PaneUpdate(PaneManifest {
            panes: [
                (0, vec![pane_info(50, true), focused, pane_info(2, false)]),
                (1, vec![pane_info(3, false)]),
            ]
            .into(),
        }));
        app.update(Event::TabUpdate(vec![
            TabInfo {
                position: 0,
                tab_id: 10,
                name: "Tab #1".into(),
                active: true,
                ..TabInfo::default()
            },
            TabInfo {
                position: 1,
                tab_id: 11,
                name: "notes".into(),
                ..TabInfo::default()
            },
        ]));
    }

    /// `text` without its escapes.
    fn plain(text: &str) -> String {
        let mut out = String::new();
        let mut esc = false;
        for c in text.chars() {
            match (esc, c) {
                (false, '\x1b') => esc = true,
                (true, 'm') => esc = false,
                (false, c) => out.push(c),
                _ => {}
            }
        }
        out
    }

    fn click_line(app: &mut App<Fake>, needle: &str, col: usize) -> bool {
        let screen = plain(&app.render(40, 40));
        assert!(screen.contains(needle), "{needle:?} not in {screen}");
        let line = screen.lines().position(|l| l.contains(needle)).unwrap();
        app.update(Event::Mouse(Mouse::LeftClick(line as isize, col)))
    }

    #[test]
    fn configure_reads_options_and_starts_the_clock() {
        let mut app = App::<Fake>::default();
        let config = BTreeMap::from([
            ("pinned".to_string(), "true".to_string()),
            ("deliver_prompts".into(), "true".into()),
            ("zj_herdr_bin".into(), "/bin/zjh".into()),
            ("start_daemon".into(), "true".into()),
            ("rename_tabs".into(), "false".into()),
            ("density".into(), "compact".into()),
        ]);
        app.configure(&config, 7);
        assert!(app.pinned && app.deliver_prompts && app.start_daemon && !app.rename_tabs);
        assert_eq!((app.bin.as_str(), app.plugin_id), ("/bin/zjh", 7));
        assert_eq!(app.density, Density::Compact);
        assert_eq!(calls(&mut app), ["timeout 0.25"]);
        let app = self::app(&[]);
        assert_eq!(app.bin, "zj-herdr");
        assert!(app.rename_tabs && !app.pinned);
    }

    #[test]
    fn granted_permissions_start_the_daemon_fetch_and_flush_prompts() {
        let mut app = app(&[("start_daemon", "true"), ("deliver_prompts", "true")]);
        app.update(Event::ModeUpdate(ModeInfo {
            session_name: Some("dev".into()),
            ..ModeInfo::default()
        }));
        // Before the permissions: the prompt waits, its CLI blocked.
        let msg = pipe(
            CMD_SCHEMA,
            r#"{"cmd":"prompt","pane_id":1,"text":"hi"}"#,
            Some("p1"),
        );
        assert!(!app.pipe(msg));
        assert_eq!(calls(&mut app), ["block p1"]);
        assert!(app.update(Event::PermissionRequestResult(PermissionStatus::Granted)));
        let got = calls(&mut app);
        assert!(got[0].starts_with("run sh -c"), "{got:?}");
        assert!(got.contains(&r#"run zj-herdr list --json ["zj_herdr_fetch"]"#.to_string()));
        assert!(got
            .iter()
            .any(|c| c.starts_with("run zj-herdr restore --session=dev --dry-run --json")));
        assert!(got.contains(&"write 1 hi".to_string()), "{got:?}");
        assert!(got.contains(&"out p1 zj_herdr.prompt.delivered".to_string()));
        assert!(got.contains(&"unblock p1".to_string()));
    }

    #[test]
    fn denied_permissions_answer_every_waiting_prompt() {
        let mut app = app(&[("pinned", "true")]);
        let msg = pipe(
            CMD_SCHEMA,
            r#"{"cmd":"prompt","pane_id":1,"text":"hi"}"#,
            Some("p1"),
        );
        app.pipe(msg.clone());
        calls(&mut app);
        assert!(app.update(Event::PermissionRequestResult(PermissionStatus::Denied)));
        let got = calls(&mut app);
        assert!(
            got.iter().any(|c| c.contains("denied its permissions")),
            "{got:?}"
        );
        assert!(got.contains(&"unblock p1".to_string()));
        // Later prompts are refused at once.
        app.pipe(msg);
        assert!(calls(&mut app).iter().any(|c| c.contains("denied")));
    }

    #[test]
    fn prompts_are_typed_only_where_asked_and_into_live_panes() {
        let mut app = ready(&[]);
        let prompt = |pane: u32| {
            pipe(
                CMD_SCHEMA,
                &format!(r#"{{"cmd":"prompt","pane_id":{pane},"text":"go"}}"#),
                Some("p"),
            )
        };
        app.pipe(prompt(1));
        let got = calls(&mut app);
        assert!(got[0].contains(r#"deliver_prompts "false""#), "{got:?}");
        let mut app = ready(&[("deliver_prompts", "true")]);
        layout_events(&mut app);
        calls(&mut app);
        app.pipe(prompt(77));
        assert!(calls(&mut app)[0].contains("pane 77 is gone"));
        app.pipe(prompt(2));
        assert_eq!(
            calls(&mut app),
            [
                "write 2 go",
                "out p zj_herdr.prompt.delivered",
                "unblock p",
                "hide"
            ]
        );
        // From a keybind there is no CLI to answer.
        app.on_command(PluginCommand::Prompt {
            pane_id: 2,
            text: "x".into(),
        });
        assert_eq!(calls(&mut app), ["write 2 x", "hide"]);
    }

    #[test]
    fn pipes_carry_states_and_commands() {
        let mut app = ready(&[]);
        // An empty message (stdin closed) only unblocks.
        assert!(!app.pipe(pipe(STATE_SCHEMA, "  ", Some("c"))));
        assert_eq!(calls(&mut app), ["unblock c"]);
        assert!(app.pipe(pipe(STATE_SCHEMA, "not json", None)));
        assert!(app.error.as_deref().unwrap().contains("invalid JSON"));
        send_state(&mut app, &state(vec![agent(1, AgentState::Idle, None)]));
        assert_eq!(app.error, None);
        assert_eq!(app.selected.as_deref(), Some("box/1"));
        // A bad command is reported on its pipe.
        assert!(app.pipe(pipe(CMD_SCHEMA, "leap", Some("c"))));
        let got = calls(&mut app);
        assert!(
            got[0].starts_with("out c zj_herdr.prompt.failed:"),
            "{got:?}"
        );
        assert!(app.error.is_some());
        // Unknown pipes are ignored.
        assert!(!app.pipe(pipe("other", "x", None)));
        // Toggle shows a hidden picker and keeps it up.
        app.pipe(pipe(CMD_SCHEMA, "toggle", None));
        assert_eq!(calls(&mut app), ["show"]);
        app.update(Event::Visible(true));
        calls(&mut app);
        app.pipe(pipe(CMD_SCHEMA, "toggle", None));
        assert_eq!(calls(&mut app), ["hide"]);
    }

    #[test]
    fn rail_commands_reach_only_the_rail_in_view() {
        let mut app = ready(&[("pinned", "true")]);
        layout_events(&mut app);
        send_state(
            &mut app,
            &state(vec![
                agent(1, AgentState::Idle, Some(Pane::new("dev", 1))),
                agent(2, AgentState::Idle, Some(Pane::new("dev", 2))),
            ]),
        );
        assert!(app.pipe(pipe(CMD_SCHEMA, "select-next", None)));
        assert_eq!(app.selected.as_deref(), Some("box/2"));
        app.pipe(pipe(CMD_SCHEMA, "select-prev", None));
        assert_eq!(app.selected.as_deref(), Some("box/1"));
        calls(&mut app);
        app.pipe(pipe(CMD_SCHEMA, "jump", None));
        assert_eq!(calls(&mut app), ["focus 1"], "pinned: stays up");
        // In a tab not in view, the rail lets the active one act.
        let mut tabs = app.tabs.clone();
        tabs[0].active = false;
        tabs[1].active = true;
        app.update(Event::TabUpdate(tabs));
        assert!(!app.pipe(pipe(CMD_SCHEMA, "select-next", None)));
        assert_eq!(app.selected.as_deref(), Some("box/1"));
    }

    #[test]
    fn attention_jumps_wait_for_a_state_then_cycle() {
        let mut app = ready(&[]);
        // No state yet: remembered, run when one arrives, then hidden.
        assert!(!app.pipe(pipe(CMD_SCHEMA, "attention-next", None)));
        assert!(app.pending.is_some());
        layout_events(&mut app);
        calls(&mut app);
        send_state(
            &mut app,
            &state(vec![
                agent(1, AgentState::Blocked, Some(Pane::new("dev", 1))),
                agent(2, AgentState::Idle, Some(Pane::new("ops", 4))),
            ]),
        );
        let got = calls(&mut app);
        // Pane 1 is focused, so the cycle moves on to the next one.
        assert!(
            got.contains(&"session ops None Some((4, false))".to_string()),
            "{got:?}"
        );
        assert!(got.contains(&"hide".to_string()));
        assert_eq!(app.last_jump.as_deref(), Some("box/2"));
        // From the focused pane 1, back is the other one too.
        app.on_command(PluginCommand::AttentionPrev);
        assert!(calls(&mut app).contains(&"session ops None Some((4, false))".to_string()));
        // With pane 2 focused, which holds no agent, the last jump leads.
        app.last_jump = Some("box/1".into());
        app.panes = None;
        app.on_command(PluginCommand::AttentionNext);
        assert!(calls(&mut app).contains(&"session ops None Some((4, false))".to_string()));
    }

    #[test]
    fn clicks_do_what_their_target_says() {
        let mut app = ready(&[("density", "compact")]);
        layout_events(&mut app);
        let mut outside = agent(5, AgentState::Idle, None);
        outside.cwd = Some("/w/away".into());
        let mut working = agent(1, AgentState::Working, Some(Pane::new("dev", 1)));
        working.cwd = Some("/w/here".into());
        send_state(&mut app, &state(vec![working, outside]));
        app.restorable = 2;
        calls(&mut app);
        // A row jumps (the picker is not pinned, so it hides).
        assert!(click_line(&mut app, "here", 2));
        assert_eq!(calls(&mut app), ["hide", "focus 1"]);
        // `✓` marks seen, `⇲` adopts: both at the right edge.
        assert!(click_line(&mut app, "away", 37));
        assert_eq!(
            calls(&mut app),
            [r#"run zj-herdr ack -- box/5 ["zj_herdr_ack"]"#]
        );
        assert!(click_line(&mut app, "away", 39));
        assert_eq!(
            calls(&mut app),
            [r#"run zj-herdr adopt --session=dev -- box/5 ["zj_herdr_adopt"]"#]
        );
        // A row click on an outside agent only selects it.
        assert!(click_line(&mut app, "away", 2));
        assert!(calls(&mut app).is_empty());
        assert_eq!(app.selected.as_deref(), Some("box/5"));
        // The restore banner, a tab, the footer buttons, `✕`.
        assert!(click_line(&mut app, "restore 2", 0));
        assert_eq!(
            calls(&mut app),
            [r#"run zj-herdr restore --session=dev ["zj_herdr_restore"]"#]
        );
        assert_eq!(app.restorable, 0);
        assert!(click_line(&mut app, "1 Tab #1", 4));
        assert_eq!(calls(&mut app), ["hide", "tab 1"]);
        assert!(click_line(&mut app, "↵ jump", 0));
        assert!(click_line(&mut app, "far", 39));
        assert!(app.dismissed.contains("far"));
        assert!(!app.render(40, 40).contains("unreachable"));
        // A click on nothing does nothing.
        let screen = plain(&app.render(40, 40));
        let blank = screen.lines().position(|l| l.trim().is_empty()).unwrap();
        assert!(!app.update(Event::Mouse(Mouse::LeftClick(blank as isize, 0))));
        assert!(!app.update(Event::Mouse(Mouse::LeftClick(-1, 0))));
    }

    #[test]
    fn dismissed_machines_return_when_back_or_gone() {
        let mut app = ready(&[]);
        let st = state(vec![]);
        send_state(&mut app, &st);
        app.dismissed.insert("far".into());
        send_state(&mut app, &st);
        assert!(app.dismissed.contains("far"), "still unreachable");
        send_state(&mut app, &StateV1::new(NOW, vec![]));
        assert!(app.dismissed.is_empty(), "gone: shown again later");
    }

    #[test]
    fn restore_needs_a_session() {
        let mut app = app(&[]);
        app.restore();
        assert!(calls(&mut app).is_empty());
    }

    #[test]
    fn other_sessions_and_their_tabs_are_switched_to() {
        let mut app = ready(&[("pinned", "true")]);
        app.update(Event::SessionUpdate(
            vec![
                SessionInfo {
                    name: "ops".into(),
                    tabs: vec![TabInfo {
                        position: 0,
                        name: "logs".into(),
                        ..TabInfo::default()
                    }],
                    panes: PaneManifest {
                        panes: [(0, vec![pane_info(4, false)])].into(),
                    },
                    ..SessionInfo::default()
                },
                SessionInfo {
                    name: "dev".into(),
                    is_current_session: true,
                    ..SessionInfo::default()
                },
            ],
            vec![("old".into(), Duration::from_secs(1))],
        ));
        send_state(
            &mut app,
            &state(vec![agent(2, AgentState::Idle, Some(Pane::new("ops", 4)))]),
        );
        calls(&mut app);
        assert!(click_line(&mut app, "  ops", 2));
        assert_eq!(calls(&mut app), ["session ops"]);
        assert!(click_line(&mut app, "1 logs", 4));
        assert_eq!(calls(&mut app), ["session ops Some(0) None"]);
        app.pipe(pipe(CMD_SCHEMA, "session-next", None));
        assert_eq!(calls(&mut app), ["session ops"]);
        app.on_command(PluginCommand::SessionPrev);
        assert_eq!(calls(&mut app), ["session ops"]);
    }

    #[test]
    fn keys_move_jump_cycle_and_hide() {
        let mut app = ready(&[]);
        layout_events(&mut app);
        send_state(
            &mut app,
            &state(vec![
                agent(1, AgentState::Idle, Some(Pane::new("dev", 1))),
                agent(2, AgentState::Blocked, Some(Pane::new("dev", 2))),
            ]),
        );
        calls(&mut app);
        let key = |c| Event::Key(KeyWithModifier::new(c));
        assert!(app.update(key(BareKey::Char('j'))));
        assert!(app.update(key(BareKey::Up)));
        assert!(app.in_use);
        let before = app.selected.clone();
        assert!(app.update(key(BareKey::Down)));
        assert_ne!(app.selected, before);
        assert!(app.update(key(BareKey::Enter)));
        assert!(calls(&mut app).iter().any(|c| c.starts_with("focus")));
        app.update(key(BareKey::Char('n')));
        app.update(key(BareKey::Char('p')));
        calls(&mut app);
        assert!(!app.update(key(BareKey::Esc)));
        assert_eq!(calls(&mut app), ["hide"]);
        assert!(!app.update(key(BareKey::Char('z'))));
        // Hidden, the user is no longer using it.
        app.update(Event::Visible(false));
        assert!(!app.in_use);
    }

    #[test]
    fn mouse_wheel_moves_the_cursor() {
        let mut app = ready(&[]);
        layout_events(&mut app);
        send_state(
            &mut app,
            &state(vec![
                agent(1, AgentState::Idle, Some(Pane::new("dev", 1))),
                agent(2, AgentState::Blocked, Some(Pane::new("dev", 2))),
            ]),
        );
        let before = app.selected.clone();
        assert!(app.update(Event::Mouse(Mouse::ScrollDown(3))));
        assert_ne!(app.selected, before);
        assert!(app.update(Event::Mouse(Mouse::ScrollUp(3))));
        assert_eq!(app.selected, before);
    }

    #[test]
    fn command_results_report_failures_and_restore_counts() {
        let mut app = ready(&[]);
        let result = |code: i32, out: &str, err: &str, key: &str, value: &str| {
            Event::RunCommandResult(
                Some(code),
                out.as_bytes().to_vec(),
                err.as_bytes().to_vec(),
                BTreeMap::from([(key.to_string(), value.to_string())]),
            )
        };
        // The first snapshot, unless the daemon pushed one already.
        let st = state(vec![agent(1, AgentState::Idle, None)]);
        assert!(app.update(result(0, &st.to_json(), "", "zj_herdr_fetch", "")));
        assert!(app.raw.is_some());
        assert!(app.update(result(1, "garbage", "", "zj_herdr_fetch", "")));
        assert!(app.update(result(1, "", "zj-herdr: boom\n", "zj_herdr_adopt", "box/1")));
        assert_eq!(app.error.as_deref(), Some("zj-herdr: boom"));
        assert!(app.update(result(1, "", "", "zj_herdr_ack", "box/1")));
        assert_eq!(app.error.as_deref(), Some("zj-herdr ack failed"));
        assert!(app.update(result(
            0,
            r#"{"count":3,"panes":[]}"#,
            "",
            "zj_herdr_restore_check",
            "dev"
        )));
        assert_eq!(app.restorable, 3);
        assert!(app.update(result(
            0,
            r#"{"count":5,"panes":[]}"#,
            "",
            "zj_herdr_restore_check",
            "old"
        )));
        assert_eq!(app.restorable, 3, "an answer for another session");
        assert!(app.update(result(1, "", "nope", "zj_herdr_restore", "dev")));
        assert_eq!(app.error.as_deref(), Some("nope"));
        assert!(!app.update(result(0, "", "", "zj_herdr_restore", "dev")));
        assert!(!app.update(result(0, "", "", "other", "")));
        // Successes leave no error.
        app.error = None;
        assert!(app.update(result(0, "", "", "zj_herdr_adopt", "box/1")));
        assert!(app.update(result(0, "", "", "zj_herdr_ack", "box/1")));
        assert_eq!(app.error, None);
    }

    #[test]
    fn panes_tabs_commands_and_the_clock_update_the_view() {
        let mut app = ready(&[("pinned", "true")]);
        layout_events(&mut app);
        let got = calls(&mut app);
        assert!(got.contains(&"selectable false".to_string()), "{got:?}");
        assert_ne!(app.theme, Theme::default(), "the terminal's colours");
        send_state(
            &mut app,
            &state(vec![agent(1, AgentState::Idle, Some(Pane::new("dev", 1)))]),
        );
        // The active tab is named after its agent's directory, once.
        assert!(calls(&mut app).contains(&"rename 10 p1".to_string()));
        app.update(Event::TabUpdate(app.tabs.clone()));
        assert!(calls(&mut app).is_empty());
        // A command in pane 2 shows once it has run a while.
        app.update(Event::CommandChanged(
            PaneId::Terminal(2),
            vec!["cargo".into(), "test".into()],
            true,
            vec![],
        ));
        app.update(Event::Timer(0.25));
        assert_eq!(app.tick, 1);
        assert_eq!(calls(&mut app), ["timeout 0.25"]);
        // Every pane has a row: the agent's, the command's, a plain one.
        let screen = plain(&app.render(30, 40));
        assert!(screen.contains("· $ t3"), "{screen}");
        // Pane 2 exits with a failure: the command lands in `earlier`.
        let mut gone = pane_info(2, false);
        gone.exited = true;
        gone.exit_status = Some(1);
        app.activity.command(2, &["make".into()], NOW - 10);
        app.update(Event::PaneUpdate(PaneManifest {
            panes: [(0, vec![pane_info(50, true), pane_info(1, false), gone])].into(),
        }));
        assert!(app
            .activity
            .ledger
            .iter()
            .any(|f| f.label == "make" && f.failed));
        // Pane 2 is gone, tab 1 no longer listed: only the agent's row.
        let screen = plain(&app.render(30, 40));
        assert!(screen.contains("p1") && !screen.contains("t2"), "{screen}");
    }

    #[test]
    fn a_closed_pane_ends_its_command() {
        let mut app = ready(&[]);
        layout_events(&mut app);
        app.activity.command(1, &["make".into()], NOW - 10);
        app.activity.command(3, &["make".into()], NOW - 10);
        app.update(Event::PaneUpdate(PaneManifest {
            panes: [(0, vec![pane_info(50, true), pane_info(1, false)])].into(),
        }));
        // Only pane 3 closed, so only its command is listed as finished.
        assert_eq!(app.activity.ledger.len(), 1);
    }

    #[test]
    fn a_rail_with_no_terminal_yet_stays_selectable() {
        let mut app = app(&[("pinned", "true")]);
        app.update(Event::PermissionRequestResult(PermissionStatus::Granted));
        assert!(calls(&mut app).contains(&"selectable true".to_string()));
        layout_events(&mut app);
        assert!(calls(&mut app).contains(&"selectable false".to_string()));
        // The last terminal exits: still unselectable, so Zellij closes the tab.
        app.update(Event::PaneUpdate(PaneManifest {
            panes: [(0, vec![pane_info(50, true)])].into(),
        }));
        assert!(calls(&mut app).is_empty());
        // Not pinned: never touched.
        let mut app = ready(&[]);
        layout_events(&mut app);
        assert!(!calls(&mut app).concat().contains("selectable"));
    }

    #[test]
    fn a_cached_grant_shows_as_pane_updates_alone() {
        // Zellij sends no `PermissionRequestResult` when the grant is cached.
        let mut app = app(&[("pinned", "true")]);
        layout_events(&mut app);
        assert!(app.permitted);
        let got = calls(&mut app);
        assert!(got.contains(&"selectable false".to_string()), "{got:?}");
        assert!(got.iter().any(|c| c.contains("list --json")), "{got:?}");
        // A late result changes nothing.
        app.update(Event::PermissionRequestResult(PermissionStatus::Granted));
        assert!(calls(&mut app).is_empty());
    }

    #[test]
    fn remote_agents_and_agents_without_panes_are_not_jumped_to() {
        let mut app = ready(&[]);
        send_state(&mut app, &state(vec![agent(1, AgentState::Idle, None)]));
        calls(&mut app);
        app.activate("box/1".into(), false);
        app.activate("far/9".into(), false);
        app.activate("nobody".into(), false);
        assert!(calls(&mut app).is_empty());
        // No session known: nowhere to adopt into.
        app.session = None;
        app.activate("box/1".into(), true);
        assert!(calls(&mut app).is_empty());
        // Nothing to move over without a state.
        let mut empty = ready(&[]);
        assert!(!empty.move_cursor(1));
        send_state(&mut empty, &StateV1::new(NOW, vec![]));
        assert!(!empty.move_cursor(1));
    }

    #[test]
    fn renders_a_waiting_line_until_a_state_arrives() {
        let mut app = ready(&[]);
        assert!(app.render(5, 30).contains("waiting for zj-herdr daemon"));
        assert!(!app.update(Event::ModeUpdate(ModeInfo::default())));
        assert!(!app.update(Event::InputReceived));
    }
}
