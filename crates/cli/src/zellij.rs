//! The few Zellij CLI calls the daemon makes, behind a trait for tests.

use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

use serde_json::Value;
use zj_herdr_core::PaneTab;

use crate::exec;

/// Zellij answers locally; anything slower is a wedged session.
const ZELLIJ_TIMEOUT: Duration = Duration::from_secs(3);
/// A pipe to one plugin may first have to load and compile it.
const DELIVER_TIMEOUT: Duration = Duration::from_secs(20);
/// The plugin types prompts only in an instance configured with this.
const DELIVER_CONFIG: &str = "deliver_prompts=true";

pub trait Zellij: Send + Sync {
    /// Names of the running sessions.
    fn sessions(&self) -> Result<Vec<String>, String>;
    /// Whether a client is attached to the session, from `zellij --session S
    /// action list-clients`. Fakes default to attached.
    fn attached(&self, _session: &str) -> Result<bool, String> {
        Ok(true)
    }
    /// `zellij --session S pipe --name NAME -- PAYLOAD`: to every plugin
    /// already loaded in the session.
    fn pipe(&self, session: &str, name: &str, payload: &str) -> Result<(), String>;
    /// [`deliver_args`]: to one plugin, launched floating if it is not
    /// loaded yet. Returns what the plugin wrote back.
    fn deliver(
        &self,
        session: &str,
        plugin: &str,
        name: &str,
        payload: &str,
    ) -> Result<String, String>;
    /// `zellij --session S run [--cwd C] -- COMMAND...`
    fn run(&self, session: &str, cwd: Option<&str>, command: &[String]) -> Result<(), String>;
    /// The tab of each terminal pane in a session, by pane id, from
    /// `zellij --session S action list-panes --json --tab`.
    fn panes(&self, session: &str) -> Result<HashMap<u32, PaneTab>, String>;
}

pub struct ZellijCli;

/// Arguments for `zellij run`.
pub fn run_args(session: &str, cwd: Option<&str>, command: &[String]) -> Vec<String> {
    let mut args = vec![
        "--session".to_string(),
        session.to_string(),
        "run".to_string(),
    ];
    if let Some(cwd) = cwd {
        args.extend(["--cwd".to_string(), cwd.to_string()]);
    }
    args.push("--".to_string());
    args.extend(command.iter().cloned());
    args
}

/// Arguments for `zellij action pipe` to `plugin` (a URL or an alias from
/// the user's Zellij config), launching it floating when it is not loaded.
/// Zellij matches instances by URL and configuration, so the extra
/// configuration reaches one dedicated instance per session, never the
/// picker panes of a layout (which would each type the prompt again).
pub fn deliver_args(session: &str, plugin: &str, name: &str, payload: &str) -> Vec<String> {
    [
        "--session",
        session,
        "action",
        "pipe",
        "--plugin",
        plugin,
        "--plugin-configuration",
        DELIVER_CONFIG,
        "--floating-plugin",
        "true",
        "--name",
        name,
        "--",
        payload,
    ]
    .map(str::to_string)
    .to_vec()
}

/// Session names from `zellij list-sessions --no-formatting`, without exited
/// ones. Not `--short`: it drops the `EXITED` marker, and piping to an exited
/// session makes Zellij resurrect it.
pub fn parse_sessions(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains("EXITED"))
        .map(|l| l.split_whitespace().next().unwrap_or(l).to_string())
        .collect()
}

/// Whether `list-clients` names any client: a header line, then one row each.
pub fn parse_attached(stdout: &str) -> bool {
    stdout.lines().skip(1).any(|l| !l.trim().is_empty())
}

/// Terminal panes and their tabs from `list-panes --json --tab`: an array
/// of objects with `id`, `is_plugin`, `tab_position` and `tab_name` (a
/// `{"panes": [...]}` wrapper is accepted too). Plugin panes and entries
/// missing a field are skipped.
pub fn parse_panes(stdout: &str) -> Result<HashMap<u32, PaneTab>, String> {
    let value: Value =
        serde_json::from_str(stdout.trim()).map_err(|e| format!("list-panes: {e}"))?;
    let entries = match &value {
        Value::Array(items) => items,
        Value::Object(map) => match map.get("panes") {
            Some(Value::Array(items)) => items,
            _ => return Err("list-panes: no pane array".to_string()),
        },
        _ => return Err("list-panes: no pane array".to_string()),
    };
    Ok(entries
        .iter()
        .filter(|p| p["is_plugin"].as_bool() != Some(true))
        .filter_map(|p| {
            let id = u32::try_from(p["id"].as_u64()?).ok()?;
            let position = usize::try_from(p["tab_position"].as_u64()?).ok()?;
            let name = p["tab_name"].as_str()?.to_string();
            Some((id, PaneTab { position, name }))
        })
        .collect())
}

impl Zellij for ZellijCli {
    fn sessions(&self) -> Result<Vec<String>, String> {
        let mut cmd = Command::new("zellij");
        cmd.args(["list-sessions", "--no-formatting"]);
        match exec::run(cmd, ZELLIJ_TIMEOUT) {
            Ok(out) => Ok(parse_sessions(&out.stdout)),
            // Zellij exits non-zero when there are no sessions at all.
            Err(e) if e.contains("No active zellij sessions") => Ok(vec![]),
            Err(e) => Err(e),
        }
    }

    fn attached(&self, session: &str) -> Result<bool, String> {
        let mut cmd = Command::new("zellij");
        cmd.args(["--session", session, "action", "list-clients"]);
        exec::run(cmd, ZELLIJ_TIMEOUT).map(|out| parse_attached(&out.stdout))
    }

    fn pipe(&self, session: &str, name: &str, payload: &str) -> Result<(), String> {
        let mut cmd = Command::new("zellij");
        cmd.args(["--session", session, "pipe", "--name", name, "--", payload]);
        exec::run(cmd, ZELLIJ_TIMEOUT).map(drop)
    }

    fn deliver(
        &self,
        session: &str,
        plugin: &str,
        name: &str,
        payload: &str,
    ) -> Result<String, String> {
        let mut cmd = Command::new("zellij");
        cmd.args(deliver_args(session, plugin, name, payload));
        exec::run(cmd, DELIVER_TIMEOUT).map(|out| out.stdout)
    }

    fn run(&self, session: &str, cwd: Option<&str>, command: &[String]) -> Result<(), String> {
        if command.is_empty() {
            return Err("spawn needs a command".to_string());
        }
        let mut cmd = Command::new("zellij");
        cmd.args(run_args(session, cwd, command));
        exec::run(cmd, ZELLIJ_TIMEOUT).map(drop)
    }

    fn panes(&self, session: &str) -> Result<HashMap<u32, PaneTab>, String> {
        let mut cmd = Command::new("zellij");
        cmd.args([
            "--session",
            session,
            "action",
            "list-panes",
            "--json",
            "--tab",
        ]);
        exec::run(cmd, ZELLIJ_TIMEOUT).and_then(|out| parse_panes(&out.stdout))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_list() {
        let out =
            "main\nwork [Created 2h ago]\nold [Created 1d ago] (EXITED - attach to resurrect)\n\n";
        assert_eq!(parse_sessions(out), vec!["main", "work"]);
    }

    #[test]
    fn parses_attached_clients() {
        assert!(!parse_attached(
            "CLIENT_ID ZELLIJ_PANE_ID RUNNING_COMMAND\n"
        ));
        assert!(!parse_attached(""));
        assert!(parse_attached(
            "CLIENT_ID ZELLIJ_PANE_ID RUNNING_COMMAND\n1         terminal_0     claude\n"
        ));
    }

    #[test]
    fn parses_pane_tabs() {
        let out = r#"[
            {"id":0,"is_plugin":false,"title":"zsh","tab_id":0,"tab_position":0,"tab_name":"Tab #1"},
            {"id":0,"is_plugin":true,"title":"zj_herdr","tab_id":0,"tab_position":0,"tab_name":"Tab #1"},
            {"id":3,"is_plugin":false,"title":"claude","tab_id":4,"tab_position":1,"tab_name":"api"},
            {"id":5,"is_plugin":false,"title":"half a record"}
        ]"#;
        let panes = parse_panes(out).unwrap();
        assert_eq!(panes.len(), 2, "{panes:?}");
        assert_eq!(
            panes[&0],
            PaneTab {
                position: 0,
                name: "Tab #1".into()
            }
        );
        assert_eq!(panes[&3].name, "api");
        assert_eq!(panes[&3].position, 1);
        assert_eq!(parse_panes(r#"{"panes":[]}"#), Ok(HashMap::new()));
        assert!(parse_panes("There is no active session!").is_err());
    }

    #[test]
    fn builds_run_args() {
        let cmd = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "x".to_string(),
        ];
        assert_eq!(
            run_args("dev", Some("/w"), &cmd).join(" "),
            "--session dev run --cwd /w -- claude --resume x"
        );
        assert_eq!(
            run_args("dev", None, &cmd[..1]).join(" "),
            "--session dev run -- claude"
        );
    }

    #[test]
    fn builds_deliver_args() {
        assert_eq!(
            deliver_args("dev", "zj_herdr", "zj_herdr.cmd.v1", "-x").join(" "),
            "--session dev action pipe --plugin zj_herdr --plugin-configuration deliver_prompts=true --floating-plugin true --name zj_herdr.cmd.v1 -- -x"
        );
    }
}
