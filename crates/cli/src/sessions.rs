//! `sessions.json`: which agent session ran in which pane, so `zj-herdr
//! restore` can reopen each one with its resume command.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use zj_herdr_core::{Agent, AgentState};

use crate::agents;

pub const SESSIONS_SCHEMA: &str = "zj_herdr.sessions.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub session: String,
    pub pane_id: u32,
    pub kind: String,
    pub cwd: Option<String>,
    /// Claude transcript stem or Codex rollout id.
    pub session_id: String,
    /// Unix seconds when this pane last got a new agent session.
    pub updated_at: u64,
}

impl Record {
    fn key(&self) -> String {
        format!("{}:{}", self.session, self.pane_id)
    }

    fn same_session(&self, other: &Record) -> bool {
        (&self.kind, &self.cwd, &self.session_id) == (&other.kind, &other.cwd, &other.session_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Store {
    pub schema: String,
    /// Keyed by `<session>:<pane_id>`.
    pub panes: BTreeMap<String, Record>,
}

impl Default for Store {
    fn default() -> Self {
        Store {
            schema: SESSIONS_SCHEMA.into(),
            panes: BTreeMap::new(),
        }
    }
}

/// One pane to reopen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub record: Record,
    pub argv: Vec<String>,
}

impl Store {
    /// A missing file is an empty store.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Store::default()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let store: Store = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        if store.schema != SESSIONS_SCHEMA {
            return Err(format!("{}: unsupported schema {:?}", path.display(), store.schema));
        }
        Ok(store)
    }

    /// Write atomically so a crash never leaves half a file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(self).expect("store serializes"))?;
        fs::rename(&tmp, path)
    }

    /// Record every live local agent that has a pane and a session id.
    /// Returns whether anything changed. Records of exited agents are kept:
    /// they are exactly what a restore after a reboot needs.
    pub fn record(&mut self, agents: &[Agent], now: u64) -> bool {
        let mut changed = false;
        for a in agents.iter().filter(|a| a.state != AgentState::Done) {
            let (Some(pane), Some(session_id)) = (&a.pane, &a.session_id) else {
                continue;
            };
            if !agents::is_valid_session_id(session_id) {
                continue;
            }
            let rec = Record {
                session: pane.session.clone(),
                pane_id: pane.pane_id,
                kind: a.kind.clone(),
                cwd: a.cwd.clone(),
                session_id: session_id.clone(),
                updated_at: now,
            };
            if self.panes.get(&rec.key()).is_none_or(|old| !old.same_session(&rec)) {
                self.panes.insert(rec.key(), rec);
                changed = true;
            }
        }
        changed
    }

    /// What `restore` would reopen: records in `session` (or all), minus
    /// agent sessions already running and agents with no resume command.
    /// Returns the plans and a note for each skipped record.
    pub fn plan(&self, session: Option<&str>, live: &[Agent]) -> (Vec<Plan>, Vec<String>) {
        let mut plans = Vec::new();
        let mut skipped = Vec::new();
        for rec in self.panes.values().filter(|r| session.is_none_or(|s| r.session == s)) {
            let running = live.iter().any(|a| {
                a.state != AgentState::Done && a.kind == rec.kind && a.session_id.as_ref() == Some(&rec.session_id)
            });
            if running {
                skipped.push(format!(
                    "{}: {} {} is already running",
                    rec.key(),
                    rec.kind,
                    rec.session_id
                ));
                continue;
            }
            match agents::resume_command(&rec.kind, &rec.session_id) {
                Some(argv) => plans.push(Plan {
                    record: rec.clone(),
                    argv,
                }),
                None => skipped.push(format!("{}: {} has no resume command", rec.key(), rec.kind)),
            }
        }
        (plans, skipped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zj_herdr_core::Pane;

    fn agent(pid: u32, kind: &str, session: &str, pane: u32, sid: Option<&str>, state: AgentState) -> Agent {
        Agent {
            id: Agent::make_id("box", pid),
            machine: "box".into(),
            kind: kind.into(),
            pid,
            cwd: Some(format!("/w/{pid}")),
            state,
            since: 0,
            pane: Some(Pane {
                session: session.into(),
                pane_id: pane,
            }),
            session_id: sid.map(str::to_owned),
        }
    }

    #[test]
    fn records_only_resumable_facts_and_reports_changes() {
        let mut store = Store::default();
        let agents = vec![
            agent(1, "claude", "dev", 1, Some("c1"), AgentState::Idle),
            agent(2, "aider", "dev", 2, None, AgentState::Idle),
            agent(3, "codex", "ops", 0, Some("r1"), AgentState::Done),
        ];
        assert!(store.record(&agents, 10));
        assert_eq!(store.panes.keys().collect::<Vec<_>>(), ["dev:1"]);
        assert!(!store.record(&agents, 20), "unchanged sessions are not rewritten");
        assert_eq!(store.panes["dev:1"].updated_at, 10);

        let replaced = vec![agent(4, "codex", "dev", 1, Some("r2"), AgentState::Working)];
        assert!(store.record(&replaced, 30));
        assert_eq!(store.panes["dev:1"].kind, "codex");
    }

    #[test]
    fn save_and_load_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/zj-herdr/sessions.json");
        assert_eq!(Store::load(&path).unwrap(), Store::default());
        let mut store = Store::default();
        store.record(&[agent(1, "claude", "dev", 1, Some("c1"), AgentState::Idle)], 5);
        store.save(&path).unwrap();
        assert_eq!(Store::load(&path).unwrap(), store);
    }

    #[test]
    fn plans_resume_commands_skipping_live_and_unsupported() {
        let mut store = Store::default();
        store.record(
            &[
                agent(1, "claude", "dev", 1, Some("c1"), AgentState::Idle),
                agent(2, "codex", "dev", 2, Some("r1"), AgentState::Idle),
                agent(3, "claude", "ops", 1, Some("c2"), AgentState::Idle),
            ],
            0,
        );
        store.panes.insert(
            "dev:9".into(),
            Record {
                session: "dev".into(),
                pane_id: 9,
                kind: "gemini".into(),
                cwd: None,
                session_id: "g".into(),
                updated_at: 0,
            },
        );
        let live = vec![agent(7, "codex", "dev", 5, Some("r1"), AgentState::Working)];

        let (plans, skipped) = store.plan(Some("dev"), &live);
        let argv: Vec<String> = plans.iter().map(|p| p.argv.join(" ")).collect();
        assert_eq!(argv, ["claude --resume c1"]);
        assert_eq!(plans[0].record.cwd.as_deref(), Some("/w/1"));
        assert_eq!(skipped.len(), 2);
        assert!(skipped[0].contains("already running") && skipped[1].contains("no resume command"));

        let (all, _) = store.plan(None, &[]);
        assert_eq!(all.len(), 3);
    }
}
