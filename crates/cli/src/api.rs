//! The socket protocol: one JSON request per line, JSON lines back.
//!
//! ```text
//! {"op":"list"}
//! {"op":"wait","agent":"box/123","state":"blocked","timeout":60}
//! {"op":"prompt","agent":"dev:3","text":"continue"}
//! {"op":"spawn","session":"dev","cwd":"/w","command":["claude"]}
//! {"op":"subscribe"}
//! ```
//!
//! Every request gets one [`Response`] line, except `subscribe`, which gets
//! a stream of [`Event`] lines.

use serde::{Deserialize, Serialize};
use zj_herdr_core::{Agent, AgentState, StateV1};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CommandLine {
    Argv(Vec<String>),
    /// Run through `sh -c`.
    Shell(String),
}

impl CommandLine {
    pub fn argv(&self) -> Vec<String> {
        match self {
            CommandLine::Argv(argv) => argv.clone(),
            CommandLine::Shell(line) => vec!["sh".into(), "-c".into(), line.clone()],
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    /// All agents, local and remote.
    List,
    /// Return once `agent` is in `state`, or fail after `timeout` seconds.
    Wait {
        agent: String,
        state: AgentState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<f64>,
    },
    /// Type `text` into the agent's pane, then Enter unless `submit` is false.
    Prompt {
        agent: String,
        text: String,
        #[serde(default = "yes")]
        submit: bool,
    },
    /// Open a new pane running `command` in a Zellij session.
    Spawn {
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        command: CommandLine,
    },
    /// Stream a snapshot, then every state change.
    Subscribe,
}

pub fn parse_request(line: &str) -> Result<Request, String> {
    let req: Request =
        serde_json::from_str(line.trim()).map_err(|e| format!("bad request: {e}"))?;
    match &req {
        Request::Wait {
            timeout: Some(t), ..
        } if !t.is_finite() || *t < 0.0 => {
            Err("bad request: timeout must be a non-negative number of seconds".into())
        }
        Request::Spawn { command, .. } if command.argv().is_empty() => {
            Err("bad request: empty command".into())
        }
        _ => Ok(req),
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<StateV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<Agent>,
}

impl Response {
    pub fn ok() -> Self {
        Response {
            ok: true,
            ..Default::default()
        }
    }

    pub fn error(msg: impl Into<String>) -> Self {
        Response {
            ok: false,
            error: Some(msg.into()),
            ..Default::default()
        }
    }

    pub fn into_result(self) -> Result<Self, String> {
        if self.ok {
            Ok(self)
        } else {
            Err(self.error.unwrap_or_else(|| "request failed".into()))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum Event {
    Snapshot {
        state: StateV1,
    },
    Change {
        agent: Agent,
        /// `None` when the agent just appeared.
        from: Option<AgentState>,
    },
}

pub fn to_line<T: Serialize>(value: &T) -> String {
    let mut line = serde_json::to_string(value).expect("protocol types serialize");
    line.push('\n');
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_op() {
        assert_eq!(parse_request(r#"{"op":"list"}"#), Ok(Request::List));
        assert_eq!(
            parse_request("{\"op\":\"subscribe\"}\n"),
            Ok(Request::Subscribe)
        );
        assert_eq!(
            parse_request(r#"{"op":"wait","agent":"box/1","state":"blocked","timeout":2.5}"#),
            Ok(Request::Wait {
                agent: "box/1".into(),
                state: AgentState::Blocked,
                timeout: Some(2.5)
            })
        );
        assert_eq!(
            parse_request(r#"{"op":"prompt","agent":"dev:3","text":"go"}"#),
            Ok(Request::Prompt {
                agent: "dev:3".into(),
                text: "go".into(),
                submit: true
            })
        );
        let spawn =
            parse_request(r#"{"op":"spawn","session":"dev","command":"claude --continue"}"#)
                .unwrap();
        let Request::Spawn {
            session,
            cwd,
            command,
        } = spawn
        else {
            panic!("not spawn")
        };
        assert_eq!((session.as_str(), cwd), ("dev", None));
        assert_eq!(command.argv(), vec!["sh", "-c", "claude --continue"]);
        let spawn = parse_request(r#"{"op":"spawn","session":"s","cwd":"/w","command":["codex"]}"#)
            .unwrap();
        assert!(
            matches!(spawn, Request::Spawn { command: CommandLine::Argv(ref a), .. } if a == &["codex"])
        );
    }

    #[test]
    fn rejects_bad_requests() {
        for line in [
            "",
            "list",
            r#"{"op":"fly"}"#,
            r#"{"op":"wait","agent":"a","state":"sleeping"}"#,
            r#"{"op":"wait","agent":"a","state":"idle","timeout":-1}"#,
            r#"{"op":"wait","state":"idle"}"#,
            r#"{"op":"spawn","session":"s","command":[]}"#,
        ] {
            assert!(parse_request(line).is_err(), "accepted {line:?}");
        }
    }

    #[test]
    fn requests_round_trip() {
        let req = Request::Wait {
            agent: "a".into(),
            state: AgentState::Idle,
            timeout: None,
        };
        assert_eq!(parse_request(&to_line(&req)), Ok(req));
    }

    #[test]
    fn responses_omit_empty_fields() {
        assert_eq!(to_line(&Response::ok()), "{\"ok\":true}\n");
        let err: Response = serde_json::from_str(&to_line(&Response::error("nope"))).unwrap();
        assert_eq!(err.into_result(), Err("nope".to_string()));
    }
}
