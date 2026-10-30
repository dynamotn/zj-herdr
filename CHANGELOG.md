# Changelog

All notable changes to zj-herdr are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.3] - 2026-10-30

### Fixed

- Shell commands that Claude Code runs in its sandbox no longer show up as
  idle `claude` agents outside Zellij, one pair per command, named after the
  directory they ran in.

## [0.3.2] - 2026-10-30

### Added

- The mouse wheel over the rail moves its cursor, scrolling a list longer
  than the pane; a pinned rail never gets keys, so it could not scroll before.

### Fixed

- A pinned rail turns unselectable again when its permissions were granted
  in an earlier session. Before, it could take focus, and closing the last
  pane left the session hanging instead of exiting.
- Closing the last terminal pane of a tab with a pinned rail closes the tab,
  instead of the rail turning selectable and keeping it open.

### Changed

- The daemon no longer keeps Zellij busy: it pushes state at most every two
  seconds and only to sessions someone is attached to, cutting Zellij's CPU
  use from about two cores to under one with ten sessions open.

## [0.3.1] - 2026-10-29

### Fixed

- A `zj-herdr list` sent just as the daemon starts waits for its first scan
  instead of answering without this machine.

## [0.3.0] - 2026-10-29

### Added

- The rail shows the background shells and subagents an agent started, under
  it on a `┊`, with their age and how they ended.
- Each agent's model, session token total and context window fill
  (`■■■□□ 62%`) on its second line, and `✗` when its last turn hit an API
  error.
- Commands run in this session's panes (builds, tests, servers, ssh) get a row
  under their tab, and every other terminal pane a dim row you can click to
  focus.
- `zj-herdr ack` and a `✓` button mark an idle agent seen, so it stops
  counting as needing you until it changes state.
- A header with counts over a rule, kind marks per agent (`✳` claude, `❉`
  codex, …), a spinner for working agents and a footer of clickable buttons.
- A `⇲` button adopts an agent running outside Zellij; `Enter` only jumps.
- `density` option: `cards` tints each tab from the terminal's colours,
  `comfortable` and `compact` drop the tint and the gaps.
- An `earlier` list of agents and commands that finished lately.
- `session-next` and `session-prev` pipe commands switch Zellij sessions.

### Changed

- The README is a short tour; the full reference moved into `docs/rail.md`,
  `docs/cli.md`, `docs/detection.md` and `docs/development.md`.

### Fixed

- Claude Code's tool-less SDK runs and sessions under a pty host are no longer
  listed as agents.

## [0.2.1] - 2026-10-29

### Added

- The rail reports each Claude Code session's token total.

### Changed

- Claude Code sessions are found through the per-process session file Claude
  writes, so agents sharing a directory keep their own transcript.
- Transcripts in sibling project directories are searched less often.

### Fixed

- Claude Code's daemon, pty hosts and unclaimed spare workers no longer show up
  as agents, and its background sessions no longer take the pane of whoever
  started the daemon.
- One-shot runs (`claude -p`) and short-lived helpers are dropped.
- Each Claude reply counts once in the token total, not once per content block.

## [0.2.0] - 2026-10-29

### Added

- Agents of local Zellij sessions sit under the tab that holds their pane, and
  each tab is named after its first agent's directory (`rename_tabs`).
- A blocked agent shows what it waits on (`↳ Bash: cargo test`).
- Idle tabs fold into one line when the tree does not fit, and a footer tallies
  who is working and who needs you.
- `select-next`, `select-prev` and `jump` pipe commands drive a pinned rail from
  keybinds.
- `zj-herdr adopt` moves an agent running outside Zellij into a new pane,
  resumed where it was.
- After Zellij resurrects a session, the rail offers `↻ restore N agents`.
- The daemon records each agent's tab in the snapshot (`pane.tab`).

### Changed

- A pinned rail makes itself unselectable, so closing the last terminal pane
  still closes the tab and the session.

### Fixed

- `adopt` checks the agent's start time before stopping it, so a reused pid is
  never signalled, and refuses other unsafe targets.
- Tab queries run on their own thread instead of slowing each scan.

## [0.1.0] - 2026-10-28

### Added

- Hook-free state detection (`blocked`, `working`, `idle`, `done`) for every
  known agent process on the machine, from process inspection and the Claude
  Code and Codex transcripts.
- `zj-herdr daemon` with a private socket API, and the `scan`, `list`, `wait`,
  `prompt`, `spawn`, `watch` and `restore` commands.
- Remote machines over SSH, merged into one list.
- The Zellij plugin: a floating picker, attention jumps across sessions, and
  `start_daemon` to launch the daemon from Zellij.
- Release archives of the CLI for Linux and macOS, and the plugin wasm.

[Unreleased]: https://github.com/dynamotn/zj-herdr/compare/v0.3.3...HEAD
[0.3.3]: https://github.com/dynamotn/zj-herdr/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/dynamotn/zj-herdr/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/dynamotn/zj-herdr/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/dynamotn/zj-herdr/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/dynamotn/zj-herdr/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/dynamotn/zj-herdr/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/dynamotn/zj-herdr/releases/tag/v0.1.0
