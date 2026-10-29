//! zj-herdr Zellij plugin: one list of every agent across sessions and
//! machines, attention jumps, and prompt delivery for the daemon.
//!
//! State arrives on the `zj_herdr.state.v1` pipe (pushed by the daemon);
//! commands on `zj_herdr.cmd.v1` (keybinds and the daemon). The behaviour
//! lives in [`app`]; this file only connects it to Zellij, and is covered
//! by the end-to-end test in a real Zellij (`tests/e2e_zellij.rs`).

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use zellij_tile::prelude::*;

mod activity;
mod app;
mod theme;
mod ui;

/// [`app::Host`] on Zellij's plugin API.
#[derive(Default)]
struct Zellij;

impl app::Host for Zellij {
    fn now(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }
    fn hide_self(&mut self) {
        hide_self();
    }
    fn show_self(&mut self) {
        show_self(true);
    }
    fn run_command(&mut self, argv: &[&str], context: BTreeMap<String, String>) {
        run_command(argv, context);
    }
    fn write_to_pane(&mut self, pane: u32, text: &str) {
        write_chars_to_pane_id(text, PaneId::Terminal(pane));
    }
    fn pipe_output(&mut self, pipe: &str, text: &str) {
        cli_pipe_output(pipe, text);
    }
    fn unblock_pipe(&mut self, pipe: &str) {
        unblock_cli_pipe_input(pipe);
    }
    fn block_pipe(&mut self, pipe: &str) {
        block_cli_pipe_input(pipe);
    }
    fn set_selectable(&mut self, selectable: bool) {
        set_selectable(selectable);
    }
    fn rename_tab(&mut self, tab_id: u64, name: &str) {
        rename_tab_with_id(tab_id, name);
    }
    fn switch_tab(&mut self, position: u32) {
        switch_tab_to(position);
    }
    fn switch_session(&mut self, name: &str) {
        switch_session(Some(name));
    }
    fn switch_session_with_focus(
        &mut self,
        name: &str,
        tab: Option<usize>,
        pane: Option<(u32, bool)>,
    ) {
        switch_session_with_focus(name, tab, pane);
    }
    fn focus_pane(&mut self, pane: u32) {
        focus_terminal_pane(pane, false, false);
    }
    fn set_timeout(&mut self, secs: f64) {
        set_timeout(secs);
    }
}

#[derive(Default)]
struct Plugin(app::App<Zellij>);

register_plugin!(Plugin);

impl ZellijPlugin for Plugin {
    fn load(&mut self, config: BTreeMap<String, String>) {
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
            EventType::CommandChanged,
        ]);
        self.0.configure(&config, get_plugin_ids().plugin_id);
    }

    fn update(&mut self, event: Event) -> bool {
        self.0.update(event)
    }

    fn pipe(&mut self, msg: PipeMessage) -> bool {
        self.0.pipe(msg)
    }

    fn render(&mut self, rows: usize, cols: usize) {
        print!("{}", self.0.render(rows, cols));
    }
}
