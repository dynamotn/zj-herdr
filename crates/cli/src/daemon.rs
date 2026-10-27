//! `zj-herdr daemon`: rescan, serve the socket, push state to Zellij.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use zj_herdr_core::STATE_SCHEMA;

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
        thread::spawn(move || push_loop(&registry, zellij.as_ref()));
    }
    for r in opts.remotes {
        let registry = Arc::clone(&registry);
        thread::spawn(move || remote::poll_loop(r, registry));
    }

    let mut scanner = Scanner::new(opts.machine, &opts.roots);
    loop {
        let started = Instant::now();
        let machine = scanner.scan();
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
