# How detection works

`zj-herdr scan` looks for processes of known agents (`claude`, `codex`,
`opencode`, `gemini`, `copilot`, `cursor-agent`, `pi`, `aider`; the table is
`crates/cli/src/agents.rs`), reads `ZELLIJ_SESSION_NAME` and `ZELLIJ_PANE_ID`
from the process environment (or its nearest ancestor's) to find the pane,
and derives a state. A process counts as an agent when its name, its
executable or its first argument is a known name, including a versioned
binary (`.../claude/versions/2.1.3`) and a script run by an interpreter
(`node .../bin/gemini`, `python3 .../aider`).

What is not listed:

- Programs exec'd from a file descriptor (`/proc/self/fd/N`), as Claude Code
  runs each command of its sandboxed shell: they are commands, not agents.
- Claude Code's own background machinery: `claude daemon`, its `bg-pty-host`
  processes, and `bg-spare` workers still waiting in the daemon's `spare`
  directory. A claimed `bg-spare` is a background session and is listed
  outside Zellij, whatever pane variables it inherited from the daemon; so is
  any session that runs under a `bg-pty-host`.
- Headless one-shot runs (`claude -p`/`--print`, as hooks and plugins start
  them), whatever other flags they carry. Headless sessions started without
  `-p` (`--input-format stream-json`) are listed.
- Claude runs allowed no tools (`--tools=` or `--tools ""`): helpers that
  plugins drive over the SDK.
- A process of an agent's own kind directly below a script launcher of that
  kind (Codex's node launcher and its native binary): the two are one agent.
  An agent started by another agent's shell tool is listed on its own.

| State | Rule |
|---|---|
| `blocked` | the transcript ends with a tool call that has no result, and both the transcript and the agent's process tree have been quiet for 5 s |
| `working` | the transcript was written in the last 5 s, or the process tree (agent plus the tools it runs) uses 2% of a core or more; also a pending tool call that is not yet quiet |
| `idle` | the turn ended (or was interrupted) and everything is quiet; also an agent stopped mid-turn |
| `done` | the process exited; kept in the list for 10 s. An agent that ran less than 30 s in all was a helper and is dropped as soon as it exits (`wait <agent> done` already waiting on it still returns) |

An idle Claude agent whose turn ended while background work it started (a
shell or subagent, but not a dev server or other service) still runs counts
as `working`: each finished task wakes it again. When its last reply was an
API error and it is idle, the agent carries `error`. An agent whose
transcript is not found, or whose kind has no transcript reader, uses the
process signals alone.

Transcripts read:

- Claude Code: the session Claude records for the process in
  `~/.claude/sessions/<pid>.json` (only its `pid`, `sessionId`, `cwd`,
  `startedAt`, `kind`, `spare` and `name` are read; a file left by an earlier
  process with the same pid is ignored), whose transcript is
  `~/.claude/projects/<cwd-slug>/<sessionId>.jsonl` or, for a session that
  moved into a worktree, the same file in another project directory. Without
  a session file: the newest `~/.claude/projects/<cwd-slug>/*.jsonl`, where
  the slug is the cwd with `/` and `.` replaced by `-` (falling back to
  Claude's own form, which also replaces `_` and other symbols), and
  `--resume <id>`, `-r <id>` or `--session-id <id>` picks that file.
  `$CLAUDE_CONFIG_DIR` is honoured. When several Claude agents share a cwd,
  each remaining transcript goes to the process that was running when it was
  created, so two agents never report the same session.
- Codex: the newest `~/.codex/sessions/**/rollout-*.jsonl` (up to four
  directories deep) whose first line records the process cwd (`$CODEX_HOME`
  is honoured; `codex resume <id>` picks that one). Besides a pending tool
  call, an approval request (a command or a patch) that has no answer counts
  as `blocked`.
- Other agents: CPU only, so they show `working` or `idle`, never `blocked`.

Only the last 256 KiB of a transcript is read for state, so a background
task launched before that is not seen.

For Claude Code the session's token use is sent as `tokens` (`input`,
`output`, `cache_read`, `cache_write`): the sums of `message.usage` over the
transcript's `assistant` entries, each reply once. Claude Code writes a reply
as one entry per content block, each carrying the reply's whole usage, so
consecutive entries with the same `message.id` count as one, at the usage of
the last. Each line is parsed once, as it is appended; a transcript that is
replaced or shrinks is summed again from the start. The same pass gives the
`model` of the last real reply (an API error is not one) and `context`: the
percent of the context window that reply's prompt filled (input plus cache
reads and writes, against 200k tokens, or 1M once past 200k). Codex and the
other agents report no tokens, model or context.

Only the shape of each entry is read, never message text, with these
exceptions, each sent as one line of at most 60 characters:

- For a `blocked` agent, the name of the tool call it waits on and, for a
  shell tool (Claude `Bash`, Codex `shell`/`exec_command` and the like), its
  command, or the command of a Codex approval request. That is `waiting`,
  absent in other states, shown by the plugin under the agent as `↳ Bash:
  cargo test`.
- For Claude, the background tasks (`tasks`): the command of a `Bash` call
  run in the background, or the description of an `Agent`/`Task` call that
  runs async, and how each ended, from the `<task-notification>` that Claude
  writes into the transcript. Tool results are not read.
- For Claude, whether a prompt starts with `[Request interrupted by user`,
  which ends the turn.
- For Claude, the session `name` from its session file, which is also the
  terminal title; the plugin uses it to find the pane of a background
  session.

Prompts and replies are otherwise never read. Known limits: a quiet tool (a
long `sleep`, a stalled download) awaiting no permission looks `blocked`, and
when two Claude agents in one directory both `/clear` at about the same time
their new transcripts can be swapped.
