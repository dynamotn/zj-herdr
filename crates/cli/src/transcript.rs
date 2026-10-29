//! Readers for the transcripts agents already write. Only the tail of a file
//! is read for state (token totals read each line once, as it is appended),
//! and only the shape of each entry matters, never message text: the one
//! exception is the name of a pending tool call and, for a shell tool, its
//! command (see [`describe_call`]).

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde_json::Value;
use zj_herdr_core::{Tokens, WAITING_MAX_CHARS};

/// Enough of a transcript tail to see where the last turn stands.
const TAIL_BYTES: u64 = 256 * 1024;

/// Where the conversation stands at the end of the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastTurn {
    /// The agent asked for a tool call that has no result yet.
    PendingTool,
    /// The agent finished its turn (or the user interrupted it).
    TurnEnded,
    /// A prompt or tool result is waiting for the agent's reply.
    AwaitingReply,
    /// No entry the reader recognises.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub path: PathBuf,
    pub session_id: String,
    /// Unix seconds of the last write.
    pub modified: u64,
    pub last: LastTurn,
    /// For [`LastTurn::PendingTool`], the most recent unanswered call, from
    /// [`describe_call`].
    pub pending: Option<String>,
}

/// Where the conversation stands, and the call it waits on if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub last: LastTurn,
    pub pending: Option<String>,
}

/// Tool calls asked for and not answered yet, oldest first, each with its
/// [`describe_call`] text.
#[derive(Default)]
struct OpenCalls(Vec<(String, String)>);

impl OpenCalls {
    fn open(&mut self, id: &str, desc: String) {
        self.close(id);
        self.0.push((id.to_string(), desc));
    }

    fn close(&mut self, id: &str) {
        self.0.retain(|(open, _)| open != id);
    }

    fn clear(&mut self) {
        self.0.clear();
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn latest(&self) -> Option<String> {
        self.0.last().map(|(_, desc)| desc.clone())
    }
}

/// Tool names whose command is worth showing: Claude's `Bash`, Codex's
/// shell tools.
const SHELL_TOOLS: &[&str] = &[
    "Bash",
    "shell",
    "shell_command",
    "exec_command",
    "local_shell",
    "container.exec",
];

/// What a pending call waits on, for [`zj_herdr_core::Agent::waiting`]:
/// `Bash: cargo test` for a shell tool, the bare tool name otherwise. One
/// line (control characters become spaces), at most
/// [`WAITING_MAX_CHARS`] characters.
pub fn describe_call(name: &str, command: Option<&str>) -> String {
    let command = command
        .map(str::trim)
        .filter(|c| !c.is_empty() && SHELL_TOOLS.contains(&name));
    let text = match command {
        Some(c) => format!("{name}: {c}"),
        None => name.to_string(),
    };
    let line: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if line.chars().count() <= WAITING_MAX_CHARS {
        return line;
    }
    let mut out: String = line.chars().take(WAITING_MAX_CHARS - 1).collect();
    out.push('…');
    out
}

/// A shell command from a tool's arguments: a string as is; an argv
/// (`["bash", "-lc", "make"]`) as the script it runs, or joined by spaces.
fn command_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => {
            let argv: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            match argv.as_slice() {
                [_, flag, script, ..] if matches!(*flag, "-c" | "-lc") => Some(script.to_string()),
                _ => Some(argv.join(" ")),
            }
        }
        _ => None,
    }
}

/// The command in a tool's arguments, under `command` or `cmd`. Codex
/// sends arguments as a JSON string.
fn args_command(args: &Value) -> Option<String> {
    let parsed;
    let args = match args {
        Value::String(s) => {
            parsed = serde_json::from_str::<Value>(s).ok()?;
            &parsed
        }
        other => other,
    };
    ["command", "cmd"]
        .iter()
        .find_map(|key| command_text(&args[*key]))
}

pub fn modified_secs(path: &Path) -> Option<u64> {
    let mtime = fs::metadata(path).ok()?.modified().ok()?;
    Some(mtime.duration_since(UNIX_EPOCH).ok()?.as_secs())
}

/// Complete lines from the last [`TAIL_BYTES`] of a file.
fn tail_lines(path: &Path) -> std::io::Result<Vec<String>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // cut mid-line by the seek
    }
    Ok(lines)
}

fn parse_lines(lines: &[String]) -> impl Iterator<Item = Value> + '_ {
    lines.iter().filter_map(|l| serde_json::from_str(l).ok())
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// A transcript file and when it was created and last written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTimes {
    pub path: PathBuf,
    /// Unix seconds of creation; the mtime where the filesystem has no
    /// birth time.
    pub created: u64,
    pub modified: u64,
}

/// Files in `dir` (non-recursive) with the given extension.
pub fn files_with_times(dir: &Path, ext: &str) -> Vec<FileTimes> {
    let Ok(entries) = fs::read_dir(dir) else {
        return vec![];
    };
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == ext))
        .filter_map(file_times)
        .collect()
}

fn file_times(path: PathBuf) -> Option<FileTimes> {
    let meta = fs::metadata(&path).ok()?;
    let secs = |t: std::io::Result<std::time::SystemTime>| {
        t.ok()?.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs())
    };
    let modified = secs(meta.modified())?;
    let created = secs(meta.created()).unwrap_or(modified).min(modified);
    Some(FileTimes {
        path,
        created,
        modified,
    })
}

// ---------------------------------------------------------------- Claude Code

/// The plan's project directory name: the cwd with `/` and `.` replaced by
/// `-`.
pub fn claude_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// Claude Code itself also replaces every other character that is not ASCII
/// alphanumeric (`_`, spaces, ...); [`claude_project_dir`] falls back to it.
pub fn claude_slug_alnum(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// `<projects>/<slug>`, trying [`claude_slug`] first and then
/// [`claude_slug_alnum`] for cwds where the two differ.
pub fn claude_project_dir(projects: &Path, cwd: &Path) -> PathBuf {
    let plain = projects.join(claude_slug(cwd));
    if plain.is_dir() {
        return plain;
    }
    let alnum = projects.join(claude_slug_alnum(cwd));
    if alnum.is_dir() {
        alnum
    } else {
        plain
    }
}

/// The only fields read from Claude Code's `<config>/sessions/<pid>.json`;
/// the parser skips everything else in the file.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionFile {
    pid: u32,
    session_id: String,
    cwd: PathBuf,
    started_at: u64,
}

/// Larger than any real session file; a bigger one is not parsed.
const SESSION_FILE_MAX: u64 = 64 * 1024;
/// `startedAt` (milliseconds, from the agent) and the process start time
/// (whole seconds, from the OS) never quite agree.
const SESSION_FILE_SLACK_SECS: u64 = 5;

/// Session id and cwd Claude Code recorded for process `pid` in
/// `<sessions>/<pid>.json`. `None` without such a file, when it was written
/// for an earlier process with the same pid, or when its id is malformed.
pub fn claude_session_file(
    sessions: &Path,
    pid: u32,
    start_time: u64,
) -> Option<(String, PathBuf)> {
    let file = File::open(sessions.join(format!("{pid}.json"))).ok()?;
    let rec: SessionFile = serde_json::from_reader(file.take(SESSION_FILE_MAX)).ok()?;
    let fresh = rec.started_at / 1000 + SESSION_FILE_SLACK_SECS >= start_time;
    (rec.pid == pid && fresh && crate::agents::is_valid_session_id(&rec.session_id))
        .then_some((rec.session_id, rec.cwd))
}

/// The transcript of session `id` started in `cwd`:
/// `<projects>/<slug>/<id>.jsonl`, else the same file name in another
/// project directory (a session that entered a worktree), else the first
/// path, not written yet.
pub fn claude_session_transcript(projects: &Path, cwd: &Path, id: &str) -> PathBuf {
    let name = format!("{id}.jsonl");
    let home = claude_project_dir(projects, cwd).join(&name);
    if home.is_file() {
        return home;
    }
    fs::read_dir(projects)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path().join(&name))
        .find(|p| p.is_file())
        .unwrap_or(home)
}

pub fn read_claude(path: &Path) -> Option<Transcript> {
    let lines = tail_lines(path).ok()?;
    let turn = claude_turn(parse_lines(&lines));
    Some(Transcript {
        path: path.to_path_buf(),
        session_id: stem(path),
        modified: modified_secs(path)?,
        last: turn.last,
        pending: turn.pending,
    })
}

/// The usage counts of one transcript line; every other field (message
/// text included) is skipped by the parser.
#[derive(Deserialize)]
struct UsageLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    message: Option<UsageMessage>,
}

#[derive(Deserialize)]
struct UsageMessage {
    id: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

/// How far a transcript's token totals have been read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageCursor {
    /// `(dev, inode)` of the file read so far.
    identity: Option<(u64, u64)>,
    /// Bytes read, always at the end of a complete line.
    offset: u64,
    /// The last reply counted, and what it added: Claude Code writes one
    /// line per content block of a reply, each with the reply's whole usage.
    last: Option<(String, Tokens)>,
    pub tokens: Tokens,
}

fn add(a: &Tokens, b: &Tokens) -> Tokens {
    Tokens {
        input: a.input.saturating_add(b.input),
        output: a.output.saturating_add(b.output),
        cache_read: a.cache_read.saturating_add(b.cache_read),
        cache_write: a.cache_write.saturating_add(b.cache_write),
    }
}

fn sub(a: &Tokens, b: &Tokens) -> Tokens {
    Tokens {
        input: a.input.saturating_sub(b.input),
        output: a.output.saturating_sub(b.output),
        cache_read: a.cache_read.saturating_sub(b.cache_read),
        cache_write: a.cache_write.saturating_sub(b.cache_write),
    }
}

/// Bring `cursor` up to date with the end of the Claude transcript at
/// `path`, summing `message.usage` over the `assistant` lines appended since
/// the last call, each reply (`message.id`) once, at its latest usage. A trailing line without its newline is left for the next
/// call; a replaced or truncated file is read again from the start.
pub fn claude_usage(path: &Path, cursor: &mut UsageCursor) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let file = File::open(path)?;
    let meta = file.metadata()?;
    let identity = Some((meta.dev(), meta.ino()));
    if cursor.identity != identity || meta.len() < cursor.offset {
        *cursor = UsageCursor {
            identity,
            ..UsageCursor::default()
        };
    }
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(cursor.offset))?;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 || line.last() != Some(&b'\n') {
            return Ok(());
        }
        cursor.offset += n as u64;
        let Ok(UsageLine {
            kind: Some(kind),
            message: Some(UsageMessage { id, usage: Some(u) }),
        }) = serde_json::from_slice(&line)
        else {
            continue;
        };
        if kind != "assistant" {
            continue;
        }
        let reply = Tokens {
            input: u.input_tokens,
            output: u.output_tokens,
            cache_read: u.cache_read_input_tokens,
            cache_write: u.cache_creation_input_tokens,
        };
        // Another block of the reply just counted: replace its usage.
        if let (Some(id), Some((last_id, counted))) = (&id, &cursor.last) {
            if id == last_id {
                cursor.tokens = sub(&cursor.tokens, counted);
            }
        }
        cursor.tokens = add(&cursor.tokens, &reply);
        cursor.last = id.map(|id| (id, reply));
    }
}

fn content_items(entry: &Value) -> &[Value] {
    entry["message"]["content"]
        .as_array()
        .map_or(&[], Vec::as_slice)
}

pub fn claude_last_turn(entries: impl Iterator<Item = Value>) -> LastTurn {
    claude_turn(entries).last
}

pub fn claude_turn(entries: impl Iterator<Item = Value>) -> Turn {
    let mut open = OpenCalls::default();
    let mut last = LastTurn::Unknown;
    for entry in entries {
        // Subagent (Task tool) traffic does not end or block the main turn.
        if entry["isSidechain"].as_bool() == Some(true) {
            continue;
        }
        match entry["type"].as_str() {
            Some("assistant") => {
                for item in content_items(&entry) {
                    if item["type"] == "tool_use" {
                        if let Some(id) = item["id"].as_str() {
                            let name = item["name"].as_str().unwrap_or("tool");
                            let command = item["input"]["command"].as_str();
                            open.open(id, describe_call(name, command));
                        }
                    }
                }
                last = LastTurn::TurnEnded;
            }
            Some("user") => {
                let items = content_items(&entry);
                let results: Vec<&str> = items
                    .iter()
                    .filter(|i| i["type"] == "tool_result")
                    .filter_map(|i| i["tool_use_id"].as_str())
                    .collect();
                if results.is_empty() {
                    // A fresh prompt abandons any tool call still open.
                    open.clear();
                    let text = match &entry["message"]["content"] {
                        Value::String(s) => s.clone(),
                        _ => items.iter().filter_map(|i| i["text"].as_str()).collect(),
                    };
                    last = if text.starts_with("[Request interrupted by user") {
                        LastTurn::TurnEnded
                    } else {
                        LastTurn::AwaitingReply
                    };
                } else {
                    for id in results {
                        open.close(id);
                    }
                    last = LastTurn::AwaitingReply;
                }
            }
            _ => {}
        }
    }
    if open.is_empty() {
        Turn {
            last,
            pending: None,
        }
    } else {
        Turn {
            last: LastTurn::PendingTool,
            pending: open.latest(),
        }
    }
}

// ---------------------------------------------------------------------- Codex

/// All `rollout-*.jsonl` files under the Codex sessions tree, most recently
/// written first.
pub fn codex_rollouts(root: &Path) -> Vec<FileTimes> {
    let mut found = Vec::new();
    let mut dirs = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = dirs.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() && depth < 4 {
                dirs.push((path, depth + 1));
            } else if kind.is_file()
                && path.extension().is_some_and(|e| e == "jsonl")
                && path
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("rollout-"))
            {
                found.extend(file_times(path));
            }
        }
    }
    found.sort_by(|a, b| (b.modified, &b.path).cmp(&(a.modified, &a.path)));
    found
}

/// Session id and cwd from a rollout's first line (`session_meta`).
pub fn codex_meta(path: &Path) -> Option<(String, Option<String>)> {
    let mut first = String::new();
    BufReader::new(File::open(path).ok()?)
        .read_line(&mut first)
        .ok()?;
    let v: Value = serde_json::from_str(&first).ok()?;
    let meta = if v["type"] == "session_meta" {
        &v["payload"]
    } else {
        &v
    };
    let id = meta["id"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| codex_id_from_name(path));
    Some((id, meta["cwd"].as_str().map(str::to_owned)))
}

/// `rollout-2025-01-01T10-00-00-<uuid>.jsonl` ends in a 36-character uuid.
fn codex_id_from_name(path: &Path) -> String {
    let stem = stem(path);
    match stem.len().checked_sub(36) {
        Some(i) if stem.is_char_boundary(i) => stem[i..].to_string(),
        _ => stem,
    }
}

pub fn read_codex(path: &Path, session_id: String) -> Option<Transcript> {
    let lines = tail_lines(path).ok()?;
    let turn = codex_turn(parse_lines(&lines));
    Some(Transcript {
        path: path.to_path_buf(),
        session_id,
        modified: modified_secs(path)?,
        last: turn.last,
        pending: turn.pending,
    })
}

pub fn codex_last_turn(entries: impl Iterator<Item = Value>) -> LastTurn {
    codex_turn(entries).last
}

/// [`describe_call`] for a Codex `response_item` that requests a tool.
fn codex_call(payload: &Value) -> String {
    if payload["type"] == "local_shell_call" {
        let command = command_text(&payload["action"]["command"]);
        return describe_call("shell", command.as_deref());
    }
    let name = payload["name"].as_str().unwrap_or("tool");
    let command = args_command(&payload["arguments"]);
    describe_call(name, command.as_deref())
}

pub fn codex_turn(entries: impl Iterator<Item = Value>) -> Turn {
    let mut open = OpenCalls::default();
    let mut last = LastTurn::Unknown;
    // An approval request, with what it asks to run.
    let mut approval: Option<String> = None;
    for entry in entries {
        let payload = &entry["payload"];
        let kind = payload["type"].as_str().unwrap_or_default();
        match entry["type"].as_str() {
            Some("response_item") => match kind {
                "function_call" | "custom_tool_call" | "local_shell_call" => {
                    if let Some(id) = payload["call_id"].as_str() {
                        open.open(id, codex_call(payload));
                    }
                    last = LastTurn::AwaitingReply;
                }
                "function_call_output" | "custom_tool_call_output" => {
                    if let Some(id) = payload["call_id"].as_str() {
                        open.close(id);
                    }
                    approval = None;
                    last = LastTurn::AwaitingReply;
                }
                "message" if payload["role"] == "assistant" => last = LastTurn::TurnEnded,
                "reasoning" => last = LastTurn::AwaitingReply,
                _ => {}
            },
            Some("event_msg") => match kind {
                "exec_approval_request" => {
                    let command = command_text(&payload["command"]);
                    approval = Some(describe_call("shell", command.as_deref()));
                }
                "apply_patch_approval_request" => {
                    approval = Some(describe_call("apply_patch", None))
                }
                "task_started" => last = LastTurn::AwaitingReply,
                "user_message" => {
                    open.clear();
                    approval = None;
                    last = LastTurn::AwaitingReply;
                }
                "task_complete" | "turn_aborted" => {
                    open.clear();
                    approval = None;
                    last = LastTurn::TurnEnded;
                }
                _ => {}
            },
            _ => {}
        }
    }
    if approval.is_some() || !open.is_empty() {
        Turn {
            last: LastTurn::PendingTool,
            pending: open.latest().or(approval),
        }
    } else {
        Turn {
            last,
            pending: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slug_replaces_slashes_and_dots() {
        assert_eq!(
            claude_slug(Path::new("/Users/me/.config/my_app")),
            "-Users-me--config-my_app"
        );
        assert_eq!(
            claude_slug_alnum(Path::new("/Users/me/.config/my_app")),
            "-Users-me--config-my-app"
        );
    }

    #[test]
    fn project_dir_falls_back_to_claude_own_slug() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = Path::new("/w/my_app");
        assert_eq!(
            claude_project_dir(tmp.path(), cwd),
            tmp.path().join("-w-my_app")
        );
        fs::create_dir(tmp.path().join("-w-my-app")).unwrap();
        assert_eq!(
            claude_project_dir(tmp.path(), cwd),
            tmp.path().join("-w-my-app")
        );
        fs::create_dir(tmp.path().join("-w-my_app")).unwrap();
        assert_eq!(
            claude_project_dir(tmp.path(), cwd),
            tmp.path().join("-w-my_app")
        );
    }

    #[test]
    fn claude_parallel_tools_stay_pending_until_all_return() {
        let entries = vec![
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"a"},{"type":"tool_use","id":"b"}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"a"}]}}),
        ];
        assert_eq!(
            claude_last_turn(entries.clone().into_iter()),
            LastTurn::PendingTool
        );
        let mut done = entries;
        done.push(
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"b"}]}}),
        );
        assert_eq!(claude_last_turn(done.into_iter()), LastTurn::AwaitingReply);
    }

    #[test]
    fn claude_new_prompt_abandons_open_tools() {
        let entries = vec![
            json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"a"}]}}),
            json!({"type":"user","message":{"content":"never mind"}}),
        ];
        assert_eq!(
            claude_last_turn(entries.into_iter()),
            LastTurn::AwaitingReply
        );
    }

    #[test]
    fn claude_ignores_sidechains_and_meta() {
        let entries = vec![
            json!({"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}),
            json!({"type":"assistant","isSidechain":true,"message":{"content":[{"type":"tool_use","id":"s"}]}}),
            json!({"type":"file-history-snapshot"}),
        ];
        assert_eq!(claude_last_turn(entries.into_iter()), LastTurn::TurnEnded);
    }

    fn append(path: &Path, text: &str) {
        use std::io::Write;
        let mut f = File::options()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(text.as_bytes()).unwrap();
    }

    const REPLY: &str = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100,"cache_creation_input_tokens":7}}}"#;

    fn tokens(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Tokens {
        Tokens {
            input,
            output,
            cache_read,
            cache_write,
        }
    }

    #[test]
    fn claude_usage_sums_replies_incrementally() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        append(&path, &format!("{REPLY}\n"));
        // Only assistant lines count; missing counts are zero.
        append(
            &path,
            r#"{"type":"user","message":{"content":"x","usage":{"input_tokens":999}}}
{"type":"assistant","message":{"usage":{"output_tokens":1}}}
{"type":"summary","summary":"x"}
"#,
        );
        let mut cursor = UsageCursor::default();
        claude_usage(&path, &mut cursor).unwrap();
        assert_eq!(cursor.tokens, tokens(10, 6, 100, 7));
        let offset = cursor.offset;

        // A line still being written waits for its newline.
        let (head, tail) = REPLY.split_at(40);
        append(&path, head);
        claude_usage(&path, &mut cursor).unwrap();
        assert_eq!(
            (cursor.tokens, cursor.offset),
            (tokens(10, 6, 100, 7), offset)
        );
        append(&path, &format!("{tail}\n"));
        claude_usage(&path, &mut cursor).unwrap();
        assert_eq!(cursor.tokens, tokens(20, 11, 200, 14));
    }

    #[test]
    fn claude_usage_counts_each_reply_once() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        // One reply written as three blocks, the last with the final output
        // count, then another reply.
        let block = |id: &str, out: u64| {
            format!(
                r#"{{"type":"assistant","message":{{"id":"{id}","usage":{{"input_tokens":10,"output_tokens":{out},"cache_read_input_tokens":100,"cache_creation_input_tokens":7}}}}}}"#
            )
        };
        append(&path, &format!("{}\n{}\n", block("a", 1), block("a", 3)));
        let mut cursor = UsageCursor::default();
        claude_usage(&path, &mut cursor).unwrap();
        assert_eq!(cursor.tokens, tokens(10, 3, 100, 7));
        // A block of the same reply in a later read still replaces it.
        append(&path, &format!("{}\n{}\n", block("a", 5), block("b", 2)));
        claude_usage(&path, &mut cursor).unwrap();
        assert_eq!(cursor.tokens, tokens(20, 7, 200, 14));
    }

    #[test]
    fn claude_usage_starts_over_on_a_new_or_shorter_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.jsonl");
        append(&path, &format!("{REPLY}\n{REPLY}\n"));
        let mut cursor = UsageCursor::default();
        claude_usage(&path, &mut cursor).unwrap();
        assert_eq!(cursor.tokens.input, 20);

        // Truncated in place: same inode, fewer bytes.
        fs::write(&path, format!("{REPLY}\n")).unwrap();
        claude_usage(&path, &mut cursor).unwrap();
        assert_eq!(cursor.tokens.input, 10);

        // Replaced by another file of the same length.
        let other = tmp.path().join("other.jsonl");
        let reply = REPLY.replace("\"input_tokens\":10", "\"input_tokens\":30");
        append(&other, &format!("{reply}\n"));
        let keep = File::open(&path).unwrap(); // the old inode stays in use
        fs::rename(&other, &path).unwrap();
        claude_usage(&path, &mut cursor).unwrap();
        assert_eq!(cursor.tokens.input, 30);
        drop(keep);
    }

    #[test]
    fn codex_id_falls_back_to_file_name() {
        let p =
            Path::new("/x/rollout-2025-05-01T10-00-00-0196a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b.jsonl");
        assert_eq!(
            codex_id_from_name(p),
            "0196a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b"
        );
    }

    #[test]
    fn calls_are_described_on_one_short_line() {
        assert_eq!(
            describe_call("Bash", Some(" cargo test ")),
            "Bash: cargo test"
        );
        assert_eq!(describe_call("Edit", Some("ignored")), "Edit");
        assert_eq!(describe_call("Bash", Some("")), "Bash");
        assert_eq!(
            describe_call("Bash", Some("a\nb\r\x1b[31mc")),
            "Bash: a b  [31mc"
        );
        let long = describe_call("Bash", Some(&"x".repeat(100)));
        assert_eq!(long.chars().count(), WAITING_MAX_CHARS);
        assert!(long.starts_with("Bash: xxx") && long.ends_with('…'));
    }

    #[test]
    fn claude_pending_call_is_the_latest_unanswered() {
        let entries = vec![
            json!({"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"a","name":"Bash","input":{"command":"ls"}},
                {"type":"tool_use","id":"b","name":"Read","input":{"file_path":"/x"}}]}}),
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"b"}]}}),
        ];
        let turn = claude_turn(entries.clone().into_iter());
        assert_eq!(turn.last, LastTurn::PendingTool);
        assert_eq!(turn.pending.as_deref(), Some("Bash: ls"));
        let mut done = entries;
        done.push(
            json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"a"}]}}),
        );
        assert_eq!(claude_turn(done.into_iter()).pending, None);
    }

    #[test]
    fn codex_shell_calls_show_their_command() {
        let call = |payload: Value| {
            codex_turn(std::iter::once(
                json!({"type":"response_item","payload":payload}),
            ))
            .pending
            .unwrap()
        };
        assert_eq!(
            call(json!({"type":"function_call","name":"shell","call_id":"1",
                "arguments":"{\"command\":[\"bash\",\"-lc\",\"make test\"]}"})),
            "shell: make test"
        );
        assert_eq!(
            call(
                json!({"type":"function_call","name":"exec_command","call_id":"1",
                "arguments":"{\"cmd\":\"git status\"}"})
            ),
            "exec_command: git status"
        );
        assert_eq!(
            call(json!({"type":"local_shell_call","call_id":"1",
                "action":{"type":"exec","command":["rg","-n","x"]}})),
            "shell: rg -n x"
        );
        assert_eq!(
            call(
                json!({"type":"custom_tool_call","name":"apply_patch","call_id":"1","input":"*** Begin"})
            ),
            "apply_patch"
        );
    }

    #[test]
    fn codex_approval_request_blocks_until_output() {
        let mut entries = vec![
            json!({"type":"response_item","payload":{"type":"function_call","call_id":"c1"}}),
            json!({"type":"event_msg","payload":{"type":"exec_approval_request"}}),
        ];
        assert_eq!(
            codex_last_turn(entries.clone().into_iter()),
            LastTurn::PendingTool
        );
        entries.push(json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"c1"}}));
        assert_eq!(
            codex_last_turn(entries.into_iter()),
            LastTurn::AwaitingReply
        );
    }
}
