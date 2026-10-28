# Rail features plan

Builds on `feat/plugin-tab-tree` (commit b8bd59d): the plugin draws local
agents as host -> session -> tab -> pane via `view::tree`
(`crates/core/src/view.rs`), renders it in `crates/plugin/src/ui.rs` with a
click target per line, and a pinned rail calls `set_selectable(false)`.
Keep all of that working.

Do the phases in order, one commit each (Conventional Commits, subject
under 50 characters). Every phase adds tests next to the code it changes
and keeps these green:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build -p zj-herdr-plugin --target wasm32-wasip1 --release
```

Update `README.md` for every user-visible change (options, pipe commands,
socket ops, what is read from transcripts). Wire-format changes stay
backward compatible: new fields are optional with `#[serde(default)]`, and
an old snapshot still parses (add a test).

Check every `zellij_tile` call against the vendored source of
zellij-tile 0.45 in `~/.cargo/registry/src/*/zellij-tile-0.45*/src/shim.rs`
rather than guessing a signature.

## Phase 1: tab names from the daemon (fallback for other sessions)

`Event::SessionUpdate` may arrive empty, so other sessions would lose
their tabs. Make the daemon supply them.

- `crates/core`: add `tab: Option<PaneTab>` to `Pane`, with
  `PaneTab { position: usize, name: String }`.
- `crates/cli/src/zellij.rs`: a `Zellij::panes(session)` method running
  `zellij --session S action list-panes --json --tab` (fields `id`,
  `is_plugin`, `tab_position`, `tab_name`), with a pure parser and a test
  on a sample JSON.
- The daemon fills `pane.tab` for local agents, querying only sessions
  that have agents, cached per session and refreshed at most every 5 s.
  A failed query leaves `tab` as `None`.
- `crates/plugin/src/main.rs` `layouts()`: a session missing from
  `SessionUpdate` gets a layout built from its agents' `pane.tab`
  (a helper in `view` so it is tested natively).

## Phase 2: why a blocked agent waits (`↳` line)

- Add `waiting: Option<String>` to `Agent`: for a `blocked` agent, the
  pending tool's name plus, for a shell tool (Claude `Bash`, Codex
  `exec_command`/`shell`), its command; at most 60 characters, newlines
  turned into spaces. `None` otherwise.
- Read it in `crates/cli/src/transcript.rs` / `detect.rs` from the same
  unmatched tool call that makes the agent `blocked`. Never read message
  text. Extend the fixture tests (`crates/cli/tests/fixtures/`).
- Rail: under a blocked agent's row, a dim `↳ <waiting>` line, indented to
  the agent row's branch (`│` continuation when more agents follow in the
  tab). It has the agent as its click target and is not a cursor stop.
- README "How detection works": replace "Only the shape of each entry is
  read, never message text" with what is now read.

## Phase 3: name tabs after their agent's directory

- Plugin option `rename_tabs`, default `true`. In the current session, a
  tab whose name is Zellij's default `Tab #<n>`, or the last name this
  instance gave it, and that holds at least one agent with a cwd, is
  renamed to the basename of the first agent's cwd (in pane order). A tab
  the user renamed is never touched.
- Only the instance whose own tab is active issues renames (find the own
  tab from `get_plugin_ids().plugin_id` in the `PaneManifest`), so the one
  rail per tab do not race.
- Keep the decision in a pure function with tests.

## Phase 4: fold tabs without agents, footer tally

- In the current session, agent-less tabs other than the active one fold
  into one dim `+N tabs` line at the end of the session's tabs (no click
  target). The active tab always shows.
- When at least two lines are spare, the last line of the rail is a tally
  `N working · M need you` (`· M need you` only when M > 0, "need you" =
  blocked + idle) with a rule above it. The header keeps
  `zj-herdr` plus counts only when there is no room for the footer.
- Scrolling still keeps the selected agent visible.

## Phase 5: keys for the pinned rail

A pinned rail is unselectable, so it gets no keys. Add pipe commands on
`zj_herdr.cmd.v1`: `select-next`, `select-prev`, `jump` (jump to the
selected agent). In `crates/core/src/cmd.rs` with parse tests.

- Every rail of a session receives the pipe; only the one whose own tab
  is active acts, the others ignore it.
- `docs/zellij.kdl` and the README: example binds. A bind must address the
  rail's own configuration, because Zellij matches instances by URL and
  configuration:
  `MessagePlugin "zj_herdr" { name "zj_herdr.cmd.v1"; payload "select-next"; pinned "true"; start_daemon "true"; }`
  (check the exact KDL `MessagePlugin` syntax for passing configuration in
  Zellij 0.45 and document what works).

## Phase 6: move an agent from outside Zellij into it

- New socket op `adopt` (`{"op":"adopt","agent":"<id>","session":"S"}`)
  and CLI `zj-herdr adopt <agent> --session S`: only for a local agent
  with no pane, a known `session_id` and a kind with a resume command
  (reuse what `restore` uses). It checks the pid is still that agent,
  sends SIGTERM, waits up to 5 s for it to exit (fails, without spawning,
  if it does not), then spawns the resume command in session S with the
  agent's cwd through `Zellij::run`. Tests with the fake `Zellij` and a
  fake process seam; never signal a real process in tests.
- Rail: clicking (or `jump` on) an outside-Zellij agent arms it: the row
  shows `click again to move here` for 5 s; a second click within that
  time runs `zj-herdr adopt <id> --session <current>` with `run_command`.
  Any other click disarms it.

## Phase 7: offer restore after a resurrect

- Once permitted, each instance runs
  `zj-herdr restore --session <current> --dry-run` (`run_command` with its
  own context key) and parses the count of agents it would open. When
  non-zero, the rail shows a banner line `↻ restore N agents` under the
  header; clicking it runs `zj-herdr restore --session <current>` and
  hides the banner. Re-check on `Visible(true)`.
- `restore` already skips sessions that are running, so a second click
  from another tab's rail is harmless; keep it that way and say so in a
  test.
- If the dry-run output is not machine-readable, add `--json` to
  `restore --dry-run` first.

## Out of scope

Desktop notifications, releases, CI, anything outside this repository,
pushing.
