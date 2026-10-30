//! `zj-herdr daemon`: rescan, serve the socket, push state to Zellij.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use zj_herdr_core::{Agent, PaneTab, STATE_SCHEMA};

use crate::adopt::{SystemProcesses, ADOPT_TIMEOUT};
use crate::config::Remote;
use crate::detect::Roots;
use crate::registry::Registry;
use crate::remote;
use crate::scan::{now_secs, Scanner};
use crate::server::{self, Context};
use crate::sessions::Store;
use crate::sync::lock;
use crate::zellij::{Zellij, ZellijCli};

/// The plan's cadence: fast enough to feel live, cheap enough to leave on.
pub const SCAN_INTERVAL: Duration = Duration::from_secs(1);
/// How often the pusher looks for changes to deliver. Every delivery makes
/// each zj_herdr instance parse the whole state, so faster burns CPU.
const PUSH_POLL: Duration = Duration::from_secs(2);
/// Re-push unchanged state this often so a plugin loaded after the last
/// change, or a new session, fills in within a few seconds.
const PUSH_HEARTBEAT: Duration = Duration::from_secs(5);
/// Ask a session for its panes' tabs at most this often.
pub const TAB_REFRESH_SECS: u64 = 5;

/// The tab of every agent's pane, so the plugin can group agents of
/// sessions Zellij did not describe to it. One `list-panes` per session
/// that has agents, at most every [`TAB_REFRESH_SECS`], asked by
/// [`tab_loop`] off the scan thread so a wedged session never delays state.
#[derive(Default)]
pub struct TabCache {
    /// Per session: when it was asked, and its answer (`None` on failure).
    sessions: HashMap<String, (u64, Option<HashMap<u32, PaneTab>>)>,
    /// Sessions the last scan found agents in.
    wanted: BTreeSet<String>,
}

impl TabCache {
    /// Note the sessions `agents` run in, drop the others, and set
    /// `pane.tab` from what is cached: `None` where the session has not
    /// answered (yet) or does not list the pane.
    pub fn apply(&mut self, agents: &mut [Agent]) {
        self.wanted = agents
            .iter()
            .filter_map(|a| a.pane.as_ref().map(|p| p.session.clone()))
            .collect();
        let wanted = &self.wanted;
        self.sessions.retain(|s, _| wanted.contains(s));
        for pane in agents.iter_mut().filter_map(|a| a.pane.as_mut()) {
            pane.tab = self
                .sessions
                .get(&pane.session)
                .and_then(|(_, tabs)| tabs.as_ref()?.get(&pane.pane_id).cloned());
        }
    }

    /// Wanted sessions not asked within [`TAB_REFRESH_SECS`].
    pub fn stale(&self, now: u64) -> Vec<String> {
        self.wanted
            .iter()
            .filter(|s| {
                self.sessions
                    .get(*s)
                    .is_none_or(|(at, _)| now.saturating_sub(*at) >= TAB_REFRESH_SECS)
            })
            .cloned()
            .collect()
    }

    /// Record a session's answer, unless its agents are gone meanwhile.
    pub fn store(&mut self, session: String, now: u64, tabs: Option<HashMap<u32, PaneTab>>) {
        if self.wanted.contains(&session) {
            self.sessions.insert(session, (now, tabs));
        }
    }
}

/// Ask stale sessions for their tabs, never holding the lock across a call.
fn refresh_tabs(cache: &Mutex<TabCache>, zellij: &dyn Zellij) {
    let stale = lock(cache).stale(now_secs());
    for session in stale {
        let tabs = zellij.panes(&session).ok();
        lock(cache).store(session, now_secs(), tabs);
    }
}

fn tab_loop(cache: &Mutex<TabCache>, zellij: &dyn Zellij) {
    loop {
        refresh_tabs(cache, zellij);
        thread::sleep(SCAN_INTERVAL);
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
    run_with(
        opts,
        Arc::new(ZellijCli),
        remote::ssh_command,
        &AtomicBool::new(false),
    )
}

/// [`run`] against any Zellij, returning `Ok` once `stop` is set (after the
/// scan in progress); the real daemon never sets it.
fn run_with(
    opts: Options,
    zellij: Arc<dyn Zellij>,
    ssh: fn(&Remote) -> Command,
    stop: &AtomicBool,
) -> Result<(), String> {
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
    let ctx = Arc::new(Context {
        registry: Arc::clone(&registry),
        zellij: Arc::clone(&zellij),
        machine: opts.machine.clone(),
        plugin: opts.plugin.clone(),
        procs: Arc::new(SystemProcesses),
        adopt_timeout: ADOPT_TIMEOUT,
        adopting: Default::default(),
    });
    eprintln!("zj-herdr: listening on {}", opts.socket.display());

    {
        let registry = Arc::clone(&registry);
        let zellij = Arc::clone(&zellij);
        thread::spawn(move || push_loop(&registry, zellij.as_ref()));
    }
    for r in opts.remotes {
        let registry = Arc::clone(&registry);
        thread::spawn(move || remote::poll_loop(r, registry, ssh));
    }

    let tabs = Arc::new(Mutex::new(TabCache::default()));
    {
        let tabs = Arc::clone(&tabs);
        let zellij = Arc::clone(&zellij);
        thread::spawn(move || tab_loop(&tabs, zellij.as_ref()));
    }
    let mut scanner = Scanner::new(opts.machine, &opts.roots);
    let mut scan = || {
        let mut machine = scanner.scan();
        lock(&tabs).apply(&mut machine.agents);
        record_sessions(&mut store, &machine.agents, &opts.sessions);
        registry.publish(machine, now_secs());
    };

    // Serve only once this machine is published, so a client connecting at
    // startup (it waits in the socket backlog meanwhile) never sees it absent.
    let mut started = Instant::now();
    scan();
    thread::spawn(move || server::serve(listener, ctx));
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(SCAN_INTERVAL.saturating_sub(started.elapsed()));
        started = Instant::now();
        scan();
    }
    Ok(())
}

/// Remember the panes' agent sessions for `zj-herdr restore`, saving only
/// on a change; a failed save is reported, never fatal.
fn record_sessions(store: &mut Store, agents: &[Agent], path: &Path) {
    if store.record(agents, now_secs()) {
        if let Err(e) = store.save(path) {
            eprintln!("zj-herdr: {}: {e}", path.display());
        }
    }
}

/// Which sessions get the state this round: every attached one on a change
/// or heartbeat, otherwise only those attached since the last round.
/// Detached sessions get nothing; they catch up when a client attaches.
pub fn push_targets(attached: &[String], before: &[String], due: bool) -> Vec<String> {
    attached
        .iter()
        .filter(|s| due || !before.contains(s))
        .cloned()
        .collect()
}

/// What the pusher remembers between rounds.
#[derive(Default)]
struct Pusher {
    /// The change count last delivered to every attached session.
    pushed: Option<u64>,
    last: Option<Instant>,
    /// Sessions attached at the last round.
    before: Vec<String>,
}

impl Pusher {
    /// One round at `now`: deliver the state where [`push_targets`] says.
    fn round(
        &mut self,
        registry: &Registry,
        zellij: &dyn Zellij,
        now: Instant,
    ) -> Result<(), String> {
        let count = registry.change_count();
        let due = self.pushed != Some(count)
            || self
                .last
                .is_none_or(|l| now.duration_since(l) >= PUSH_HEARTBEAT);
        let sessions = zellij.sessions()?;
        // An unanswered `list-clients` counts as attached: better a wasted
        // pipe than a rail left stale.
        let attached: Vec<String> = sessions
            .into_iter()
            .filter(|s| zellij.attached(s).unwrap_or(true))
            .collect();
        let targets = push_targets(&attached, &self.before, due);
        self.before = attached;
        if due {
            self.pushed = Some(count);
            self.last = Some(now);
        }
        if targets.is_empty() {
            return Ok(());
        }
        let json = registry.snapshot().to_json();
        let failed: Vec<String> = targets
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
    }
}

/// Deliver `zj_herdr.state.v1` to every attached session on change, and on a
/// heartbeat. Sessions without the plugin simply ignore the pipe.
fn push_loop(registry: &Registry, zellij: &dyn Zellij) {
    let mut pusher = Pusher::default();
    let mut last_error = String::new();
    loop {
        thread::sleep(PUSH_POLL);
        report_push(
            pusher.round(registry, zellij, Instant::now()),
            &mut last_error,
        );
    }
}

/// Print a failed round once, however long it keeps failing.
fn report_push(result: Result<(), String>, last_error: &mut String) {
    match result {
        Err(e) if e != *last_error => {
            eprintln!("zj-herdr: push: {e}");
            *last_error = e;
        }
        Err(_) => {}
        Ok(()) => last_error.clear(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{CommandLine, Request};
    use crate::client;
    use crate::config::Config;
    use std::sync::Mutex;
    use zj_herdr_core::{AgentState, Machine, MachineStatus, Pane};

    #[test]
    fn pushes_to_attached_sessions_only() {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let attached = v(&["a", "b"]);
        assert_eq!(push_targets(&attached, &v(&["a"]), true), attached);
        // Nothing new: only a session attached since the last round.
        assert_eq!(push_targets(&attached, &v(&["a"]), false), v(&["b"]));
        assert!(push_targets(&attached, &attached, false).is_empty());
        assert!(push_targets(&[], &attached, true).is_empty());
    }

    /// Sessions `a` (attached), `b` (detached) and `c` (unanswered, and its
    /// pipe fails unless `healthy`); records each pipe, run and `panes`
    /// question. `panes` answers for `dev` only.
    #[derive(Default)]
    struct FakeZellij {
        down: bool,
        healthy: bool,
        piped: Mutex<Vec<String>>,
        asked: Mutex<Vec<String>>,
        ran: Mutex<Vec<String>>,
    }

    impl Zellij for FakeZellij {
        fn sessions(&self) -> Result<Vec<String>, String> {
            if self.down {
                return Err("no server".into());
            }
            Ok(vec!["a".into(), "b".into(), "c".into()])
        }
        fn attached(&self, session: &str) -> Result<bool, String> {
            match session {
                "c" => Err("timeout".into()),
                s => Ok(s == "a"),
            }
        }
        fn pipe(&self, session: &str, name: &str, _: &str) -> Result<(), String> {
            assert_eq!(name, STATE_SCHEMA);
            self.piped.lock().unwrap().push(session.to_string());
            if session == "c" && !self.healthy {
                return Err("gone".into());
            }
            Ok(())
        }
        fn deliver(&self, _: &str, _: &str, _: &str, _: &str) -> Result<String, String> {
            Ok(String::new())
        }
        fn run(&self, session: &str, _: Option<&str>, command: &[String]) -> Result<(), String> {
            let call = format!("{session} {}", command.join(" "));
            self.ran.lock().unwrap().push(call);
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

    #[test]
    fn pusher_delivers_on_change_and_heartbeat_to_attached_sessions() {
        let registry = Registry::default();
        let zellij = FakeZellij::default();
        let piped = || std::mem::take(&mut *zellij.piped.lock().unwrap());
        let mut pusher = Pusher::default();
        let t0 = Instant::now();
        // First round: to `a`, and to `c` whose clients are unknown.
        let err = pusher.round(&registry, &zellij, t0).unwrap_err();
        assert_eq!(err, "session c: gone");
        assert_eq!(piped(), ["a", "c"]);
        // Nothing changed, no heartbeat yet: nothing sent.
        assert!(pusher.round(&registry, &zellij, t0).is_ok());
        assert!(piped().is_empty());
        // The heartbeat sends again.
        let later = t0 + PUSH_HEARTBEAT;
        assert!(pusher.round(&registry, &zellij, later).is_err());
        assert_eq!(piped(), ["a", "c"]);
        // So does a change.
        registry.publish(
            Machine {
                name: "box".into(),
                ssh: None,
                status: MachineStatus::Ok,
                error: None,
                agents: vec![agent(1, None)],
            },
            0,
        );
        assert!(pusher.round(&registry, &zellij, later).is_err());
        assert_eq!(piped(), ["a", "c"]);
        // No sessions to ask: the error, and nothing sent.
        let down = FakeZellij {
            down: true,
            ..FakeZellij::default()
        };
        assert_eq!(
            pusher.round(&registry, &down, later),
            Err("no server".into())
        );
        assert!(down.piped.lock().unwrap().is_empty());
        // Every pipe lands: the round is a success.
        let healthy = FakeZellij {
            healthy: true,
            ..FakeZellij::default()
        };
        assert_eq!(Pusher::default().round(&registry, &healthy, later), Ok(()));
        assert_eq!(*healthy.piped.lock().unwrap(), ["a", "c"]);
    }

    #[test]
    fn a_failing_push_is_reported_once_until_it_recovers() {
        let mut last = String::new();
        report_push(Err("boom".into()), &mut last);
        assert_eq!(last, "boom");
        report_push(Err("boom".into()), &mut last);
        assert_eq!(last, "boom");
        report_push(Err("other".into()), &mut last);
        assert_eq!(last, "other");
        report_push(Ok(()), &mut last);
        assert_eq!(last, "");
    }

    #[test]
    fn sessions_are_saved_on_change_and_a_failed_save_is_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = agent(1, Some(Pane::new("dev", 3)));
        a.session_id = Some("0f0e0d0c-0b0a-4908-8706-050403020100".into());
        let (mut store, good) = (Store::default(), dir.path().join("sessions.json"));
        record_sessions(&mut store, &[a.clone()], &good);
        assert!(good.exists());
        // Unchanged: nothing to save, so nothing is written again.
        std::fs::remove_file(&good).unwrap();
        record_sessions(&mut store, &[a.clone()], &good);
        assert!(!good.exists());
        // A path below a file cannot be saved to.
        let mut store = Store::default();
        let bad = good.join("below/sessions.json");
        std::fs::write(&good, "").unwrap();
        record_sessions(&mut store, &[a], &bad);
        assert!(!bad.exists());
    }

    #[test]
    fn refresh_tabs_asks_stale_sessions_and_keeps_the_answers() {
        let fake = FakeZellij::default();
        let cache = Mutex::new(TabCache::default());
        let mut agents = [agent(1, Some(Pane::new("dev", 3)))];
        lock(&cache).apply(&mut agents);
        refresh_tabs(&cache, &fake);
        lock(&cache).apply(&mut agents);
        let tab = agents[0].pane.as_ref().unwrap().tab.as_ref();
        assert_eq!(tab.unwrap().name, "api");
        assert_eq!(*fake.asked.lock().unwrap(), ["dev"]);
        // Fresh answers are not asked for again.
        refresh_tabs(&cache, &fake);
        assert_eq!(fake.asked.lock().unwrap().len(), 1);
    }

    fn options(dir: &std::path::Path) -> Options {
        Options {
            socket: dir.join("run/zj-herdr.sock"),
            machine: "box".into(),
            roots: Roots::default(),
            remotes: vec![],
            plugin: "zj_herdr".into(),
            sessions: dir.join("sessions.json"),
        }
    }

    /// Stands in for ssh: prints nothing, so the poll fails and no network is used.
    fn no_ssh(_: &Remote) -> Command {
        Command::new("true")
    }

    fn remote(name: &str) -> Remote {
        let toml =
            format!("[[remote]]\nname = \"{name}\"\nssh = \"far-alias\"\ninterval_secs = 3600");
        Config::parse(&toml).unwrap().remote.remove(0)
    }

    #[test]
    fn run_refuses_a_remote_named_like_this_machine() {
        let dir = tempfile::tempdir().unwrap();
        let opts = Options {
            remotes: vec![remote("box")],
            ..options(dir.path())
        };
        let err = run(opts).unwrap_err();
        assert!(err.contains("same name as this machine"), "{err}");
    }

    #[test]
    fn run_sets_an_unreadable_sessions_file_aside_and_reports_a_bad_socket() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = options(dir.path());
        std::fs::write(&opts.sessions, "not json").unwrap();
        // The socket's directory would sit below a file.
        opts.socket = opts.sessions.join("run/zj-herdr.sock");
        let stop = AtomicBool::new(true);
        let err = run_with(opts, Arc::new(FakeZellij::default()), no_ssh, &stop).unwrap_err();
        assert!(err.contains("sessions.json/run"), "{err}");
        assert!(dir.path().join("sessions.json.unreadable").exists());
    }

    #[test]
    fn the_daemon_scans_serves_and_pushes_until_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = options(dir.path());
        opts.remotes = vec![remote("far")];
        let socket = opts.socket.clone();
        let fake = Arc::new(FakeZellij::default());
        let stop = Arc::new(AtomicBool::new(false));
        let daemon = {
            let (fake, stop) = (Arc::clone(&fake), Arc::clone(&stop));
            thread::spawn(move || run_with(opts, fake, no_ssh, &stop))
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let state = loop {
            assert!(!daemon.is_finished(), "the daemon exited");
            match client::request(&socket, &Request::List) {
                Ok(resp) => break resp.state.unwrap(),
                Err(e) => assert!(Instant::now() < deadline, "no answer: {e}"),
            }
            thread::sleep(Duration::from_millis(50));
        };
        assert!(state.machines.iter().any(|m| m.name == "box"));

        let spawn = Request::Spawn {
            session: "dev".into(),
            cwd: None,
            command: CommandLine::Argv(vec!["claude".into()]),
        };
        client::request(&socket, &spawn).unwrap();
        assert_eq!(*fake.ran.lock().unwrap(), ["dev claude"]);
        // Prompts only reach `deliver` for a pane the scan found.
        assert_eq!(fake.deliver("", "", "", ""), Ok(String::new()));

        // The pusher's first round comes after PUSH_POLL.
        while fake.piped.lock().unwrap().is_empty() {
            assert!(Instant::now() < deadline + PUSH_POLL, "nothing pushed");
            thread::sleep(Duration::from_millis(50));
        }
        stop.store(true, Ordering::Relaxed);
        daemon.join().unwrap().unwrap();
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

    impl TabCache {
        /// One scan as the daemon sees it once `tab_loop` has caught up.
        fn fill(&mut self, agents: &mut [Agent], zellij: &dyn Zellij, now: u64) {
            self.apply(agents);
            for session in self.stale(now) {
                let tabs = zellij.panes(&session).ok();
                self.store(session, now, tabs);
            }
            self.apply(agents);
        }
    }

    #[test]
    fn tab_cache_drops_answers_for_sessions_gone_meanwhile() {
        let mut cache = TabCache::default();
        cache.apply(&mut [agent(1, Some(Pane::new("dev", 3)))]);
        assert_eq!(cache.stale(100), ["dev"]);
        cache.apply(&mut [agent(4, None)]);
        cache.store("dev".into(), 100, None);
        assert!(cache.sessions.is_empty());
        assert!(cache.stale(100).is_empty());
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
