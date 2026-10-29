# How detection works

`zj-herdr scan` looks for processes of known agents (`claude`, `codex`,
`opencode`, `gemini`, `copilot`, `cursor-agent`, `pi`, `aider`; the table is
`crates/cli/src/agents.rs`), reads `ZELLIJ_SESSION_NAME` and `ZELLIJ_PANE_ID`
from the process environment (or its nearest ancestor's) to find the pane,
and derives a state. Claude Code's own background machinery is skipped:
`claude daemon`, its `bg-pty-host` processes, and `bg-spare` workers still
waiting in the daemon's `spare` directory. A claimed `bg-spare` is a
background session and is listed outside Zellij, whatever pane variables it
inherited from the daemon. Headless one-shot runs (`claude -p`/`--print`, as
hooks and plugins start them) are skipped too; long-lived headless sessions
(`--input-format stream-json`) are listed.

| State | Rule |
|---|---|
| `blocked` | the transcript ends with a tool call that has no result, and both the transcript and the agent's process tree have been quiet for 5 s |
| `working` | the transcript was written in the last 5 s, or the process tree (agent plus the tools it runs) uses CPU |
| `idle` | the turn ended (or was interrupted) and everything is quiet |
| `done` | the process exited; kept in the list for 10 s. An agent that ran less than 30 s in all was a helper and is dropped as soon as it exits (`wait <agent> done` already waiting on it still returns) |

Transcripts read:

- Claude Code: the session Claude records for the process in
  `~/.claude/sessions/<pid>.json` (only its `pid`, `sessionId`, `cwd` and
  `startedAt` are read; a file left by an earlier process with the same pid
  is ignored), whose transcript is
  `~/.claude/projects/<cwd-slug>/<sessionId>.jsonl` or, for a session that
  moved into a worktree, the same file in another project directory. Without
  a session file: the newest `~/.claude/projects/<cwd-slug>/*.jsonl`, where
  the slug is the cwd with `/` and `.` replaced by `-` (falling back to
  Claude's own form, which also replaces `_` and other symbols), and
  `--resume <id>` picks that file. `$CLAUDE_CONFIG_DIR` is honoured. When
  several Claude agents share a cwd, each remaining transcript goes to the
  process that was running when it was created, so two agents never report
  the same session.
- Codex: the newest `~/.codex/sessions/**/rollout-*.jsonl` recorded for the
  process cwd (`$CODEX_HOME` is honoured; `codex resume <id>` picks that one).
- Other agents: CPU only, so they show `working` or `idle`, never `blocked`.

For Claude Code the session's token use is sent as `tokens` (`input`,
`output`, `cache_read`, `cache_write`): the sums of `message.usage` over the
transcript's `assistant` entries, each reply once. Claude Code writes a reply
as one entry per content block, each carrying the reply's whole usage, so
consecutive entries with the same `message.id` count as one, at the usage of
the last. Each line is parsed once, as it is
appended; a transcript that is replaced or shrinks is summed again from the
start. Codex and the other agents report no tokens yet.

Only the shape of each entry is read, never message text, with one
exception: for a `blocked` agent, the name of the tool call it waits on and,
for a shell tool (Claude `Bash`, Codex `shell`/`exec_command`), its command.
That text is sent as `waiting` (one line, at most 60 characters, absent in
other states) and shown by the plugin under the agent as `↳ Bash: cargo
test`. Tool results, prompts and replies are never read. Known limits: a
quiet tool (a long `sleep`, a stalled download) awaiting no permission looks
`blocked`, and when two Claude agents in one directory both `/clear` at
about the same time their new transcripts can be swapped.
