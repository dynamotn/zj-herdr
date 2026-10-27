//! Readers for the transcripts agents already write. Only the tail of a file
//! is read, and only the shape of each entry matters, never its text.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

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
        .filter_map(|path| {
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
        })
        .collect()
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

pub fn read_claude(path: &Path) -> Option<Transcript> {
    let lines = tail_lines(path).ok()?;
    Some(Transcript {
        path: path.to_path_buf(),
        session_id: stem(path),
        modified: modified_secs(path)?,
        last: claude_last_turn(parse_lines(&lines)),
    })
}

fn content_items(entry: &Value) -> &[Value] {
    entry["message"]["content"]
        .as_array()
        .map_or(&[], Vec::as_slice)
}

pub fn claude_last_turn(entries: impl Iterator<Item = Value>) -> LastTurn {
    let mut open: HashSet<String> = HashSet::new();
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
                            open.insert(id.to_string());
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
                        open.remove(id);
                    }
                    last = LastTurn::AwaitingReply;
                }
            }
            _ => {}
        }
    }
    if open.is_empty() {
        last
    } else {
        LastTurn::PendingTool
    }
}

// ---------------------------------------------------------------------- Codex

/// All `rollout-*.jsonl` files under the Codex sessions tree, newest first.
pub fn codex_rollouts(root: &Path) -> Vec<(u64, PathBuf)> {
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
                if let Some(m) = modified_secs(&path) {
                    found.push((m, path));
                }
            }
        }
    }
    found.sort_by(|a, b| b.cmp(a));
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
    Some(Transcript {
        path: path.to_path_buf(),
        session_id,
        modified: modified_secs(path)?,
        last: codex_last_turn(parse_lines(&lines)),
    })
}

pub fn codex_last_turn(entries: impl Iterator<Item = Value>) -> LastTurn {
    let mut open: HashSet<String> = HashSet::new();
    let mut last = LastTurn::Unknown;
    let mut approval = false;
    for entry in entries {
        let payload = &entry["payload"];
        let kind = payload["type"].as_str().unwrap_or_default();
        match entry["type"].as_str() {
            Some("response_item") => match kind {
                "function_call" | "custom_tool_call" | "local_shell_call" => {
                    if let Some(id) = payload["call_id"].as_str() {
                        open.insert(id.to_string());
                    }
                    last = LastTurn::AwaitingReply;
                }
                "function_call_output" | "custom_tool_call_output" => {
                    if let Some(id) = payload["call_id"].as_str() {
                        open.remove(id);
                    }
                    approval = false;
                    last = LastTurn::AwaitingReply;
                }
                "message" if payload["role"] == "assistant" => last = LastTurn::TurnEnded,
                "reasoning" => last = LastTurn::AwaitingReply,
                _ => {}
            },
            Some("event_msg") => match kind {
                "exec_approval_request" | "apply_patch_approval_request" => approval = true,
                "task_started" => last = LastTurn::AwaitingReply,
                "user_message" => {
                    open.clear();
                    approval = false;
                    last = LastTurn::AwaitingReply;
                }
                "task_complete" | "turn_aborted" => {
                    open.clear();
                    approval = false;
                    last = LastTurn::TurnEnded;
                }
                _ => {}
            },
            _ => {}
        }
    }
    if approval || !open.is_empty() {
        LastTurn::PendingTool
    } else {
        last
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
