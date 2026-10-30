//! `zj_herdr.cmd.v1`: commands piped to the Zellij plugin.
//!
//! Keybinds send a bare word (`attention-next`); the daemon sends JSON
//! (`{"cmd":"prompt","pane_id":3,"text":"hi\r"}`). Both parse here.

use serde::{Deserialize, Serialize};

/// Pipe name for plugin commands.
pub const CMD_SCHEMA: &str = "zj_herdr.cmd.v1";

/// What the plugin writes back down a CLI pipe once a prompt is typed. The
/// daemon reports success only when it reads this line.
pub const PROMPT_ACK: &str = "zj_herdr.prompt.delivered";
/// Prefix of the line the plugin writes back when it could not type a
/// prompt; the reason follows.
pub const PROMPT_NACK: &str = "zj_herdr.prompt.failed:";

/// Read a plugin's reply to a prompt from the pipe's output.
pub fn prompt_reply(output: &str) -> Result<(), String> {
    for line in output.lines().map(str::trim) {
        if line == PROMPT_ACK {
            return Ok(());
        }
        if let Some(reason) = line.strip_prefix(PROMPT_NACK) {
            return Err(reason.trim().to_string());
        }
    }
    Err("no zj-herdr plugin acknowledged the prompt".to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum PluginCommand {
    /// Jump to the next agent that needs attention (blocked, then idle).
    AttentionNext,
    /// Jump to the previous agent that needs attention.
    AttentionPrev,
    /// Show or hide the picker.
    Toggle,
    /// Move the cursor to the next agent row. Like `select-prev` and
    /// `jump`, only the instance whose own tab is active acts on it, so a
    /// keybind can drive the pinned rail of the tab in view.
    SelectNext,
    /// Move the cursor to the previous agent row.
    SelectPrev,
    /// Jump to the agent under the cursor.
    Jump,
    /// Switch to the next Zellij session, by name.
    SessionNext,
    /// Switch to the previous Zellij session, by name.
    SessionPrev,
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
            return serde_json::from_str(payload)
                .map_err(|e| format!("invalid {CMD_SCHEMA} payload: {e}"));
        }
        match payload {
            "attention-next" => Ok(PluginCommand::AttentionNext),
            "attention-prev" => Ok(PluginCommand::AttentionPrev),
            "toggle" => Ok(PluginCommand::Toggle),
            "select-next" => Ok(PluginCommand::SelectNext),
            "select-prev" => Ok(PluginCommand::SelectPrev),
            "jump" => Ok(PluginCommand::Jump),
            "session-next" => Ok(PluginCommand::SessionNext),
            "session-prev" => Ok(PluginCommand::SessionPrev),
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
        assert!(PluginCommand::parse("leap").is_err());
        assert!(PluginCommand::parse(r#"{"cmd":"prompt"}"#).is_err());
    }

    #[test]
    fn rail_commands_parse() {
        for (word, cmd) in [
            ("select-next", PluginCommand::SelectNext),
            ("select-prev", PluginCommand::SelectPrev),
            ("jump", PluginCommand::Jump),
            ("session-next", PluginCommand::SessionNext),
            ("session-prev", PluginCommand::SessionPrev),
        ] {
            assert_eq!(PluginCommand::parse(&format!("{word}\n")), Ok(cmd.clone()));
            assert_eq!(cmd.to_json(), format!(r#"{{"cmd":"{word}"}}"#));
            assert_eq!(PluginCommand::parse(&cmd.to_json()), Ok(cmd));
        }
        assert!(PluginCommand::parse("select").is_err());
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

    #[test]
    fn prompt_replies_parse() {
        assert_eq!(prompt_reply(&format!("{PROMPT_ACK}\n")), Ok(()));
        assert_eq!(
            prompt_reply(&format!("{PROMPT_NACK} permission denied\n")),
            Err("permission denied".into())
        );
        assert!(prompt_reply("").is_err(), "silence is not delivery");
        assert_eq!(
            prompt_reply(&format!("noise\n{PROMPT_ACK}\n")),
            Ok(()),
            "unrelated lines are skipped"
        );
    }
}
