# CLI, daemon and socket API

## Daemon

Run the daemon (from a service manager, a Zellij pane, or the plugin's
`start_daemon` option):

```sh
zj-herdr daemon [--config FILE]
```

It rescans every second and serves `$ZJ_HERDR_SOCKET`, else
`$XDG_RUNTIME_DIR/zj-herdr.sock`, else `$TMPDIR/zj-herdr-$UID.sock` (`/tmp`
when `$TMPDIR` is unset). `--socket FILE`, accepted by every subcommand,
overrides all three. The first scan runs before the socket answers, so a
client that connects at startup never sees this machine missing.

Every two seconds the daemon checks which Zellij sessions have a client
attached and pushes the state to them with
`zellij --session S pipe --name zj_herdr.state.v1`. A session gets it when
something changed, when five seconds passed since the last push, or when a
client has just attached. A session nobody is attached to gets nothing, and
catches up when one attaches. It also asks each such session for its tab
layout (`zellij action list-panes --json --tab`, at most every 5 s) and sends
the result as `pane.tab` (`{"position":0,"name":"api"}`, absent when
unknown).

The socket is mode 0600, in a directory that is either private (yours, not
writable by others) or shared with the sticky bit (`/tmp`). A socket path
owned by another user is refused by both the daemon and the CLI, a second
daemon on the same path exits with an error, and the daemon serves only
connections from its own uid.

The daemon refuses to start when `config.toml` is invalid, or when a
`[[remote]]` has the name of this machine. A `sessions.json` it cannot read
is renamed to `sessions.json.unreadable` and started afresh.

## CLI

```sh
zj-herdr scan [--json]                 # this machine, once, no daemon needed
zj-herdr list [--json]                 # everything the daemon knows
zj-herdr wait <agent> <state> [--timeout S]
zj-herdr prompt <agent> <text...> [--no-enter]
zj-herdr spawn --session S [--cwd DIR] -- <command...>
zj-herdr watch [--json]                # stream state changes
zj-herdr restore [--session S] [--dry-run [--json]]
zj-herdr adopt <agent> --session S      # move an agent from outside Zellij into S
zj-herdr ack <agent>                    # mark an idle agent seen
```

`<agent>` is an id (`machine/pid`), a pid, or `session:pane_id`. When a pane
selector matches agents on several machines, the local one wins. Every
failure prints `zj-herdr: <reason>` on stderr and exits 1; success exits 0.

- `scan` and `list` print a table (blocked agents first), or the
  `zj_herdr.state.v1` snapshot with `--json`. Without a daemon, `list` says
  so on stderr and falls back to a local scan; the other commands that talk
  to the daemon (`wait`, `prompt`, `spawn`, `watch`, `adopt`, `ack`) fail
  with "cannot reach the daemon".
- `wait` takes `working`, `blocked`, `idle` or `done` and returns at once if
  the agent is in that state. It prints the agent's line on success and
  exits 1 on `--timeout` (seconds, may be fractional). Without a timeout it
  waits for ever, also for an agent that is not listed yet. `wait <agent>
  done` also returns for a helper that exits before it was listed as `done`.
  An agent on a machine that is unreachable never satisfies a wait.
- `prompt` joins the words with spaces and presses Enter unless `--no-enter`.
  It works only for a local agent that runs in a Zellij pane and has not
  exited; it goes through the plugin (see [Prompt delivery](rail.md#prompt-delivery))
  and fails unless the plugin confirms the text was typed.
- `spawn` runs `zellij --session S run [--cwd DIR] -- <command>`, which opens
  a new pane. A relative `--cwd` is made absolute first.
- `watch` prints a table, then one line per event (`box/123 claude idle ->
  blocked dev:3 /w/proj`, `<id> gone`, `<machine> unreachable: <why>`); with
  `--json`, the raw event lines from the socket API below.
- `ack` works on an idle agent only; see [Seen marks](rail.md#buttons).
- `restore --json` needs `--dry-run`.

`adopt` works on a local agent running outside Zellij whose session id and
cwd are known and whose kind has a resume command (`claude`, `codex`). It is
refused for a background session of the Claude Code daemon, which a client in
some pane shows. The daemon checks that session S is running and that the pid
still runs that agent (same start time, kind and cwd, outside Zellij), sends
it SIGTERM, waits up to 5 s for it and any native helper of its own kind to
exit (and fails without opening anything if they do not), then opens
`claude --resume <id>` or `codex resume <id>` in a new pane of S, in the
agent's cwd. A second `adopt` of the same agent while one is under way is
refused.

An agent can wait for another to need attention, then answer it:

```sh
zj-herdr wait dev:3 blocked --timeout 600 && zj-herdr prompt dev:3 "yes, go ahead"
```

## Socket API

Newline-delimited JSON on the socket; one response line per request:

```json
{"op":"list"}
{"op":"wait","agent":"box/123","state":"blocked","timeout":60}
{"op":"prompt","agent":"dev:3","text":"continue","submit":true}
{"op":"spawn","session":"dev","cwd":"/work","command":["claude"]}
{"op":"subscribe"}
{"op":"adopt","agent":"box/123","session":"dev"}
{"op":"ack","agent":"box/123"}
```

`submit` defaults to true. `command` in `spawn` is an argv array, or a single
string that runs through `sh -c`; an empty one is refused. `timeout` is
optional, in seconds, and must be a non-negative number.

Responses look like `{"ok":true,"state":{...}}`, with `"agent":{...}` for
`wait`, `adopt` and `ack`, or `{"ok":false,"error":"..."}`. `subscribe` takes
the connection over and streams one JSON object per line:

| Event | When |
|---|---|
| `{"event":"snapshot","state":{...}}` | once, first |
| `{"event":"change","agent":{...},"from":"working"}` | an agent changed state; `from` is `null` for a new one |
| `{"event":"gone","id":"box/123"}` | a `done` agent was dropped from the list |
| `{"event":"machine","name":"box","status":"unreachable","error":"..."}` | a machine became unreachable or came back |

Snapshots use the versioned `zj_herdr.state.v1` format from `crates/core`:
`{"schema","generated_at","machines":[{"name","ssh","status","error",
"agents":[...]}]}`. An agent has `id`, `machine`, `kind`, `pid`, `cwd`,
`state` (`working`, `blocked`, `idle`, `done`, `unknown`), `since` (Unix
seconds), `pane` (`{"session","pane_id","tab"}`, `null` outside Zellij) and
`session_id`. These fields are left out when empty: `waiting`, `started`,
`tokens` (`input`, `output`, `cache_read`, `cache_write`), `error`, `acked`,
`tasks` (`kind`, `label`, `state`, `started`, `ended`), `model`, `context`
(percent), `background` and `title`.

## Remote machines

List hosts in `$XDG_CONFIG_HOME/zj-herdr/config.toml` (default
`~/.config/zj-herdr/config.toml`; `zj-herdr daemon --config FILE` picks
another). A missing file is an empty config; unknown keys are an error.

```toml
# plugin = "zj_herdr"     # Zellij alias or URL that types prompts

[[remote]]
name = "l30n4rd0d4v1nc1"
ssh = "l30n4rd0d4v1nc1"   # a Host alias from ~/.ssh/config
# command = "zj-herdr"    # remote binary, if not on the non-interactive PATH
# interval_secs = 5
# timeout_secs = 10
```

The daemon runs `ssh -o BatchMode=yes -o ConnectTimeout=<timeout_secs> --
<ssh> "zj-herdr scan --json"` on each interval and merges the result under
`name`. `name` must be unique, non-empty, contain no `/` and differ from this
machine's name; `ssh` must not start with `-` or hold spaces; both intervals
must be positive. Remote agent ids become `name/pid`. A failure, a timeout
or output over 8 MiB marks the machine `unreachable` and keeps its last
known agents until it answers again.

It relies on your own SSH config and agent: zj-herdr never reads, copies or
stores keys, and the config holds no credentials. Remote agents are listed
and can be waited on; `prompt`, `adopt` and jumping only work for local
agents.

## Session restore

The daemon records, per pane, the agent kind, cwd and agent session id in
`$XDG_STATE_HOME/zj-herdr/sessions.json` (default
`~/.local/state/zj-herdr/sessions.json`, schema `zj_herdr.sessions.v1`). It
records an agent only once its session id is known, and drops a pane's
record when an agent of another kind takes the pane over. After Zellij
resurrects a session, `zj-herdr restore --session S` opens a pane per record
running `claude --resume <id>` or `codex resume <id>` in its cwd, skipping
records whose agent session is already running (it notes each skip on
stderr) and kinds with no resume command. Without `--session` it covers every
session. Layout is left to Zellij's session resurrection. It exits 1 if a
pane could not be opened.
`zj-herdr restore --session S --dry-run --json` prints what it would
reopen as `{"count":N,"panes":[{"session","pane_id","cwd","command"}]}`.

The plugin offers this itself: once permitted (and again whenever it is
shown), each instance runs that dry run for its own session, and when it
would reopen anything, shows `↻ restore N agents` under the header.
Clicking it runs `zj-herdr restore --session S` and hides the banner.
Since `restore` skips agent sessions that are already running (a resumed
Claude counts from the moment it starts), clicking the banner of another
tab's rail afterwards opens nothing twice.
