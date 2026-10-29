//! What a rail sees happen on its own, apart from the daemon's agents: the
//! commands running in this session's panes (Zellij's `CommandChanged`),
//! and what finished lately, for the `earlier` list.
//!
//! Hook-free, like zj-radar's observed layer: a command typed at a shell
//! has no exit status to read, so its pane returning to the prompt counts
//! as done. Only a command pane (`zellij run`) reports one.

use std::collections::BTreeMap;

use zj_herdr_core::command::{self, Class};
use zj_herdr_core::view::basename;
use zj_herdr_core::{Agent, AgentState, Pane, StateV1};

use crate::ui::{self, Finished, LEDGER_MAX, PANE_KIND};

/// A command shows once it has run this long, so `ls` and `git status`
/// never flicker through the rail or the `earlier` list.
pub const PROMOTE_SECS: u64 = 2;

/// A turn shorter than this is not listed: without its transcript an
/// agent's state comes from CPU use alone and can flicker.
pub const MIN_TURN_SECS: u64 = 10;

/// Program names of the agents the daemon tracks: their panes are its.
const AGENTS: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "gemini",
    "aider",
    "copilot",
    "cursor-agent",
    "pi",
    "amp",
    "droid",
    "goose",
    "zj-herdr",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Running {
    line: String,
    class: Class,
    since: u64,
}

#[derive(Debug, Default)]
pub struct Activity {
    /// The command in each pane of this session, by pane id.
    running: BTreeMap<u32, Running>,
    /// Newest first, at most [`LEDGER_MAX`].
    pub ledger: Vec<Finished>,
}

impl Activity {
    /// List `done` first, in place of an older entry for the same thing.
    fn push(&mut self, done: Finished) {
        self.ledger.retain(|f| f.key != done.key);
        self.ledger.insert(0, done);
        self.ledger.truncate(LEDGER_MAX);
    }

    /// Pane `pane` now runs `argv` in the foreground (empty or a shell: it
    /// is back at its prompt).
    pub fn command(&mut self, pane: u32, argv: &[String], now: u64) {
        let line = argv.join(" ");
        if self.running.get(&pane).is_some_and(|r| r.line == line) {
            return;
        }
        // Whatever ran before is over: back at the prompt, or replaced.
        self.finish(pane, false, now);
        let at_prompt = argv.first().is_none_or(|p| command::is_shell(p));
        let agent = argv.iter().any(|w| AGENTS.contains(&basename(w)));
        let class = command::classify(&line);
        if at_prompt || agent || class == Class::Interactive {
            return;
        }
        self.running.insert(
            pane,
            Running {
                line,
                class,
                since: now,
            },
        );
    }

    /// The command of pane `pane` ended; listed under `earlier` if it ran
    /// long enough to be shown.
    pub fn finish(&mut self, pane: u32, failed: bool, now: u64) {
        let Some(run) = self.running.remove(&pane) else {
            return;
        };
        if now.saturating_sub(run.since) >= PROMOTE_SECS {
            self.push(Finished {
                key: format!("cmd:{pane}"),
                mark: run.class.mark().to_string(),
                label: run.line,
                failed,
                at: now,
            });
        }
    }

    /// Forget the commands of panes `live` no longer knows (closed panes).
    pub fn keep_panes(&mut self, live: impl Fn(u32) -> bool, now: u64) {
        let gone: Vec<u32> = self.running.keys().copied().filter(|p| !live(*p)).collect();
        for pane in gone {
            self.finish(pane, false, now);
        }
    }

    #[cfg(test)]
    fn is_running(&self, pane: u32) -> bool {
        self.running.contains_key(&pane)
    }

    /// List agents of `new` that ended a turn since `old`: working before,
    /// idle (or exited) now.
    pub fn agents_changed(&mut self, old: &StateV1, new: &StateV1, now: u64) {
        for agent in new.agents() {
            let worked = old.agents().any(|a| {
                a.id == agent.id
                    && a.state == AgentState::Working
                    && now.saturating_sub(a.since) >= MIN_TURN_SECS
            });
            if worked && matches!(agent.state, AgentState::Idle | AgentState::Done) {
                let label = agent.cwd.as_deref().map(basename).unwrap_or(&agent.kind);
                self.push(Finished {
                    key: agent.id.clone(),
                    mark: ui::mark(&agent.kind).to_string(),
                    label: label.to_string(),
                    failed: agent.error,
                    at: now,
                });
            }
        }
    }

    /// The command lines shown in session `session`, by row id.
    pub fn labels(&self, session: &str, now: u64) -> BTreeMap<String, String> {
        self.running
            .iter()
            .filter(|(_, r)| now.saturating_sub(r.since) >= PROMOTE_SECS)
            .map(|(p, r)| (format!("cmd:{session}:{p}"), r.line.clone()))
            .collect()
    }

    /// Add the commands shown to `state` as rows of the local machine, in
    /// session `session`: a row per pane that has no agent of its own.
    pub fn add_to(&self, state: &mut StateV1, session: &str, now: u64) {
        let has_agent = |state: &StateV1, pane: u32| {
            state.agents().any(|a| {
                a.pane
                    .as_ref()
                    .is_some_and(|p| p.session == session && p.pane_id == pane)
            })
        };
        let rows: Vec<Agent> = self
            .running
            .iter()
            .filter(|(pane, r)| {
                now.saturating_sub(r.since) >= PROMOTE_SECS && !has_agent(state, **pane)
            })
            .map(|(pane, r)| command_row(session, *pane, r))
            .collect();
        if let Some(local) = state.machines.iter_mut().find(|m| m.ssh.is_none()) {
            for mut row in rows {
                row.machine = local.name.clone();
                local.agents.push(row);
            }
        }
    }
}

/// A terminal pane of some session, as Zellij reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeenPane {
    pub session: String,
    pub id: u32,
    pub title: String,
}

/// The session name a Claude terminal title carries after its glyph
/// (`✳ name`, or a spinner frame while working); the title itself otherwise.
pub fn title_name(title: &str) -> &str {
    title
        .split_once(' ')
        .filter(|(glyph, _)| {
            glyph.chars().count() == 1 && !glyph.chars().all(char::is_alphanumeric)
        })
        .map_or(title, |(_, name)| name)
        .trim()
}

/// Put each background session of the local machine into the pane whose
/// title names it, in place of the client row there: the client is how
/// that pane shows the session, and holds none of its data.
pub fn attach_background(state: &mut StateV1, panes: &[SeenPane]) {
    let Some(local) = state.machines.iter_mut().find(|m| m.ssh.is_none()) else {
        return;
    };
    let homes: Vec<(usize, Pane)> = local
        .agents
        .iter()
        .enumerate()
        .filter(|(_, a)| a.background && a.pane.is_none())
        .filter_map(|(i, a)| {
            let name = a.title.as_deref()?;
            let pane = panes.iter().find(|p| title_name(&p.title) == name)?;
            Some((i, Pane::new(&pane.session, pane.id)))
        })
        .collect();
    for (i, pane) in &homes {
        local.agents[*i].pane = Some(pane.clone());
    }
    local
        .agents
        .retain(|a| a.background || !homes.iter().any(|(_, p)| a.pane.as_ref() == Some(p)));
}

/// Add a row for each pane in `panes` that has none yet (no agent, no
/// command), to the local machine of `state`, and its title to `labels`.
pub fn add_panes(state: &mut StateV1, panes: &[SeenPane], labels: &mut BTreeMap<String, String>) {
    let taken = |state: &StateV1, pane: &SeenPane| {
        state.agents().any(|a| {
            a.pane
                .as_ref()
                .is_some_and(|p| p.session == pane.session && p.pane_id == pane.id)
        })
    };
    let rows: Vec<Agent> = panes
        .iter()
        .filter(|p| !taken(state, p))
        .map(|p| {
            let id = format!("pane:{}:{}", p.session, p.id);
            labels.insert(id.clone(), p.title.clone());
            let run = Running {
                line: String::new(),
                class: Class::Other,
                since: 0,
            };
            Agent {
                id,
                kind: PANE_KIND.into(),
                state: AgentState::Unknown,
                ..command_row(&p.session, p.id, &run)
            }
        })
        .collect();
    if let Some(local) = state.machines.iter_mut().find(|m| m.ssh.is_none()) {
        for mut row in rows {
            row.machine = local.name.clone();
            local.agents.push(row);
        }
    }
}

/// A command as a row: its class's kind, running since it started.
fn command_row(session: &str, pane: u32, run: &Running) -> Agent {
    Agent {
        id: format!("cmd:{session}:{pane}"),
        machine: String::new(),
        kind: run.class.kind().into(),
        pid: 0,
        cwd: None,
        state: AgentState::Working,
        since: run.since,
        pane: Some(Pane::new(session, pane)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use zj_herdr_core::{Machine, MachineStatus};

    fn argv(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn commands_show_once_promoted_and_finish_into_the_ledger() {
        let mut a = Activity::default();
        a.command(3, &argv("cargo test"), 100);
        assert!(a.labels("dev", 101).is_empty(), "not promoted yet");
        assert_eq!(a.labels("dev", 102)["cmd:dev:3"], "cargo test");
        // The pane is back at its shell: done.
        a.command(3, &argv("-zsh"), 110);
        assert!(!a.is_running(3));
        assert_eq!(a.ledger[0].label, "cargo test");
        assert_eq!(a.ledger[0].mark, "⚗");
        assert!(!a.ledger[0].failed);
        // Too quick to have been shown: not listed either.
        a.command(3, &argv("ls -la"), 120);
        a.command(3, &argv("zsh"), 120);
        assert_eq!(a.ledger.len(), 1);
    }

    #[test]
    fn editors_agents_and_shells_are_not_commands() {
        let mut a = Activity::default();
        for line in [
            "nvim README.md",
            "claude --resume x",
            "node /opt/bin/codex",
            "fish",
            "",
        ] {
            a.command(1, &argv(line), 0);
            assert!(!a.is_running(1), "{line:?}");
        }
        // Opening an editor after a build ends the build.
        a.command(1, &argv("make"), 0);
        a.command(1, &argv("nvim x"), 5);
        assert_eq!(a.ledger[0].label, "make");
    }

    #[test]
    fn command_panes_report_failure_and_closed_panes_finish() {
        let mut a = Activity::default();
        a.command(1, &argv("npm run build"), 0);
        a.command(2, &argv("npm run dev"), 0);
        a.finish(1, true, 10);
        assert!(a.ledger[0].failed);
        a.keep_panes(|p| p != 2, 10);
        assert!(!a.is_running(2));
        assert_eq!(a.ledger.len(), 2);
        for i in 0..20 {
            a.command(10 + i, &argv(&format!("make t{i}")), 100);
            a.finish(10 + i, false, 200);
        }
        assert_eq!(a.ledger.len(), LEDGER_MAX);
        assert_eq!(a.ledger[0].label, "make t19");
        // A pane's next command replaces its last one.
        a.command(10, &argv("make again"), 300);
        a.finish(10, false, 400);
        assert_eq!(a.ledger.iter().filter(|f| f.key == "cmd:10").count(), 1);
    }

    fn agent(state: AgentState, pane: u32) -> Agent {
        Agent {
            id: format!("box/{pane}"),
            machine: "box".into(),
            kind: "claude".into(),
            pid: pane,
            cwd: Some("/w/api".into()),
            state,
            pane: Some(Pane::new("dev", pane)),
            ..command_row(
                "dev",
                pane,
                &Running {
                    line: String::new(),
                    class: Class::Other,
                    since: 0,
                },
            )
        }
    }

    fn state(agents: Vec<Agent>) -> StateV1 {
        StateV1::new(
            0,
            vec![Machine {
                name: "box".into(),
                ssh: None,
                status: MachineStatus::Ok,
                error: None,
                agents,
            }],
        )
    }

    #[test]
    fn agents_that_end_a_turn_go_to_the_ledger() {
        let mut a = Activity::default();
        let old = state(vec![
            agent(AgentState::Working, 1),
            agent(AgentState::Idle, 2),
        ]);
        let mut failed = agent(AgentState::Idle, 1);
        failed.error = true;
        let new = state(vec![failed, agent(AgentState::Working, 2)]);
        a.agents_changed(&old, &new, 50);
        assert_eq!(a.ledger.len(), 1);
        assert_eq!(
            (a.ledger[0].mark.as_str(), a.ledger[0].label.as_str()),
            ("✳", "api")
        );
        assert!(a.ledger[0].failed);
        // The same agent again replaces its entry; a flicker shorter than
        // a turn is not listed.
        a.agents_changed(&old, &new, 60);
        assert_eq!(a.ledger.len(), 1);
        assert_eq!(a.ledger[0].at, 60);
        let mut flicker = agent(AgentState::Working, 2);
        flicker.since = 55;
        a.agents_changed(
            &state(vec![flicker]),
            &state(vec![agent(AgentState::Idle, 2)]),
            60,
        );
        assert_eq!(a.ledger.len(), 1);
    }

    #[test]
    fn commands_join_the_local_machine_unless_an_agent_has_the_pane() {
        let mut a = Activity::default();
        a.command(1, &argv("cargo build"), 0);
        a.command(7, &argv("npm run dev"), 0);
        let mut st = state(vec![agent(AgentState::Idle, 1)]);
        a.add_to(&mut st, "dev", 10);
        let rows: Vec<_> = st
            .agents()
            .map(|r| (r.id.as_str(), r.kind.as_str(), r.machine.as_str()))
            .collect();
        assert_eq!(
            rows,
            [("box/1", "claude", "box"), ("cmd:dev:7", "server", "box")]
        );
    }

    #[test]
    fn bare_panes_get_rows_unless_something_runs_there() {
        let mut a = Activity::default();
        a.command(2, &argv("cargo build"), 0);
        let mut st = state(vec![agent(AgentState::Idle, 1)]);
        let mut labels = a.labels("dev", 10);
        a.add_to(&mut st, "dev", 10);
        let seen = |session: &str, id, title: &str| SeenPane {
            session: session.into(),
            id,
            title: title.into(),
        };
        let panes = [
            seen("dev", 1, "claude"),
            seen("dev", 2, "cargo"),
            seen("dev", 3, "~/w"),
            seen("ops", 1, "htop"),
        ];
        add_panes(&mut st, &panes, &mut labels);
        let rows: Vec<_> = st
            .agents()
            .map(|r| (r.id.as_str(), r.kind.as_str()))
            .collect();
        assert_eq!(
            rows,
            [
                ("box/1", "claude"),
                ("cmd:dev:2", "build"),
                ("pane:dev:3", PANE_KIND),
                ("pane:ops:1", PANE_KIND)
            ]
        );
        assert_eq!(labels["pane:ops:1"], "htop");
        assert_eq!(labels["cmd:dev:2"], "cargo build");
        assert!(!st.agents().any(|r| r.needs_you() && r.kind == PANE_KIND));
    }

    #[test]
    fn background_sessions_move_into_the_pane_their_title_names() {
        assert_eq!(
            title_name("✳ Thanh hành động zj-radarr"),
            "Thanh hành động zj-radarr"
        );
        assert_eq!(title_name("⠐ api work"), "api work");
        assert_eq!(title_name("~/w"), "~/w");
        assert_eq!(title_name("a b"), "a b", "a word is no glyph");
        let mut session = agent(AgentState::Working, 9);
        session.id = "box/90".into();
        session.pane = None;
        session.background = true;
        session.title = Some("api work".into());
        let mut lost = session.clone();
        lost.id = "box/91".into();
        lost.title = Some("nowhere".into());
        // The client in pane 3 shows `api work`.
        let client = agent(AgentState::Idle, 3);
        let other = agent(AgentState::Idle, 4);
        let mut st = state(vec![client, other, session, lost]);
        let panes = [SeenPane {
            session: "dev".into(),
            id: 3,
            title: "⠐ api work".into(),
        }];
        attach_background(&mut st, &panes);
        let rows: Vec<_> = st
            .agents()
            .map(|a| (a.id.as_str(), a.pane.as_ref().map(|p| p.pane_id)))
            .collect();
        assert_eq!(
            rows,
            [("box/4", Some(4)), ("box/90", Some(3)), ("box/91", None)]
        );
    }
}
