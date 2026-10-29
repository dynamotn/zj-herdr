//! `zj-herdr daemon`: rescan, serve the socket, push state to Zellij.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use zj_herdr_core::{Agent, PaneTab, STATE_SCHEMA};

use crate::config::Remote;
use crate::detect::Roots;
use crate::registry::Registry;
use crate::remote;
use crate::scan::{now_secs, Scanner};
use crate::server::{self, Context};
use crate::sessions::Store;
use crate::zellij::{Zellij, ZellijCli};

/// The plan's cadence: fast enough to feel live, cheap enough to leave on.
pub const SCAN_INTERVAL: Duration = Duration::from_secs(1);
/// How often the pusher looks for changes to deliver.
const PUSH_POLL: Duration = Duration::from_millis(250);
/// Re-push unchanged state this often so a plugin loaded after the last
/// change, or a new session, fills in within a few seconds.
const PUSH_HEARTBEAT: Duration = Duration::from_secs(5);
/// Ask a session for its panes' tabs at most this often.
pub const TAB_REFRESH_SECS: u64 = 5;

/// The tab of every agent's pane, so the plugin can group agents of
/// sessions Zellij did not describe to it. One `list-panes` per session
/// that has agents, at most every [`TAB_REFRESH_SECS`].
#[derive(Default)]
pub struct TabCache {
    /// Per session: when it was asked, and its answer (`None` on failure).
    sessions: HashMap<String, (u64, Option<HashMap<u32, PaneTab>>)>,
}

impl TabCache {
    /// Set `pane.tab` on every agent in a Zellij pane; `None` where the
    /// session did not answer or does not list the pane.
    pub fn fill(&mut self, agents: &mut [Agent], zellij: &dyn Zellij, now: u64) {
        let wanted: BTreeSet<String> = agents
            .iter()
            .filter_map(|a| a.pane.as_ref().map(|p| p.session.clone()))
            .collect();
        self.sessions.retain(|s, _| wanted.contains(s));
        for session in wanted {
            let fresh = self
                .sessions
                .get(&session)
                .is_some_and(|(at, _)| now.saturating_sub(*at) < TAB_REFRESH_SECS);
            if !fresh {
                let tabs = zellij.panes(&session).ok();
                self.sessions.insert(session, (now, tabs));
            }
        }
        for pane in agents.iter_mut().filter_map(|a| a.pane.as_mut()) {
            pane.tab = self
                .sessions
                .get(&pane.session)
                .and_then(|(_, tabs)| tabs.as_ref()?.get(&pane.pane_id).cloned());
        }
    }
}

pub struct Options {
    pub socket: PathBuf,
    pub machine: String,
    pub roots: Roots,
    pub remotes: Vec<Remote>,
    /// Zellij plugin URL or alias that types prompts.
    pub plugin: String,
    /// Where pane sessions are recorded for `zj-herdr restore`.
    pub sessions: PathBuf,
}

pub fn run(opts: Options) -> Result<(), String> {
    if let Some(r) = opts.remotes.iter().find(|r| r.name == opts.machine) {
        return Err(format!(
            "remote {:?} has the same name as this machine; rename it",
            r.name
        ));
    }
    let (mut store, aside) = Store::load_or_set_aside(&opts.sessions)?;
    if let Some(aside) = aside {
        eprintln!(
            "zj-herdr: {} was unreadable; moved it to {} and started afresh",
            opts.sessions.display(),
            aside.display()
        );
    }
    let listener =
        server::bind(&opts.socket).map_err(|e| format!("{}: {e}", opts.socket.display()))?;
    let registry = Arc::new(Registry::default());
    let zellij: Arc<dyn Zellij> = Arc::new(ZellijCli);
    let ctx = Arc::new(Context {
        registry: Arc::clone(&registry),
        zellij: Arc::clone(&zellij),
        machine: opts.machine.clone(),
        plugin: opts.plugin.clone(),
    });
    eprintln!("zj-herdr: listening on {}", opts.socket.display());

    thread::spawn(move || server::serve(listener, ctx));
    {
        let registry = Arc::clone(&registry);
        let zellij = Arc::clone(&zellij);
        thread::spawn(move || push_loop(&registry, zellij.as_ref()));
    }
    for r in opts.remotes {
        let registry = Arc::clone(&registry);
        thread::spawn(move || remote::poll_loop(r, registry));
    }

    let mut scanner = Scanner::new(opts.machine, &opts.roots);
    let mut tabs = TabCache::default();
    loop {
        let started = Instant::now();
        let mut machine = scanner.scan();
        tabs.fill(&mut machine.agents, zellij.as_ref(), now_secs());
        if store.record(&machine.agents, now_secs()) {
            if let Err(e) = store.save(&opts.sessions) {
                eprintln!("zj-herdr: {}: {e}", opts.sessions.display());
            }
        }
        registry.publish(machine, now_secs());
        thread::sleep(SCAN_INTERVAL.saturating_sub(started.elapsed()));
    }
}

/// Deliver `zj_herdr.state.v1` to every running session on change, and on a
/// heartbeat. Sessions without the plugin simply ignore the pipe.
fn push_loop(registry: &Registry, zellij: &dyn Zellij) {
    let mut pushed: Option<u64> = None;
    let mut last = Instant::now();
    let mut last_error = String::new();
    loop {
        thread::sleep(PUSH_POLL);
        let count = registry.change_count();
        if pushed == Some(count) && last.elapsed() < PUSH_HEARTBEAT {
            continue;
        }
        pushed = Some(count);
        last = Instant::now();
        let json = registry.snapshot().to_json();
        let result = zellij.sessions().and_then(|sessions| {
            let failed: Vec<String> = sessions
                .iter()
                .filter_map(|s| {
                    zellij
                        .pipe(s, STATE_SCHEMA, &json)
                        .err()
                        .map(|e| format!("session {s}: {e}"))
                })
                .collect();
            if failed.is_empty() {
                Ok(())
            } else {
                Err(failed.join("; "))
            }
        });
        match result {
            Err(e) if e != last_error => {
                eprintln!("zj-herdr: push: {e}");
                last_error = e;
            }
            Err(_) => {}
            Ok(()) => last_error.clear(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use zj_herdr_core::{AgentState, Pane};

    /// Answers `panes` for `dev` only, and counts the questions.
    #[derive(Default)]
    struct FakeZellij {
        asked: Mutex<Vec<String>>,
    }

    impl Zellij for FakeZellij {
        fn sessions(&self) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
        fn pipe(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn deliver(&self, _: &str, _: &str, _: &str, _: &str) -> Result<String, String> {
            Ok(String::new())
        }
        fn run(&self, _: &str, _: Option<&str>, _: &[String]) -> Result<(), String> {
            Ok(())
        }
        fn panes(&self, session: &str) -> Result<HashMap<u32, PaneTab>, String> {
            self.asked.lock().unwrap().push(session.to_string());
            if session != "dev" {
                return Err("There is no active session!".into());
            }
            let tab = PaneTab {
                position: 1,
                name: "api".into(),
            };
            Ok(HashMap::from([(3, tab)]))
        }
    }

    fn agent(pid: u32, pane: Option<Pane>) -> Agent {
        Agent {
            id: Agent::make_id("box", pid),
            machine: "box".into(),
            kind: "claude".into(),
            pid,
            cwd: None,
            state: AgentState::Idle,
            since: 0,
            pane,
            session_id: None,
        }
    }

    #[test]
    fn tab_cache_asks_sessions_with_agents_at_most_every_few_seconds() {
        let fake = FakeZellij::default();
        let mut cache = TabCache::default();
        let scan = || {
            vec![
                agent(1, Some(Pane::new("dev", 3))),
                agent(2, Some(Pane::new("dev", 9))),
                agent(3, Some(Pane::new("gone", 1))),
                agent(4, None),
            ]
        };
        let mut agents = scan();
        cache.fill(&mut agents, &fake, 100);
        let tab = |a: &Agent| a.pane.as_ref().unwrap().tab.clone();
        assert_eq!(tab(&agents[0]).unwrap().name, "api");
        assert_eq!(tab(&agents[1]), None, "a pane the session does not list");
        assert_eq!(tab(&agents[2]), None, "a session that did not answer");
        assert_eq!(agents[3].pane, None);
        assert_eq!(*fake.asked.lock().unwrap(), ["dev", "gone"]);

        // Cached, failures included, until the refresh interval is up.
        let mut agents = scan();
        cache.fill(&mut agents, &fake, 100 + TAB_REFRESH_SECS - 1);
        assert_eq!(tab(&agents[0]).unwrap().position, 1);
        assert_eq!(fake.asked.lock().unwrap().len(), 2);
        cache.fill(&mut scan(), &fake, 100 + TAB_REFRESH_SECS);
        assert_eq!(fake.asked.lock().unwrap().len(), 4);

        // A session whose agents are gone is neither asked nor kept.
        cache.fill(&mut [agent(4, None)], &fake, 1000);
        assert_eq!(fake.asked.lock().unwrap().len(), 4);
        assert!(cache.sessions.is_empty());
    }
}
