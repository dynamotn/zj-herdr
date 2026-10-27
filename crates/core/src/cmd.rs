//! `zj_herdr.cmd.v1`: commands piped to the Zellij plugin.
//!
//! Keybinds send a bare word (`attention-next`); the daemon sends JSON
//! (`{"cmd":"prompt","pane_id":3,"text":"hi\r"}`). Both parse here.

use serde::{Deserialize, Serialize};

/// Pipe name for plugin commands.
pub const CMD_SCHEMA: &str = "zj_herdr.cmd.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum PluginCommand {
    /// Jump to the next agent that needs attention (blocked, then idle).
    AttentionNext,
    /// Jump to the previous agent that needs attention.
    AttentionPrev,
    /// Show or hide the picker.
    Toggle,
    /// Type `text` into a terminal pane of this session.
    Prompt { pane_id: u32, text: String },
}

impl PluginCommand {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("command serializes")
    }

    pub fn parse(payload: &str) -> Result<Self, String> {
        let payload = payload.trim();
        if payload.starts_with('{') {
            return serde_json::from_str(payload).map_err(|e| format!("invalid {CMD_SCHEMA} payload: {e}"));
        }
        match payload {
            "attention-next" => Ok(PluginCommand::AttentionNext),
            "attention-prev" => Ok(PluginCommand::AttentionPrev),
            "toggle" => Ok(PluginCommand::Toggle),
            other => Err(format!("unknown {CMD_SCHEMA} command {other:?}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_words_and_json_parse() {
        assert_eq!(
            PluginCommand::parse(" attention-next\n"),
            Ok(PluginCommand::AttentionNext)
        );
        assert_eq!(PluginCommand::parse("toggle"), Ok(PluginCommand::Toggle));
        assert_eq!(
            PluginCommand::parse(r#"{"cmd":"attention-prev"}"#),
            Ok(PluginCommand::AttentionPrev)
        );
        assert!(PluginCommand::parse("jump").is_err());
        assert!(PluginCommand::parse(r#"{"cmd":"prompt"}"#).is_err());
    }

    #[test]
    fn prompt_round_trips() {
        let cmd = PluginCommand::Prompt {
            pane_id: 3,
            text: "hi \"there\"\r".into(),
        };
        let json = cmd.to_json();
        assert!(json.starts_with(r#"{"cmd":"prompt""#));
        assert_eq!(PluginCommand::parse(&json), Ok(cmd));
    }
}
