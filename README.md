# zj-herdr

A [herdr](https://herdr.dev/docs/)-style agent runtime for Zellij: hook-free
agent state detection for every agent on the machine, a socket API agents can
drive, a combined agent list across sessions and SSH machines, and session
restore.

## How it differs from zj-radar

| | zj-radar | zj-herdr |
|---|---|---|
| State source | a hook installed in each agent CLI | process inspection plus the transcripts agents already write; nothing to install in any agent |
| Coverage | agents in the current Zellij session | every agent process on the machine, inside Zellij or not |
| Machines | local only | local plus remote hosts over SSH |
| Control | display | socket API and CLI: `list`, `wait`, `prompt`, `spawn`, `watch` |
| Restore | none | `zj-herdr restore` reopens agent sessions with their resume command |

## How detection works

`zj-herdr scan` looks for processes of known agents (`claude`, `codex`,
`opencode`, `gemini`, `copilot`, `cursor-agent`, `pi`, `aider`; the table is
`crates/cli/src/agents.rs`), reads `ZELLIJ_SESSION_NAME` and `ZELLIJ_PANE_ID`
from the process environment (or its nearest ancestor's) to find the pane,
and derives a state:

| State | Rule |
|---|---|
| `blocked` | the transcript ends with a tool call that has no result, and both the transcript and the agent's process tree have been quiet for 5 s |
| `working` | the transcript was written in the last 5 s, or the process tree (agent plus the tools it runs) uses CPU |
| `idle` | the turn ended (or was interrupted) and everything is quiet |
| `done` | the process exited; kept in the list for 60 s |

Transcripts read:

- Claude Code: the newest `~/.claude/projects/<cwd-slug>/*.jsonl`, where the
  slug is the cwd with `/` and `.` replaced by `-` (falling back to Claude's
  own form, which also replaces `_` and other symbols). `$CLAUDE_CONFIG_DIR`
  is honoured and `--resume <id>` picks that file. When several Claude
  agents share a cwd, each transcript goes to the process that was running
  when it was created, so two agents never report the same session.
- Codex: the newest `~/.codex/sessions/**/rollout-*.jsonl` recorded for the
  process cwd (`$CODEX_HOME` is honoured; `codex resume <id>` picks that one).
- Other agents: CPU only, so they show `working` or `idle`, never `blocked`.

Only the shape of each entry is read, never message text, with one
exception: for a `blocked` agent, the name of the tool call it waits on and,
for a shell tool (Claude `Bash`, Codex `shell`/`exec_command`), its command.
That text is sent as `waiting` (one line, at most 60 characters, absent in
other states) and shown by the plugin under the agent as `↳ Bash: cargo
test`. Tool results, prompts and replies are never read. Known limits: a
quiet tool (a long `sleep`, a stalled download) awaiting no permission looks
`blocked`, and when two Claude agents in one directory both `/clear` at
about the same time their new transcripts can be swapped.

## Install

```sh
cargo install --path crates/cli          # the zj-herdr binary
rustup target add wasm32-wasip1
cargo build -p zj-herdr-plugin --target wasm32-wasip1 --release
mkdir -p ~/.config/zellij/plugins
cp target/wasm32-wasip1/release/zj_herdr.wasm ~/.config/zellij/plugins/
```

Run the daemon (from a service manager, or a Zellij pane):

```sh
zj-herdr daemon
```

It rescans every second, serves `$XDG_RUNTIME_DIR/zj-herdr.sock` (falling
back to `$TMPDIR/zj-herdr-$UID.sock`; override with `--socket` or
`$ZJ_HERDR_SOCKET`), and pushes state to every running Zellij session with
`zellij pipe --name zj_herdr.state.v1`. The socket is mode 0600, in a
directory that is either private or shared with the sticky bit (`/tmp`).
A socket path owned by another user is refused by both the daemon and the
CLI, and the daemon serves only connections from its own uid.

## Zellij config

[`docs/zellij.kdl`](docs/zellij.kdl) has the plugin alias and keybinds:

- `Alt a` opens the floating picker: one row per agent across all sessions
  and machines, grouped by machine then session, blocked first, with a state
  glyph and the time in that state. `j`/`k` or arrows move, `Enter` jumps to
  the pane (switching session if needed), `n`/`p` cycle attention, `Esc`
  hides it. Remote rows show their SSH target and do not jump.
  On this machine, the agents of each Zellij session sit under the tab that
  holds their pane, as in zj-radar: the current session first with all its
  tabs (`▌` marks the active one), other sessions with the tabs that have
  agents. Agents outside Zellij and remote machines stay grouped by host
  and session. Click an agent, tab or session row to jump to it.
  Zellij does not always describe other sessions to a plugin, so the
  daemon also records each agent's tab: every 5 s at most, it runs
  `zellij --session S action list-panes --json --tab` in each session
  that has agents, and sends the result as `pane.tab`
  (`{"position":0,"name":"api"}`, absent when unknown) in the snapshot.
- A pinned rail (`pinned "true"` in a layout) makes itself unselectable
  once its permissions are answered, like Zellij's status bar, so closing
  the last terminal pane still closes the tab and the session.
- `Alt ]` / `Alt [` jump to the next / previous agent that needs you:
  blocked ones first, then idle ones.

The plugin asks for permission to read and change application state, write
to panes (for `prompt`), read CLI pipes, and run commands (once, to fetch the
first snapshot with `zj-herdr list --json`). Plugin options:

| Option | Default | Meaning |
|---|---|---|
| `pinned` | `false` | stay open after a jump (for a pane in a layout) |
| `deliver_prompts` | `false` | type prompts sent to this instance; `zj-herdr prompt` sets it on its own instance, so leave it unset in layouts |
| `zj_herdr_bin` | `zj-herdr` | binary used for the first snapshot (and by `start_daemon`) |
| `start_daemon` | `false` | start `zj-herdr daemon` in the background once permitted, unless one runs already; it logs to `$XDG_STATE_HOME/zj-herdr/daemon.log` and outlives Zellij |

Pipe commands (`zellij pipe --name zj_herdr.cmd.v1 -- <payload>`):
`attention-next`, `attention-prev`, `toggle`, and
`{"cmd":"prompt","pane_id":N,"text":"..."}`.

`zj-herdr prompt` runs `zellij --session S action pipe --plugin zj_herdr
--plugin-configuration deliver_prompts=true --floating-plugin true` in the
agent's session. Zellij matches plugin instances by URL and configuration,
so this reaches one dedicated instance (loaded floating the first time, and
hidden again afterwards) and never the picker panes, which would each type
the prompt again. The plugin answers `zj_herdr.prompt.delivered` once the
text is typed, or fails if the pane is gone; without
that answer the command fails rather than report success. The first time
the plugin loads in a session, Zellij asks for its permissions in the
floating pane, and the prompt waits for them. Set `plugin = "..."` at the
top of `config.toml` if your alias or plugin URL differs.

## CLI

```sh
zj-herdr scan [--json]                 # this machine, once, no daemon needed
zj-herdr list [--json]                 # everything the daemon knows (falls back to scan)
zj-herdr wait <agent> <state> [--timeout S]
zj-herdr prompt <agent> <text...> [--no-enter]
zj-herdr spawn --session S [--cwd DIR] -- <command...>
zj-herdr watch [--json]                # stream state changes
zj-herdr restore [--session S] [--dry-run]
```

`<agent>` is an id (`machine/pid`), a pid, or `session:pane_id`. `wait`
exits 1 on timeout, so an agent can wait for another to need attention:

```sh
zj-herdr wait dev:3 blocked --timeout 600 && zj-herdr prompt dev:3 "yes, go ahead"
```

### Socket API

Newline-delimited JSON on the socket; one response line per request:

```json
{"op":"list"}
{"op":"wait","agent":"box/123","state":"blocked","timeout":60}
{"op":"prompt","agent":"dev:3","text":"continue","submit":true}
{"op":"spawn","session":"dev","cwd":"/work","command":["claude"]}
{"op":"subscribe"}
```

Responses look like `{"ok":true,"state":{...}}` or
`{"ok":false,"error":"..."}`. `subscribe` streams
`{"event":"snapshot","state":{...}}` and then
`{"event":"change","agent":{...},"from":"working"}` lines. Snapshots use the
versioned `zj_herdr.state.v1` format from `crates/core`.

## Remote machines

List hosts in `~/.config/zj-herdr/config.toml`:

```toml
# plugin = "zj_herdr"     # Zellij alias or URL that types prompts

[[remote]]
name = "l30n4rd0d4v1nc1"
ssh = "l30n4rd0d4v1nc1"   # a Host alias from ~/.ssh/config
# command = "zj-herdr"    # remote binary, if not on the non-interactive PATH
# interval_secs = 5
# timeout_secs = 10
```

The daemon runs `ssh -o BatchMode=yes <ssh> zj-herdr scan --json` on each
interval and merges the result under `name`; a failure or timeout marks the
machine `unreachable` and keeps its last known agents. It relies on your own
SSH config and agent: zj-herdr never reads, copies or stores keys, and the
config holds no credentials. Remote agents are listed and can be waited on;
`prompt` and jumping only work for local agents.

## Session restore

The daemon records, per pane, the agent kind, cwd and agent session id in
`$XDG_STATE_HOME/zj-herdr/sessions.json` (default
`~/.local/state/zj-herdr/sessions.json`). After Zellij resurrects a session,
`zj-herdr restore --session S` opens a pane per record running
`claude --resume <id>` or `codex resume <id>` in its cwd, skipping sessions
that are already running. Layout is left to Zellij's session resurrection.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build -p zj-herdr-plugin --target wasm32-wasip1 --release
```

Tests use the fixture transcripts in `crates/cli/tests/fixtures/` and never
read the real `~/.claude`, `~/.codex` or `~/.ssh`. See
[docs/plan.md](docs/plan.md) for the design.
