//! The table of known agent CLIs. Supporting a new agent is one line here.

use std::path::Path;

/// How to recognise an agent process and how to resume one of its sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentSpec {
    /// Family name reported as `Agent::kind`.
    pub kind: &'static str,
    /// Executable or script basenames that identify the agent.
    pub names: &'static [&'static str],
    /// Command that resumes a session; `{id}` is replaced by the session id.
    pub resume: Option<&'static [&'static str]>,
}

pub const KNOWN_AGENTS: &[AgentSpec] = &[
    AgentSpec {
        kind: "claude",
        names: &["claude", "claude-code"],
        resume: Some(&["claude", "--resume", "{id}"]),
    },
    AgentSpec {
        kind: "codex",
        names: &["codex", "codex-cli"],
        resume: Some(&["codex", "resume", "{id}"]),
    },
    AgentSpec {
        kind: "opencode",
        names: &["opencode"],
        resume: None,
    },
    AgentSpec {
        kind: "gemini",
        names: &["gemini", "gemini-cli"],
        resume: None,
    },
    AgentSpec {
        kind: "copilot",
        names: &["copilot"],
        resume: None,
    },
    AgentSpec {
        kind: "cursor-agent",
        names: &["cursor-agent"],
        resume: None,
    },
    AgentSpec {
        kind: "pi",
        names: &["pi"],
        resume: None,
    },
    AgentSpec {
        kind: "aider",
        names: &["aider"],
        resume: None,
    },
];

/// Interpreters that run agents shipped as scripts (`node .../bin/gemini`).
const INTERPRETERS: &[&str] = &["node", "nodejs", "bun", "deno", "python", "python3"];

pub fn spec(kind: &str) -> Option<&'static AgentSpec> {
    KNOWN_AGENTS.iter().find(|s| s.kind == kind)
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn by_basename(name: &str) -> Option<&'static AgentSpec> {
    KNOWN_AGENTS.iter().find(|s| s.names.contains(&name))
}

/// `2.1.3`, `0.45.0-beta.1`: a file named after a release, not a program.
fn is_version(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_digit())
        && name.contains('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+' | '_'))
}

/// Directory names that sit between an agent's own directory and a
/// versioned binary (`~/.local/share/claude/versions/2.1.3`).
const VERSION_DIRS: &[&str] = &["versions", "version", "bin"];

/// Match a basename, or for a versioned binary the agent directory above it.
fn by_name(path: &str) -> Option<&'static AgentSpec> {
    let name = basename(path);
    if let Some(spec) = by_basename(name) {
        return Some(spec);
    }
    if !is_version(name) {
        return None;
    }
    path.rsplit('/')
        .skip(1)
        .filter(|c| !c.is_empty())
        .find(|c| !VERSION_DIRS.contains(c))
        .and_then(by_basename)
}

fn is_interpreter(name: &str) -> bool {
    let name = basename(name);
    INTERPRETERS.iter().any(|i| {
        name == *i
            || name
                .strip_prefix(i)
                .is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit() || c == '.'))
    })
}

/// Whether the process is an interpreter running a script (`node .../codex`)
/// rather than a native binary. Such a script may hand off to a native
/// helper of its own kind.
pub fn is_script_launcher(name: &str, argv: &[String]) -> bool {
    is_interpreter(name) || argv.first().is_some_and(|a| is_interpreter(a))
}

/// Identify an agent from a process name, executable path and argv.
pub fn identify(name: &str, exe: Option<&str>, argv: &[String]) -> Option<&'static AgentSpec> {
    // A binary exec'd from a descriptor (`/proc/self/fd/3`), as Claude Code's
    // sandbox runs each shell command: its exe says nothing of what it is.
    if argv
        .first()
        .is_some_and(|a| a.starts_with("/proc/self/fd/"))
    {
        return None;
    }
    if let Some(spec) = by_name(name).or_else(|| exe.and_then(by_name)) {
        return Some(spec);
    }
    let first = argv.first()?;
    if let Some(spec) = by_name(first) {
        return Some(spec);
    }
    if is_interpreter(first) || is_interpreter(name) {
        // First non-flag argument is the script being run.
        let script = argv.iter().skip(1).find(|a| !a.starts_with('-'))?;
        return by_name(script);
    }
    None
}

/// Claude Code's own background machinery, never a session of its own:
/// `claude daemon run` and the pty hosts it keeps for background sessions.
const CLAUDE_MACHINERY: &[&str] = &["daemon", "bg-pty-host"];
/// A worker the Claude Code daemon starts ahead of time. It waits in the
/// daemon's `spare` directory until claimed, and is a background session after.
const CLAUDE_SPARE: &str = "bg-spare";

/// The word after the program name, as a subcommand: the daemon's
/// workers retitle themselves, so it may share `argv[0]` with it
/// (`"claude bg-spare"`), and newer releases pass it as a flag
/// (`claude --bg-pty-host <socket>`).
fn claude_subcommand<'a>(spec: &AgentSpec, argv: &'a [String]) -> Option<&'a str> {
    (spec.kind == "claude")
        .then(|| argv.iter().flat_map(|a| a.split_whitespace()).nth(1))
        .flatten()
        .map(|w| w.trim_start_matches("--"))
}

/// Whether the process hosts the pty of a Claude Code background session:
/// what runs under it is in no pane, whatever Zellij variables it has.
pub fn is_session_host(spec: &AgentSpec, argv: &[String]) -> bool {
    claude_subcommand(spec, argv) == Some("bg-pty-host")
}

/// Whether an agent-looking process is machinery rather than a session: the
/// Claude Code daemon, its pty hosts, and spare workers nobody claimed yet
/// (they still sit in the daemon's `spare` directory).
pub fn is_machinery(spec: &AgentSpec, argv: &[String], cwd: Option<&Path>) -> bool {
    match claude_subcommand(spec, argv) {
        Some(sub) if CLAUDE_MACHINERY.contains(&sub) => true,
        Some(CLAUDE_SPARE) => cwd.is_none_or(|c| c.ends_with("spare")),
        _ => false,
    }
}

/// Whether the process is a headless one-shot run (`claude -p ...`), as
/// hooks and plugins start: it answers one prompt and exits within seconds,
/// so it is no agent to watch. Long-lived headless sessions
/// (`--input-format stream-json`) are agents and are kept.
pub fn is_one_shot(spec: &AgentSpec, argv: &[String]) -> bool {
    // `argv[0]` may be retitled (`"claude bg-spare"`); the flags follow it.
    let mut words = argv
        .first()
        .into_iter()
        .flat_map(|a| a.split_whitespace())
        .skip(1)
        .chain(argv.iter().skip(1).map(String::as_str));
    spec.kind == "claude" && words.any(|w| w == "-p" || w == "--print")
}

/// Whether the process is a Claude run allowed no tools at all
/// (`--tools=` or `--tools ""`): a helper some plugin drives over the SDK,
/// such as a memory observer, never a coding agent.
pub fn is_toolless(spec: &AgentSpec, argv: &[String]) -> bool {
    spec.kind == "claude"
        && argv.iter().enumerate().any(|(i, a)| {
            a == "--tools=" || (a == "--tools" && argv.get(i + 1).is_some_and(|v| v.is_empty()))
        })
}

/// Whether the agent is a background session of the Claude Code daemon. It
/// runs in no pane, whatever Zellij variables it inherited from the pane that
/// started the daemon.
pub fn is_detached(spec: &AgentSpec, argv: &[String]) -> bool {
    claude_subcommand(spec, argv) == Some(CLAUDE_SPARE)
}

/// Agent session ids are uuids or similar. Anything else (in particular a
/// leading `-`, which the agent would read as a flag) is refused, since ids
/// come from file names and file contents.
pub fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The resume command for `kind` and `session_id`, if the agent supports one
/// and the id is well formed.
pub fn resume_command(kind: &str, session_id: &str) -> Option<Vec<String>> {
    if !is_valid_session_id(session_id) {
        return None;
    }
    let template = spec(kind)?.resume?;
    Some(
        template
            .iter()
            .map(|part| part.replace("{id}", session_id))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn identifies_native_binaries() {
        assert_eq!(
            identify("claude", None, &argv(&["claude"])).unwrap().kind,
            "claude"
        );
        assert_eq!(
            identify(
                "2.1.3",
                Some("/home/u/.local/share/claude/versions/2.1.3"),
                &argv(&["claude", "--resume"])
            )
            .unwrap()
            .kind,
            "claude"
        );
        assert_eq!(
            identify("codex", Some("/usr/bin/codex"), &[]).unwrap().kind,
            "codex"
        );
    }

    #[test]
    fn ignores_programs_executed_through_a_descriptor() {
        // Claude Code's sandbox runs each shell command as its own binary,
        // exec'd from fd 3: a command, not an agent.
        let exe = "/home/u/.local/share/mise/installs/claude/2.1.312/claude";
        let cmd = argv(&["/proc/self/fd/3", "/bin/bash", "-c", "sleep 5"]);
        assert!(identify("3", Some(exe), &cmd).is_none());
    }

    #[test]
    fn identifies_versioned_binaries_by_their_directory() {
        let exe = "/home/u/.local/share/claude/versions/2.1.3";
        assert_eq!(
            identify("2.1.3", Some(exe), &argv(&["2.1.3"]))
                .unwrap()
                .kind,
            "claude"
        );
        assert_eq!(
            identify("2.1.3", None, &argv(&[exe, "-c"])).unwrap().kind,
            "claude"
        );
        assert!(identify("2.1.3", Some("/opt/tool/versions/2.1.3"), &argv(&["2.1.3"])).is_none());
        assert!(identify("app", Some("/home/u/claude/bin/app"), &argv(&["app"])).is_none());
    }

    #[test]
    fn identifies_scripts_behind_interpreters() {
        let found = identify(
            "node",
            Some("/usr/bin/node"),
            &argv(&["node", "--no-warnings", "/opt/lib/bin/gemini"]),
        );
        assert_eq!(found.unwrap().kind, "gemini");
        let found = identify(
            "python3.12",
            None,
            &argv(&["python3.12", "/home/u/.local/bin/aider", "--model", "x"]),
        );
        assert_eq!(found.unwrap().kind, "aider");
    }

    #[test]
    fn ignores_unrelated_processes() {
        assert!(identify("node", None, &argv(&["node", "server.js"])).is_none());
        assert!(identify("vim", None, &argv(&["vim", "claude.md"])).is_none());
        assert!(identify("pip", None, &argv(&["pip"])).is_none());
    }

    #[test]
    fn one_shot_claude_runs_are_not_agents() {
        let claude = spec("claude").unwrap();
        assert!(is_one_shot(claude, &argv(&["claude", "-p", "hi"])));
        assert!(is_one_shot(
            claude,
            &argv(&["/opt/claude", "--model", "x", "--print"])
        ));
        assert!(is_one_shot(claude, &argv(&["claude bg-spare -p", "x"])));
        assert!(!is_one_shot(
            claude,
            &argv(&[
                "claude",
                "--output-format",
                "stream-json",
                "--input-format",
                "stream-json"
            ])
        ));
        assert!(!is_one_shot(claude, &argv(&["claude", "explain -p"])));
        assert!(!is_one_shot(claude, &argv(&["claude"])));
        assert!(!is_one_shot(
            spec("aider").unwrap(),
            &argv(&["aider", "-p"])
        ));
    }

    #[test]
    fn knows_claude_machinery_in_flag_form_and_toolless_helpers() {
        let claude = spec("claude").unwrap();
        let bin = "/opt/claude/2.1.312/claude";
        // Claude Code 2.1.312 passes the pty host as a flag.
        let host = argv(&[
            bin,
            "--bg-pty-host",
            "/t/pty/a.sock",
            "181",
            "49",
            "--",
            bin,
        ]);
        assert!(is_machinery(claude, &host, None));
        assert!(is_session_host(claude, &host));
        assert!(is_session_host(
            claude,
            &argv(&["claude bg-pty-host", "--bg-pty-host", "x"])
        ));
        assert!(!is_session_host(claude, &argv(&[bin, "--session-id", "s"])));
        let observer = argv(&[
            bin,
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--tools=",
            "--setting-sources=",
        ]);
        assert!(is_toolless(claude, &observer));
        assert!(is_toolless(claude, &argv(&[bin, "--tools", ""])));
        assert!(!is_toolless(claude, &argv(&[bin, "--tools", "Bash,Read"])));
        assert!(!is_toolless(claude, &argv(&[bin, "--tools=Bash"])));
        assert!(!is_toolless(
            spec("codex").unwrap(),
            &argv(&["codex", "--tools="])
        ));
    }

    #[test]
    fn builds_resume_commands() {
        assert_eq!(
            resume_command("claude", "abc").unwrap(),
            argv(&["claude", "--resume", "abc"])
        );
        assert_eq!(
            resume_command("codex", "r1").unwrap(),
            argv(&["codex", "resume", "r1"])
        );
        assert!(resume_command("aider", "x").is_none());
    }

    #[test]
    fn refuses_session_ids_that_could_be_flags() {
        for bad in [
            "--dangerously-skip-permissions",
            "-r",
            "",
            "a b",
            "a;b",
            "../x",
        ] {
            assert!(resume_command("claude", bad).is_none(), "accepted {bad:?}");
        }
        assert!(resume_command("codex", "0196a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b").is_some());
    }
}
