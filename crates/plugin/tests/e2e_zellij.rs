//! End to end in a real Zellij: a headless session in a private HOME and
//! TMPDIR loads the built `zj_herdr.wasm` (permissions granted ahead in
//! Zellij's cache), receives a state on its pipe, and the rail it draws is
//! read back with `zellij action dump-screen`.
//!
//! Needs `zellij` on PATH and the wasm built first:
//!
//! ```sh
//! cargo build -p zj-herdr-plugin --target wasm32-wasip1 --release
//! cargo test -p zj-herdr-plugin --test e2e_zellij
//! ```
//!
//! Without them, or where Zellij cannot start a server (a sandbox that
//! forbids Unix sockets), the test skips; with `ZJ_HERDR_E2E_ZELLIJ=1` (CI)
//! it fails instead.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use zj_herdr_core::{Agent, AgentState, Machine, MachineStatus, Pane, StateV1, Tokens};

const SESSION: &str = "zj-herdr-e2e";
const PERMISSIONS: &str =
    "ReadApplicationState\nChangeApplicationState\nWriteToStdin\nReadCliPipes\nRunCommands\n";

fn required() -> bool {
    std::env::var("ZJ_HERDR_E2E_ZELLIJ").is_ok_and(|v| v == "1")
}

/// Skip, or fail when the run requires this test.
fn skip(why: &str) {
    assert!(!required(), "ZJ_HERDR_E2E_ZELLIJ=1 but {why}");
    eprintln!("skipped: {why}");
}

fn wasm() -> PathBuf {
    std::env::var_os("ZJ_HERDR_WASM").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/wasm32-wasip1/release/zj_herdr.wasm")
        },
        PathBuf::from,
    )
}

struct Zellij {
    /// A client attached through a pty: a background session has no size,
    /// and draws nothing, until one is.
    client: Option<std::process::Child>,
    dir: PathBuf,
    /// Zellij's socket directory: Unix socket paths are capped at about
    /// 100 bytes, which a temp dir under `/var/folders` already exceeds.
    sockets: PathBuf,
}

impl Zellij {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("zje2e-{}-{nanos}", std::process::id()));
        for sub in ["home", "tmp", "config/zellij", "cache/zellij"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let short = PathBuf::from(format!("/tmp/zje2e-{}", std::process::id()));
        let sockets = if std::fs::create_dir_all(&short).is_ok() {
            short
        } else {
            dir.join("s")
        };
        std::fs::create_dir_all(&sockets).unwrap();
        Zellij {
            client: None,
            dir,
            sockets,
        }
    }

    /// `program` in this Zellij's private environment.
    fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.env("HOME", self.dir.join("home"))
            .env("TMPDIR", self.dir.join("tmp"))
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env("XDG_CACHE_HOME", self.dir.join("cache"))
            .env("ZELLIJ_CONFIG_DIR", self.dir.join("config/zellij"))
            .env("ZELLIJ_SOCKET_DIR", &self.sockets)
            .env_remove("ZELLIJ")
            .env_remove("ZELLIJ_SESSION_NAME")
            .stdin(Stdio::null());
        cmd
    }

    fn cmd(&self) -> Command {
        self.command("zellij")
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    /// Config, a layout of the rail alone, and the plugin's permissions
    /// granted where Zellij caches them (macOS and XDG locations).
    fn configure(&self, wasm: &Path) {
        let wasm = wasm.canonicalize().unwrap();
        let config = self.dir.join("config/zellij");
        let layout = config.join("rail.kdl");
        std::fs::write(
            &layout,
            format!(
                "layout {{\n    pane {{\n        plugin location=\"file:{}\" {{\n            density \"compact\"\n            rename_tabs \"false\"\n            zj_herdr_bin \"/usr/bin/false\"\n        }}\n    }}\n}}\n",
                wasm.display()
            ),
        )
        .unwrap();
        std::fs::write(
            config.join("config.kdl"),
            format!(
                "show_startup_tips false\nshow_release_notes false\ndefault_layout \"{}\"\n",
                layout.display()
            ),
        )
        .unwrap();
        let grant = format!("\"{}\" {{\n{PERMISSIONS}}}\n", wasm.display());
        for dir in [
            self.dir
                .join("home/Library/Caches/org.Zellij-Contributors.Zellij"),
            self.dir.join("cache/zellij"),
        ] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("permissions.kdl"), &grant).unwrap();
        }
    }

    /// Why a start failed: the client's stderr and the end of Zellij's log.
    fn diagnose(&self) -> String {
        let err = std::fs::read_to_string(self.dir.join("attach.err")).unwrap_or_default();
        let log = std::fs::read_dir(self.dir.join("tmp"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path().join("zellij-log/zellij.log"))
            .find(|p| p.is_file())
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default();
        let tail: Vec<&str> = log.lines().rev().take(15).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        format!(
            "attach stderr: {err}\nzellij.log tail:\n{}",
            tail.join("\n")
        )
    }

    fn start(&self) -> bool {
        let config = self.dir.join("config/zellij/config.kdl");
        // The server it forks keeps any pipe open: no output is read, and
        // the client gets a deadline of its own.
        let mut client = self
            .cmd()
            .args(["--config", config.to_str().unwrap()])
            .args(["attach", "--create-background", SESSION])
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(self.dir.join("attach.err")).unwrap())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match client.try_wait().unwrap() {
                Some(status) if status.success() => break,
                Some(_) => return false,
                None if Instant::now() > deadline => {
                    let _ = client.kill();
                    let _ = client.wait();
                    return false;
                }
                None => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if self.plugin_pane().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    }

    /// `plugin_<id>` of the rail, once Zellij lists it.
    fn plugin_pane(&self) -> Option<String> {
        let out = self.run(&["--session", SESSION, "action", "list-panes", "--json"]);
        let text = String::from_utf8_lossy(&out.stdout);
        let panes: Vec<serde_like::Pane> = serde_like::panes(&text);
        panes
            .into_iter()
            .find(|p| p.is_plugin)
            .map(|p| format!("plugin_{}", p.id))
    }

    fn pipe_state(&self, state: &StateV1) {
        let mut child = self
            .cmd()
            .args([
                "--session",
                SESSION,
                "pipe",
                "--name",
                "zj_herdr.state.v1",
                "--",
            ])
            .arg(state.to_json())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Attach a client in a pty of 40 by 60, through `script(1)`.
    fn attach(&mut self) {
        let inner = format!("stty rows 40 cols 60; exec zellij attach {SESSION}");
        let mut script = self.command("script");
        if cfg!(target_os = "macos") {
            script.args(["-q", "/dev/null", "sh", "-c", &inner]);
        } else {
            script.args(["-qfc", &format!("sh -c '{inner}'"), "/dev/null"]);
        }
        self.client = script
            .env("TERM", "xterm-256color")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
    }

    /// The rail's text: the plugin pane by id, else the focused pane (the
    /// layout has only the rail).
    fn screen(&self, pane: &str) -> String {
        let dump = |args: &[&str]| {
            let mut all = vec!["--session", SESSION, "action", "dump-screen"];
            all.extend_from_slice(args);
            let out = self.run(&all);
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let by_id = dump(&["--pane-id", pane]);
        if by_id.trim().is_empty() {
            dump(&[])
        } else {
            by_id
        }
    }

    /// What Zellij says about the session, for a failure message.
    fn report(&self, pane: &str) -> String {
        let panes = self.run(&["--session", SESSION, "action", "list-panes", "--json"]);
        let dump = self.run(&[
            "--session",
            SESSION,
            "action",
            "dump-screen",
            "--pane-id",
            pane,
        ]);
        format!(
            "pane {pane}\nlist-panes: {}\ndump-screen stderr: {}\n{}",
            String::from_utf8_lossy(&panes.stdout),
            String::from_utf8_lossy(&dump.stderr),
            self.diagnose()
        )
    }
}

impl Drop for Zellij {
    fn drop(&mut self) {
        if let Some(mut client) = self.client.take() {
            let _ = client.kill();
            let _ = client.wait();
        }
        let _ = self.run(&["kill-session", SESSION]);
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_dir_all(&self.sockets);
    }
}

/// Just enough JSON reading for `list-panes --json`, without a dependency.
mod serde_like {
    pub struct Pane {
        pub id: u64,
        pub is_plugin: bool,
    }

    /// Each object's `"id"` and `"is_plugin"`, in order.
    pub fn panes(text: &str) -> Vec<Pane> {
        text.split('{')
            .skip(1)
            .filter_map(|obj| {
                let field = |name: &str| {
                    let at = obj.find(&format!("\"{name}\":"))? + name.len() + 3;
                    Some(
                        obj[at..]
                            .trim_start()
                            .split([',', '}'])
                            .next()?
                            .trim()
                            .to_string(),
                    )
                };
                Some(Pane {
                    id: field("id")?.parse().ok()?,
                    is_plugin: field("is_plugin")? == "true",
                })
            })
            .collect()
    }
}

fn state() -> StateV1 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let agent = Agent {
        id: "box/7".into(),
        machine: "box".into(),
        kind: "claude".into(),
        pid: 7,
        cwd: Some("/w/e2e-project".into()),
        state: AgentState::Blocked,
        since: now - 90,
        pane: Some(Pane::new("elsewhere", 4)),
        session_id: None,
        waiting: Some("Bash: cargo test".into()),
        started: 0,
        tokens: Some(Tokens {
            input: 4_800_000,
            ..Tokens::default()
        }),
        error: false,
        acked: false,
        tasks: vec![],
        model: Some("claude-opus-5-5".into()),
        context: Some(62),
        background: false,
        title: None,
    };
    StateV1::new(
        now,
        vec![Machine {
            name: "box".into(),
            ssh: None,
            status: MachineStatus::Ok,
            error: None,
            agents: vec![agent],
        }],
    )
}

#[test]
fn the_rail_draws_a_piped_state_in_real_zellij() {
    let wasm = wasm();
    if !wasm.is_file() {
        return skip(&format!("{} is not built", wasm.display()));
    }
    let mut zellij = Zellij::new();
    if zellij.run(&["--version"]).status.code().is_none() {
        return skip("zellij is not on PATH");
    }
    zellij.configure(&wasm);
    if !zellij.start() {
        return skip(&format!(
            "Zellij could not start a session here\n{}",
            zellij.diagnose()
        ));
    }
    let pane = zellij.plugin_pane().unwrap();
    zellij.attach();
    // The first pipe may reach the plugin before its permissions load.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut screen = String::new();
    while Instant::now() < deadline {
        zellij.pipe_state(&state());
        screen = zellij.screen(&pane);
        if screen.contains("e2e-project") {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(
        screen.contains("zj-herdr"),
        "screen: {screen:?}\n{}",
        zellij.report(&pane)
    );
    assert!(screen.contains("·1 1!"), "one agent, needing you: {screen}");
    assert!(screen.contains("◆ ✳ e2e-project"), "{screen}");
    assert!(screen.contains("opus5.5 · 4.8M"), "{screen}");
    assert!(screen.contains("↳ Bash: cargo test"), "{screen}");
}
