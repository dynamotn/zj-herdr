//! The few Zellij CLI calls the daemon makes, behind a trait for tests.

use std::process::Command;
use std::time::Duration;

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

/// Session names from `zellij list-sessions --short`, without exited ones.
pub fn parse_sessions(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains("EXITED"))
        .map(|l| l.split_whitespace().next().unwrap_or(l).to_string())
        .collect()
}

impl Zellij for ZellijCli {
    fn sessions(&self) -> Result<Vec<String>, String> {
        let mut cmd = Command::new("zellij");
        cmd.args(["list-sessions", "--short", "--no-formatting"]);
        match exec::run(cmd, ZELLIJ_TIMEOUT) {
            Ok(out) => Ok(parse_sessions(&out.stdout)),
            // Zellij exits non-zero when there are no sessions at all.
            Err(e) if e.contains("No active zellij sessions") => Ok(vec![]),
            Err(e) => Err(e),
        }
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
