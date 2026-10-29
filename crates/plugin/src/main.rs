//! zj-herdr Zellij plugin: one list of every agent across sessions and
//! machines, attention jumps, and prompt delivery for the daemon.
//!
//! State arrives on the `zj_herdr.state.v1` pipe (pushed by the daemon);
//! commands on `zj_herdr.cmd.v1` (keybinds and the daemon).

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use ui::Target;
use zellij_tile::prelude::*;
use zj_herdr_core::view::{SessionLayout, TabLayout};
use zj_herdr_core::{
    view, Agent, PluginCommand, StateV1, CMD_SCHEMA, PROMPT_ACK, PROMPT_NACK, STATE_SCHEMA,
};

mod ui;

/// Elapsed times tick in whole seconds.
const TICK_SECS: f64 = 1.0;
/// Marks the `run_command` that fetches the first snapshot.
const FETCH_CONTEXT: &str = "zj_herdr_fetch";
/// `sh -c` script for `start_daemon`; `$1` is the zj-herdr binary.
const START_DAEMON: &str = r#"d="${XDG_STATE_HOME:-$HOME/.local/state}/zj-herdr"
mkdir -p "$d" && nohup "$1" daemon >>"$d/daemon.log" 2>&1 </dev/null &"#;

#[derive(Default)]
struct Plugin {
    state: Option<StateV1>,
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
    /// What a click on each rendered line does.
    targets: Vec<Option<Target>>,
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
}

register_plugin!(Plugin);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl ZellijPlugin for Plugin {
    fn load(&mut self, config: BTreeMap<String, String>) {
        self.pinned = config.get("pinned").is_some_and(|v| v == "true");
        // Off by default: every picker pane would otherwise type each prompt.
        self.deliver_prompts = config.get("deliver_prompts").is_some_and(|v| v == "true");
        self.bin = config
            .get("zj_herdr_bin")
            .cloned()
            .unwrap_or_else(|| "zj-herdr".into());
        self.start_daemon = config.get("start_daemon").is_some_and(|v| v == "true");
        self.rename_tabs = config.get("rename_tabs").is_none_or(|v| v != "false");
        self.plugin_id = get_plugin_ids().plugin_id;
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::WriteToStdin,
            PermissionType::ReadCliPipes,
            PermissionType::RunCommands,
        ]);
        subscribe(&[
            EventType::Key,
            EventType::Timer,
            EventType::ModeUpdate,
            EventType::Visible,
            EventType::PermissionRequestResult,
            EventType::RunCommandResult,
            EventType::PaneUpdate,
            EventType::TabUpdate,
            EventType::SessionUpdate,
            EventType::Mouse,
        ]);
        set_timeout(TICK_SECS);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::ModeUpdate(info) => {
                self.session = info.session_name;
                false
            }
            Event::PaneUpdate(manifest) => {
                self.panes = Some(manifest);
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
                self.visible
            }
            Event::Mouse(Mouse::LeftClick(line, _)) => {
                let target = usize::try_from(line)
                    .ok()
                    .and_then(|l| self.targets.get(l).cloned().flatten());
                target.is_some_and(|t| self.on_click(t))
            }
            Event::Visible(visible) => {
                self.visible = visible;
                if !visible {
                    self.in_use = false;
                }
                visible
            }
            Event::Timer(_) => {
                set_timeout(TICK_SECS);
                self.visible
            }
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                self.permitted = true;
                self.sync_selectable();
                self.name_tabs_after_agents();
                if self.start_daemon {
                    // Detached, so it outlives Zellij and is never a zombie of
                    // its server; a second daemon exits at once (socket in use).
                    run_command(
                        &["sh", "-c", START_DAEMON, "sh", self.bin.as_str()],
                        BTreeMap::new(),
                    );
                }
                // Do not wait up to a heartbeat for the first push.
                let context = BTreeMap::from([(FETCH_CONTEXT.to_string(), String::new())]);
                run_command(&[self.bin.as_str(), "list", "--json"], context);
                for (pipe, pane_id, text) in std::mem::take(&mut self.queued_prompts) {
                    self.prompt(pipe, pane_id, text);
                }
                true
            }
            Event::PermissionRequestResult(PermissionStatus::Denied) => {
                self.denied = true;
                self.sync_selectable();
                for (pipe, _, _) in std::mem::take(&mut self.queued_prompts) {
                    if let Some(id) = pipe {
                        nack_denied(&id);
                    }
                }
                true
            }
            Event::RunCommandResult(_, stdout, _, context)
                if context.contains_key(FETCH_CONTEXT) =>
            {
                // A failed fetch is fine: the daemon's next push fills in.
                if let Ok(state) = StateV1::from_json(&String::from_utf8_lossy(&stdout)) {
                    if self.state.is_none() {
                        self.set_state(state);
                    }
                }
                true
            }
            Event::Key(key) => self.on_key(key),
            _ => false,
        }
    }

    fn pipe(&mut self, msg: PipeMessage) -> bool {
        let cli = match &msg.source {
            PipeSource::Cli(id) => Some(id.clone()),
            _ => None,
        };
        let payload = msg.payload.as_deref().unwrap_or_default();
        // `zellij pipe -- PAYLOAD` with stdin closed also delivers an empty
        // message once its input ends: nothing to parse, nothing wrong.
        if payload.trim().is_empty() {
            if let Some(id) = &cli {
                unblock_cli_pipe_input(id);
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
                Ok(cmd) => {
                    let redraw = self.on_command(cmd.clone());
                    if self.pending.is_some() {
                        self.hide_after_pending = true;
                    } else if cmd != PluginCommand::Toggle {
                        self.hide_unless_in_use();
                    }
                    redraw
                }
                Err(e) => {
                    if let Some(id) = &cli {
                        cli_pipe_output(id, &format!("{PROMPT_NACK} {e}\n"));
                    }
                    self.error = Some(e);
                    true
                }
            },
            _ => false,
        };
        if let Some(id) = &cli {
            unblock_cli_pipe_input(id);
        }
        redraw
    }

    fn render(&mut self, rows: usize, cols: usize) {
        let layouts = self.layouts();
        let tree = self.tree(&layouts);
        let (lines, targets) = ui::render(
            self.state.as_ref(),
            &tree,
            self.session.as_deref(),
            self.selected.as_deref(),
            self.error.as_deref(),
            now_secs(),
            (rows, cols),
        );
        self.targets = targets;
        print!("{}", lines.join("\n"));
    }
}

fn nack_denied(pipe: &str) {
    cli_pipe_output(
        pipe,
        &format!("{PROMPT_NACK} the plugin was denied its permissions\n"),
    );
    unblock_cli_pipe_input(pipe);
}

impl Plugin {
    fn set_state(&mut self, state: StateV1) {
        self.state = Some(state);
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
    fn hide_unless_in_use(&self) {
        if !self.pinned && !self.in_use && self.permitted {
            hide_self();
        }
    }

    /// Type a prompt from the daemon and answer on its CLI pipe, if any.
    fn prompt(&mut self, pipe: Option<String>, pane_id: u32, text: String) {
        if self.denied {
            if let Some(id) = &pipe {
                nack_denied(id);
            }
            return;
        }
        if !self.permitted {
            // Keep the CLI waiting until the permissions are answered.
            if let Some(id) = &pipe {
                block_cli_pipe_input(id);
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
            write_chars_to_pane_id(&text, PaneId::Terminal(pane_id));
            format!("{PROMPT_ACK}\n")
        };
        if let Some(id) = pipe {
            cli_pipe_output(&id, &reply);
            unblock_cli_pipe_input(&id);
        }
        self.hide_unless_in_use();
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
    /// closed while the layout is still opening.
    fn sync_selectable(&mut self) {
        if !self.pinned {
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
            set_selectable(want);
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
            let Some(tab) = self.tabs.iter().find(|t| t.position == position) else {
                continue;
            };
            rename_tab_with_id(tab.tab_id as u64, &name);
            self.given_names.insert(tab.tab_id, name);
        }
    }

    fn on_click(&mut self, target: Target) -> bool {
        match target {
            Target::Agent(id) => {
                if let Some(agent) = self.find(&id) {
                    self.selected = Some(id);
                    self.jump(&agent);
                }
            }
            Target::Tab { session, position } => {
                if !self.pinned {
                    hide_self();
                }
                if self.session.as_deref() == Some(session.as_str()) {
                    switch_tab_to(position as u32 + 1);
                } else {
                    switch_session_with_focus(&session, Some(position), None);
                }
            }
            Target::Session(name) => switch_session(Some(&name)),
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
            BareKey::Enter => {
                if let Some(agent) = self.selected.clone().and_then(|id| self.find(&id)) {
                    self.jump(&agent);
                }
                true
            }
            BareKey::Char('n') => self.on_command(PluginCommand::AttentionNext),
            BareKey::Char('p') => self.on_command(PluginCommand::AttentionPrev),
            BareKey::Esc | BareKey::Char('q') => {
                hide_self();
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
            PluginCommand::Toggle => {
                if self.visible {
                    hide_self();
                } else {
                    show_self(true);
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
            hide_self();
        }
        if self.session.as_deref() == Some(pane.session.as_str()) {
            focus_terminal_pane(pane.pane_id, false, false);
        } else {
            switch_session_with_focus(&pane.session, None, Some((pane.pane_id, false)));
        }
    }
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
