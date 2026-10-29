# CLI, daemon and socket API

## Daemon

Run the daemon (from a service manager, or a Zellij pane):

```sh
zj-herdr daemon
```

It rescans every second, serves `$XDG_RUNTIME_DIR/zj-herdr.sock` (falling
back to `$TMPDIR/zj-herdr-$UID.sock`; override with `--socket` or
`$ZJ_HERDR_SOCKET`), and pushes state with
`zellij pipe --name zj_herdr.state.v1` at most every two seconds, only to
sessions with a client attached; a session catches up when one attaches.
The socket is mode 0600, in a
directory that is either private or shared with the sticky bit (`/tmp`).
A socket path owned by another user is refused by both the daemon and the
CLI, and the daemon serves only connections from its own uid.

## CLI

```sh
zj-herdr scan [--json]                 # this machine, once, no daemon needed
zj-herdr list [--json]                 # everything the daemon knows (falls back to scan)
zj-herdr wait <agent> <state> [--timeout S]
zj-herdr prompt <agent> <text...> [--no-enter]
zj-herdr spawn --session S [--cwd DIR] -- <command...>
zj-herdr watch [--json]                # stream state changes
zj-herdr restore [--session S] [--dry-run [--json]]
zj-herdr adopt <agent> --session S      # move an agent from outside Zellij into S
zj-herdr ack <agent>                    # mark an idle agent seen
```

`adopt` works on a local agent running outside Zellij whose session id and
cwd are known and whose kind has a resume command (`claude`, `codex`). The
daemon checks that session S is running and that the pid still runs that
agent (same kind and cwd, outside Zellij), sends it SIGTERM, waits up to
5 s for it and any native helper of its own kind to exit (and fails
without opening anything if they do not), then opens `claude --resume
<id>` or `codex resume <id>` in a new pane of S, in the agent's cwd. A
second `adopt` of the same agent while one is under way is refused.

`<agent>` is an id (`machine/pid`), a pid, or `session:pane_id`. `wait`
exits 1 on timeout, so an agent can wait for another to need attention:

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
`zj-herdr restore --session S --dry-run --json` prints what it would
reopen as `{"count":N,"panes":[{"session","pane_id","cwd","command"}]}`.

The plugin offers this itself: once permitted (and again whenever it is
shown), each instance runs that dry run for its own session, and when it
would reopen anything, shows `↻ restore N agents` under the header.
Clicking it runs `zj-herdr restore --session S` and hides the banner.
Since `restore` skips agent sessions that are already running (a resumed
Claude counts from the moment it starts), clicking the banner of another
tab's rail afterwards opens nothing twice.
