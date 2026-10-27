//! zj-herdr Zellij plugin: one list of every agent across sessions and
//! machines, attention jumps, and prompt delivery for the daemon.
//!
//! State arrives on the `zj_herdr.state.v1` pipe (pushed by the daemon);
//! commands on `zj_herdr.cmd.v1` (keybinds and the daemon).

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use zellij_tile::prelude::*;
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
                false
            }
            Event::TabUpdate(tabs) => {
                self.tabs = tabs;
                false
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
        let lines = ui::render(
            self.state.as_ref(),
            self.selected.as_deref(),
            self.error.as_deref(),
            now_secs(),
            rows,
            cols,
        );
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
        let ids = ui::selectable(&state);
        if self.selected.as_ref().is_none_or(|s| !ids.contains(s)) {
            self.selected = ids.first().cloned();
        }
        self.state = Some(state);
        self.error = None;
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

    fn move_cursor(&mut self, delta: isize) -> bool {
        let Some(state) = &self.state else {
            return false;
        };
        let ids = ui::selectable(state);
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
