# Architecture

zj-herdr is an agent runtime for Zellij. It finds the AI coding agents on a
machine, derives what each is doing, shows them on a rail inside Zellij, and
serves a socket so scripts and other agents can wait on and prompt them. No
hook runs inside any agent CLI, it covers every agent process on the machine
rather than only the current Zellij session, and it merges other machines
over SSH.

## Crates

A Cargo workspace of three crates:

| Crate | Target | Role |
|---|---|---|
| `crates/core` (`zj-herdr-core`) | native and `wasm32-wasip1` | The shared types (`Agent`, `AgentState`, `Machine`, `Task`, `Tokens`), the wire formats `zj_herdr.state.v1` and `zj_herdr.cmd.v1`, the restore preview, and the pure view logic the rail uses (`view.rs`, `command.rs`). |
| `crates/cli` (`zj-herdr`) | native | The `zj-herdr` binary: scanner, detectors, daemon, socket server and client, remote polling, session store. |
| `crates/plugin` (`zj-herdr-plugin`) | `wasm32-wasip1` | The Zellij plugin `zj_herdr.wasm`: the rail, pipe commands, prompt delivery. |

## Data flow

```text
 agents (claude, codex, …) ──┐
 ~/.claude, ~/.codex ────────┼──▶ zj-herdr daemon ──zellij pipe──▶ rail in every session
 ssh host: zj-herdr scan ────┘         │
                                       └──▶ socket ◀── zj-herdr CLI · your scripts
```

- **Scan.** Once a second the daemon samples the process table
  (`procs.rs`), picks out the known agents (`agents.rs`), finds each one's
  Zellij pane from `ZELLIJ_SESSION_NAME` and `ZELLIJ_PANE_ID` in the
  process environment (or its nearest ancestor's), and asks the detector of
  its family for a state (`detect.rs`, `transcript.rs`). One scan of one
  machine is also what `zj-herdr scan` prints.
- **Registry.** `registry.rs` turns successive scans into stable `since`
  times, keeps an exited agent as `done` for 10 s (a helper that ran under
  30 s is dropped at once), holds seen marks, and wakes `wait` callers and
  `subscribe` streams. Local scans and remote polls publish into it.
- **Push.** Every two seconds the daemon lists the Zellij sessions that have
  a client attached and pipes the snapshot to those that need it: when
  something changed, every five seconds as a heartbeat, or when a client
  just attached. The pipe is `zellij --session S pipe --name
  zj_herdr.state.v1 -- <json>`. A session nobody watches is not touched.
  The daemon also asks each such session for its panes' tabs, at most every
  5 s, so the rail can place agents in tabs Zellij does not describe.
- **Rail.** The plugin receives the snapshot and draws it. It reaches Zellij
  only through a `Host` trait (`app.rs`), which is why its behaviour is
  tested natively. How it builds the tree is in
  [rail-internals.md](rail-internals.md).
- **Socket.** The CLI and scripts talk to the daemon over a Unix socket in
  newline-delimited JSON (`api.rs`, `server.rs`, `client.rs`). The commands
  are in [cli.md](cli.md).

## Detection

Detection is hook-free. The process table says which agents run and where;
the transcripts the agents already write say what they are doing:

- `blocked`: the transcript ends on a tool call with no result, and both the
  transcript and the process tree have been quiet for 5 s.
- `working`: the transcript moved in the last 5 s, or the process tree uses
  CPU.
- `idle`: the turn ended and everything is quiet.
- `done`: the process exited.

Claude Code and Codex are read from their transcripts; the other agents from
CPU alone. Each family has one detector behind a small trait, and the table
of known agents is one place, so a new agent is one line. The full rules,
thresholds, transcript lookup and what is skipped are in
[detection.md](detection.md).

## Wire formats

- `zj_herdr.state.v1` is a full snapshot: `schema`, `generated_at` and the
  machines with their agents. The daemon pushes it to the plugin, serves it
  from `list`, and a remote's `scan --json` is the same format. Readers
  refuse any other `schema`. The fields are in [cli.md](cli.md).
- `zj_herdr.cmd.v1` carries commands to the plugin: a bare word from a
  keybind (`attention-next`) or JSON (`{"cmd":"prompt","pane_id":3,...}`).
  The commands are in [rail.md](rail.md). A prompt is acknowledged on the
  CLI pipe with `zj_herdr.prompt.delivered`, or refused with
  `zj_herdr.prompt.failed: <reason>`; the daemon reports success only when
  it reads the first.

## The daemon

`zj-herdr daemon` runs these loops as threads: the scan loop (every second,
which also records sessions), the push loop (every two seconds), the tab
loop (asks Zellij for pane layouts), one poll loop per configured remote,
and the socket server, one thread per connection. It starts serving only
after its first scan. Commands to Zellij (`pipe`, `run`, `list-clients`,
`list-panes`, prompt delivery) are `zellij` child processes with a hard
timeout (3 s, 20 s for a prompt), behind a trait so tests never need a real
Zellij.

The socket is `$ZJ_HERDR_SOCKET`, else `$XDG_RUNTIME_DIR/zj-herdr.sock`, else
`$TMPDIR/zj-herdr-$UID.sock`. It is bound inside a fresh 0700 directory,
set to mode 0600, then linked into place, so it is never reachable with
looser permissions and a running daemon's socket is never taken over. The
parent directory must be private or sticky and owned by the user or root.
The server checks the peer's uid on every connection, and the client checks
the listener's, so a socket planted by another user is refused both ways.

## Remote machines

`~/.config/zj-herdr/config.toml` lists hosts. For each, the daemon runs
`ssh -o BatchMode=yes -o ConnectTimeout=<n> -- <host> "zj-herdr scan --json"`
on an interval, under a timeout, and merges the result into the registry
under the configured name, with ids re-keyed to `name/pid` and its text fields
stripped of control characters. A failure, a timeout or oversized output
marks the machine `unreachable`, keeps its last agents, and emits a
`machine` event; the next good answer brings it back. Authentication is the
user's own SSH config and agent. See [cli.md](cli.md).

## Session recording and restore

On each scan the daemon records, for every agent in a Zellij pane with a
known session id, the pane, kind, cwd and session id in
`$XDG_STATE_HOME/zj-herdr/sessions.json` (default
`~/.local/state/zj-herdr/sessions.json`), written atomically and only when
something changed. `zj-herdr restore` turns each record into a resume
command (`claude --resume <id>`, `codex resume <id>`; session ids are
validated first) and opens it with `zellij --session S run --cwd DIR`,
skipping any whose agent session is running. Layout is left to Zellij's
session resurrection. The plugin offers the same as a banner.

## Constraints

These hold across the code:

- **No hooks in agents.** Nothing is installed into, or launched around, an
  agent. A new agent version needs no new hook.
- **Transcript shape only.** Transcripts are read from the tail (the last
  256 KiB), and what matters is the shape of each entry. Message text is
  not read, with these exceptions: the name and shell command of the tool
  call a blocked agent waits on (and of a pending Codex approval); the
  command or description of a Claude background task and the outcome in its
  `<task-notification>`; and whether a prompt begins with Claude's
  interruption marker. Each is cut to one line of 60 characters. Token
  counts, model and context come from the `usage` and `model` fields, and a
  session name from Claude's own session file. Tool results, prompts and
  replies are not stored or sent anywhere else.
- **No credentials.** zj-herdr never reads, copies or stores SSH keys or any
  credential, and the config file holds none. Remote access is the `ssh`
  command with the user's own configuration, in `BatchMode`, so it never
  prompts.
- **Local trust.** The socket answers only the user who runs the daemon;
  remote output is treated as untrusted; session ids and ssh targets that
  could read as command-line flags are refused.
- **Tests use fixtures.** Tests read transcripts from
  `crates/cli/tests/fixtures/` and never the real `~/.claude`, `~/.codex` or
  `~/.ssh`. See [development.md](development.md).
