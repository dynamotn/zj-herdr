//! `~/.config/zj-herdr/config.toml`.
//!
//! ```toml
//! [[remote]]
//! name = "l30n4rd0d4v1nc1"
//! ssh = "l30n4rd0d4v1nc1"   # a Host alias from ~/.ssh/config
//! ```
//!
//! Only SSH targets live here. Authentication is the user's own SSH config
//! and agent; zj-herdr never reads keys and the config holds no credentials.

use std::path::Path;

use serde::Deserialize;

/// Matches the daemon's local rescan closely enough without hammering SSH.
pub const DEFAULT_REMOTE_INTERVAL_SECS: u64 = 5;
/// A scan takes a fraction of a second; longer means the host is not there.
pub const DEFAULT_REMOTE_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Remote {
    /// Machine name shown in lists and used in agent ids.
    pub name: String,
    /// Target handed to `ssh`, typically a `Host` alias.
    pub ssh: String,
    /// zj-herdr on the remote side, if not on its non-interactive PATH.
    #[serde(default = "default_command")]
    pub command: String,
    #[serde(default = "default_interval")]
    pub interval_secs: u64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_command() -> String {
    "zj-herdr".into()
}
fn default_interval() -> u64 {
    DEFAULT_REMOTE_INTERVAL_SECS
}
fn default_timeout() -> u64 {
    DEFAULT_REMOTE_TIMEOUT_SECS
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub remote: Vec<Remote>,
}

impl Config {
    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        let mut seen = std::collections::HashSet::new();
        for r in &config.remote {
            if r.name.is_empty() || r.name.contains('/') {
                return Err(format!("remote name {:?} must be non-empty and contain no '/'", r.name));
            }
            if !seen.insert(r.name.as_str()) {
                return Err(format!("remote name {:?} is used twice", r.name));
            }
            // A leading '-' would be read by ssh as an option.
            if r.ssh.is_empty() || r.ssh.starts_with('-') || r.ssh.chars().any(char::is_whitespace) {
                return Err(format!("remote {:?}: ssh target {:?} is not a host", r.name, r.ssh));
            }
            if r.interval_secs == 0 || r.timeout_secs == 0 {
                return Err(format!(
                    "remote {:?}: interval_secs and timeout_secs must be positive",
                    r.name
                ));
            }
        }
        Ok(config)
    }

    /// A missing file is an empty config.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remotes_with_defaults() {
        let config = Config::parse(
            r#"
            [[remote]]
            name = "l30n4rd0d4v1nc1"
            ssh = "l30n4rd0d4v1nc1"

            [[remote]]
            name = "build"
            ssh = "me@build.example"
            command = "/opt/bin/zj-herdr"
            interval_secs = 30
            "#,
        )
        .unwrap();
        assert_eq!(config.remote.len(), 2);
        assert_eq!(config.remote[0].command, "zj-herdr");
        assert_eq!(config.remote[0].interval_secs, DEFAULT_REMOTE_INTERVAL_SECS);
        assert_eq!(config.remote[1].command, "/opt/bin/zj-herdr");
        assert_eq!(config.remote[1].interval_secs, 30);
        assert_eq!(Config::parse("").unwrap(), Config::default());
    }

    #[test]
    fn rejects_unsafe_or_ambiguous_remotes() {
        for bad in [
            "[[remote]]\nname = \"a\"\nssh = \"-oProxyCommand=evil\"",
            "[[remote]]\nname = \"a\"\nssh = \"host other\"",
            "[[remote]]\nname = \"a/b\"\nssh = \"h\"",
            "[[remote]]\nname = \"a\"\nssh = \"h\"\n[[remote]]\nname = \"a\"\nssh = \"g\"",
            "[[remote]]\nname = \"a\"\nssh = \"h\"\npassword = \"x\"",
            "[[remote]]\nname = \"a\"\nssh = \"h\"\ntimeout_secs = 0",
        ] {
            assert!(Config::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn missing_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(Config::load(&tmp.path().join("none.toml")).unwrap(), Config::default());
    }
}
