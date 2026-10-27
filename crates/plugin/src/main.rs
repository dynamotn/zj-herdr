//! zj-herdr Zellij plugin: one list of every agent across sessions and
//! machines, attention jumps, and prompt delivery for the daemon.
//!
//! State arrives on the `zj_herdr.state.v1` pipe (pushed by the daemon);
//! commands on `zj_herdr.cmd.v1` (keybinds and the daemon).

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use zellij_tile::prelude::*;
use zj_herdr_core::{view, Agent, PluginCommand, StateV1, CMD_SCHEMA, STATE_SCHEMA};

mod ui;

/// Elapsed times tick in whole seconds.
const TICK_SECS: f64 = 1.0;
/// Marks the `run_command` that fetches the first snapshot.
const FETCH_CONTEXT: &str = "zj_herdr_fetch";

#[derive(Default)]
struct Plugin {
    state: Option<StateV1>,
    /// Id of the agent under the cursor.
    selected: Option<String>,
    /// Id of the agent the last attention jump went to.
    last_jump: Option<String>,
    /// The Zellij session this instance runs in.
    session: Option<String>,
    error: Option<String>,
    visible: bool,
    /// Pinned instances stay open after a jump.
    pinned: bool,
    /// Only one instance per session should type prompts.
    deliver_prompts: bool,
    /// Binary used to fetch the first snapshot.
    bin: String,
    /// Attention jump requested before any state arrived.
    pending: Option<PluginCommand>,
}

register_plugin!(Plugin);

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl ZellijPlugin for Plugin {
    fn load(&mut self, config: BTreeMap<String, String>) {
        self.pinned = config.get("pinned").is_some_and(|v| v == "true");
        self.deliver_prompts = config.get("deliver_prompts").is_none_or(|v| v != "false");
        self.bin = config.get("zj_herdr_bin").cloned().unwrap_or_else(|| "zj-herdr".into());
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
        ]);
        set_timeout(TICK_SECS);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::ModeUpdate(info) => {
                self.session = info.session_name;
                false
            }
            Event::Visible(visible) => {
                self.visible = visible;
                visible
            }
            Event::Timer(_) => {
                set_timeout(TICK_SECS);
                self.visible
            }
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                // Do not wait up to a heartbeat for the first push.
                let context = BTreeMap::from([(FETCH_CONTEXT.to_string(), String::new())]);
                run_command(&[self.bin.as_str(), "list", "--json"], context);
                true
            }
            Event::RunCommandResult(_, stdout, _, context) if context.contains_key(FETCH_CONTEXT) => {
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
        if let PipeSource::Cli(id) = &msg.source {
            unblock_cli_pipe_input(id);
        }
        let payload = msg.payload.as_deref().unwrap_or_default();
        match msg.name.as_str() {
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
                Ok(cmd) => self.on_command(cmd),
                Err(e) => {
                    self.error = Some(e);
                    true
                }
            },
            _ => false,
        }
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
        }
    }

    fn find(&self, id: &str) -> Option<Agent> {
        self.state.as_ref()?.agents().find(|a| a.id == id).cloned()
    }

    fn move_cursor(&mut self, delta: isize) -> bool {
        let Some(state) = &self.state else { return false };
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
                let Some(state) = &self.state else {
                    self.pending = Some(cmd);
                    return false;
                };
                let forward = cmd == PluginCommand::AttentionNext;
                let order = view::attention_order(state);
                if let Some(agent) = view::cycle(&order, self.last_jump.as_deref(), forward).cloned() {
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
                if self.deliver_prompts {
                    write_chars_to_pane_id(&text, PaneId::Terminal(pane_id));
                }
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
