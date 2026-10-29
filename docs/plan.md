# zj-herdr implementation plan

Goal: a herdr-style agent runtime for Zellij, written in Rust. It replaces
zj-radar (marktoda/zj-radar) for this user. Unlike zj-radar it needs **no hook
inside any agent CLI**, covers **every agent on the machine** (like `abtop`,
not only the current Zellij session), and merges **remote machines over SSH**.

Reference for behaviour: herdr (https://github.com/herdrdev/herdr,
https://herdr.dev/docs/). Do not copy its code; reimplement the ideas.

## Shape

Cargo workspace, three crates:

| Crate | Target | Role |
|-------|--------|------|
| `crates/core` (`zj-herdr-core`) | native + wasm | shared types: `Agent`, `AgentState` (`working`, `blocked`, `idle`, `done`, `unknown`), `Machine`, the versioned JSON wire format `zj_herdr.state.v1` and `zj_herdr.cmd.v1` (serde) |
| `crates/cli` (`zj-herdr` binary) | native | daemon, scanner, socket API, CLI |
| `crates/plugin` (`zj_herdr.wasm`) | `wasm32-wasip1` | Zellij plugin: agent list and jumping |

Build the plugin with `cargo build -p zj-herdr-plugin --target wasm32-wasip1
--release`. If the target is missing, run `rustup target add wasm32-wasip1`;
if that is not possible, say so and finish the native crates.

## Phase 1: hook-free detection (core of the project)

`zj-herdr scan --json` prints every agent on this machine.

1. **Find agent processes** with the `sysinfo` crate: match the executable or
   argv against a table of known agents (`claude`, `codex`, `opencode`,
   `gemini`, `copilot`, `cursor-agent`, `pi`, `aider`). Keep the table in one
   place so new agents are one line.
2. **Locate the pane**: read the process environment (`ZELLIJ_SESSION_NAME`,
   `ZELLIJ_PANE_ID`; walk up the parent chain if the agent itself lacks them).
   An agent outside Zellij is still listed, with `pane: null`.
3. **Derive state without hooks**, from what the agent already writes:
   - Claude Code: newest `~/.claude/projects/<cwd-slug>/*.jsonl` for the
     process cwd (slug = cwd with `/` and `.` replaced by `-`).
   - Codex: newest `~/.codex/sessions/**/rollout-*.jsonl` whose recorded cwd
     matches.
   - Others: fall back to process signals only.
   Rules: last assistant entry has a `tool_use` with no `tool_result` and the
   process is near-idle on CPU for > N seconds, then `blocked`; transcript or
   CPU active in the last few seconds, then `working`; last entry is an
   assistant turn that ended, then `idle`; process gone, then `done`.
   Make thresholds constants with a one-line why.
4. Each detector is a small trait (`Detector::detect(&ProcInfo) ->
   Option<AgentStatus>`), one impl per agent family.
5. Tests use **fixture transcripts** under `crates/cli/tests/fixtures/` only.
   Never read the real `~/.claude` or `~/.codex` in tests.

## Phase 2: daemon and socket API

`zj-herdr daemon` rescans every second and serves a Unix socket at
`$XDG_RUNTIME_DIR/zj-herdr.sock` (fallback `$TMPDIR/zj-herdr-$UID.sock`,
mode 0600). Newline-delimited JSON requests:

- `list`: all agents (local and remote).
- `wait {agent, state, timeout}`: return when that agent reaches the state.
  This is herdr's "wait until another agent is blocked".
- `prompt {agent, text}`: send text to the agent's pane (via the plugin,
  see phase 3).
- `spawn {session, cwd, command}`: `zellij --session S run --cwd ... -- cmd`.
- `subscribe`: stream state changes.

CLI subcommands map one to one: `zj-herdr list|wait|prompt|spawn|watch`.
When the state of an agent changes, the daemon pushes it, at most every two
seconds, to every Zellij session with a client attached:
`zellij --session S pipe --name zj_herdr.state.v1 -- <json>`.
Use `tokio` only if plain threads get awkward; prefer fewer dependencies.

## Phase 3: Zellij plugin

- Floating picker (also usable pinned as a narrow pane): one row per agent
  across all sessions and machines, grouped by machine then session, state
  glyph and elapsed time, blocked first.
- Enter on a row jumps: `switch_session` plus `focus_terminal_pane`.
  Remote rows show the SSH target and do nothing else for now.
- Pipe commands (`zj_herdr.cmd.v1`): `attention-next`, `attention-prev`
  (cycle through blocked, then idle agents), `toggle`, and
  `prompt {pane_id, text}` which calls `write_chars_to_pane_id`. This is how
  the daemon delivers `prompt`.
- Ship an example `docs/zellij.kdl` with the plugin alias and keybinds.

## Phase 4: remote machines

`~/.config/zj-herdr/config.toml` lists hosts:

```toml
[[remote]]
name = "l30n4rd0d4v1nc1"
ssh = "l30n4rd0d4v1nc1"   # a Host alias from ~/.ssh/config
```

The daemon runs `ssh -o BatchMode=yes <ssh> zj-herdr scan --json` on an
interval, with a timeout, and marks the machine `unreachable` on failure.
It relies on the user's own SSH config and agent: never read, copy or store
keys, and never put credentials in config.

## Phase 5: session restore

The daemon records, per pane, the agent kind, cwd and agent session id
(Claude: the transcript file stem; Codex: the rollout id) in
`$XDG_STATE_HOME/zj-herdr/sessions.json`. `zj-herdr restore [--session S]`
reopens each one in Zellij with its resume command (`claude --resume <id>`,
`codex resume <id>`). Layout itself is left to Zellij's session
resurrection.

## Done when

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` pass for the workspace; the plugin builds for `wasm32-wasip1`.
- Unit tests cover each detector's state rules with fixtures, the wire format
  round trip, the socket request parsing, and the remote-output merge.
- `README.md` explains install, the Zellij config, the CLI, and how it
  differs from zj-radar.
- One commit per phase, Conventional Commits, no agent trailers.

## Out of scope

- Any change to the Dotfiles repository (dytoy recipe, zellij config): done
  separately after this lands.
- Screen scraping of pane contents, desktop notifications, mouse support,
  a plugin marketplace.
- Publishing releases or pushing anywhere.
