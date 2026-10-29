<h1 align="center">zj-herdr</h1>

<p align="center">
  <b>Herd every AI agent on your machine from inside Zellij.</b><br>
  No hooks to install, no wrapper to launch through. It watches the processes
  and transcripts your agents already have, and tells you who needs you.
</p>

<p align="center">
  <a href="https://github.com/dynamotn/zj-herdr/actions/workflows/ci.yml">
    <img alt="CI" src="https://img.shields.io/github/actions/workflow/status/dynamotn/zj-herdr/ci.yml?branch=main&label=ci">
  </a>
  <a href="https://github.com/dynamotn/zj-herdr/releases/latest">
    <img alt="Release" src="https://img.shields.io/github/v/release/dynamotn/zj-herdr?label=release">
  </a>
  <img alt="Coverage gate" src="https://img.shields.io/badge/coverage-%E2%89%A595%25-brightgreen">
  <img alt="Zellij plugin" src="https://img.shields.io/badge/zellij-0.45%20plugin-8A2BE2">
  <img alt="Hook-free" src="https://img.shields.io/badge/hooks-none-informational">
  <br>
  <img alt="Claude Code" src="https://img.shields.io/badge/Claude%20Code-full-orange">
  <img alt="Codex" src="https://img.shields.io/badge/Codex-full-black">
  <img alt="Opencode" src="https://img.shields.io/badge/Opencode-cpu%20only-4B8BBE">
  <img alt="Gemini" src="https://img.shields.io/badge/Gemini-cpu%20only-1A73E8">
  <img alt="pi" src="https://img.shields.io/badge/pi-cpu%20only-6E56CF">
  <img alt="aider" src="https://img.shields.io/badge/aider-cpu%20only-14B014">
</p>

<p align="center">
  <a href="#quick-start">Quick start</a> ·
  <a href="#what-you-get">What you get</a> ·
  <a href="#how-it-works">How it works</a> ·
  <a href="#drive-it-from-a-script">Scripting</a> ·
  <a href="#how-is-this-different">Compared</a> ·
  <a href="#documentation">Docs</a>
</p>

```text
 zj-herdr  ·4▲ 1!
 ═════════◆══════════════════════
 ▌ api
   ◆ ✳ api                     2m
     opus5.5 4.8M ■■■□□ 62% #12
     ↳ Bash: cargo test --workspace
   ⠋ ❉ api-worker             14s
     gpt-5 1.2M ■□□□□ 18% #14
     ┊ ⠹ npm test            0:42
   ⚗ cargo test               ⠋ 31s
   · $ nvim src/main.rs
   web
   ○ ✳ web                    9m ✓
   +3 tabs
 l30n4rd0d4v1nc1 · ssh
   ⠋ ✳ infra                   1m
 ─ earlier ─
   ● ✳ docs                  done
 ────────────────────────────────
 2 working · 1 need you
 ↵ jump  n next  p prev
```

<p align="center">
  <code>◆ blocked</code> · <code>⠋ working</code> · <code>○ idle</code> ·
  <code>✗ API error</code> · <code>✓ exited</code>
</p>

Agents work for minutes, then stop on a permission prompt or finish without a
word. Spread over tabs, sessions and a server or two, you lose track of which
one is waiting. zj-herdr finds them all, puts them on one rail in the Zellij you
already run, and gives you a socket so agents can wait on and prompt each other.

## Quick start

```sh
# 1. The CLI and daemon. Archives exist for x86_64/aarch64 Linux (musl) and
#    macOS; swap the target to match.
curl -fsSL https://github.com/dynamotn/zj-herdr/releases/latest/download/zj-herdr-x86_64-unknown-linux-musl.tar.gz \
  | tar -xz -C ~/.local/bin zj-herdr

# 2. Look before you commit: one scan of this machine, no daemon needed.
zj-herdr scan
```

Then add the plugin to `~/.config/zellij/config.kdl` and let it start the
daemon for you:

```kdl
plugins {
    zj_herdr location="https://github.com/dynamotn/zj-herdr/releases/latest/download/zj_herdr.wasm" {
        start_daemon "true"
    }
}

keybinds {
    shared_except "locked" {
        bind "Alt a" { LaunchOrFocusPlugin "zj_herdr" { floating true; move_to_focused_tab true; }; }
        bind "Alt ]" { MessagePlugin "zj_herdr" { name "zj_herdr.cmd.v1"; payload "attention-next"; floating true; }; }
        bind "Alt [" { MessagePlugin "zj_herdr" { name "zj_herdr.cmd.v1"; payload "attention-prev"; floating true; }; }
    }
}
```

Restart Zellij, grant the permissions once, and press `Alt a`. A pinned rail
for your layout and more keybinds are in [`docs/zellij.kdl`](docs/zellij.kdl);
building from source is in [`docs/development.md`](docs/development.md).

## What you get

- **Every agent, wherever it runs.** Claude Code, Codex, Opencode, Gemini,
  Copilot, Cursor, pi and aider, inside Zellij or outside it, on this machine
  or on hosts you reach over SSH.
- **Nothing to install in the agents.** State comes from the process tree and
  the transcripts agents already write, so a new agent version never needs a
  new hook.
- **Who needs you, and for what.** A blocked agent shows the tool it waits on
  (`↳ Bash: cargo test`); `Alt ]` jumps to it, across sessions.
- **The numbers at a glance.** Model, session token total, how full the
  context window is, and the background shells and subagents it left running.
- **Your panes too.** Builds, tests and dev servers in your panes get their own
  rows; editors and pagers stay quiet.
- **Pull agents in.** `⇲` moves an agent running outside Zellij into a new pane
  here, resumed where it was.
- **Survive a restart.** After Zellij resurrects a session, the rail offers
  `↻ restore N agents` and reopens each one with its resume command.

## How it works

```text
 agents (claude, codex, …) ──┐
 ~/.claude, ~/.codex ────────┼──▶ zj-herdr daemon ──zellij pipe──▶ rail in every session
 ssh host: zj-herdr scan ────┘         │
                                       └──▶ socket ◀── zj-herdr CLI · your scripts
```

Once a second the daemon scans processes for known agents, finds each one's
pane from `ZELLIJ_SESSION_NAME` and `ZELLIJ_PANE_ID` in its environment, and
derives a state:

| State | When |
|---|---|
| `blocked` | the transcript ends on a tool call with no result, and everything has been quiet for 5 s |
| `working` | the transcript moved in the last 5 s, or the agent's process tree uses CPU |
| `idle` | the turn ended and everything is quiet |
| `done` | the process exited (kept for 10 s) |

Claude Code and Codex get the full picture from their transcripts; the others
are read from CPU alone, so they show `working` or `idle`. Only the shape of a
transcript is read, never your prompts or the replies. The fine print is in
[`docs/detection.md`](docs/detection.md).

## Drive it from a script

The daemon serves newline-delimited JSON on a private socket, and the CLI wraps
it: `list`, `watch`, `wait`, `prompt`, `spawn`, `adopt`, `ack` and `restore`.
One agent can wait for another to need attention, then answer it:

```sh
zj-herdr wait dev:3 blocked --timeout 600 && zj-herdr prompt dev:3 "yes, go ahead"
```

Every command, the socket API and remote hosts are in
[`docs/cli.md`](docs/cli.md).

## How is this different?

| | [zj-radar](https://github.com/marktoda/zj-radar) | zj-herdr |
|---|---|---|
| State source | a hook installed in each agent CLI | process inspection plus the transcripts agents already write |
| Coverage | agents in the current Zellij session | every agent process on the machine, inside Zellij or not |
| Machines | local only | local plus remote hosts over SSH |
| Control | display | socket API and CLI: `list`, `wait`, `prompt`, `spawn`, `watch`, `adopt` |
| Restore | none | reopens agent sessions with their resume command |

The name and the socket-driven control come from
[herdr](https://herdr.dev/docs/); the rail borrows from zj-radar (the tab tree,
cards, the `earlier` list) and abtop (context bars, short model names, task
trees).

## Documentation

| Doc | What's in it |
|---|---|
| [`docs/rail.md`](docs/rail.md) | Reading the rail, buttons, plugin options, pipe commands, prompt delivery. |
| [`docs/zellij.kdl`](docs/zellij.kdl) | A complete plugin alias, keybinds and pinned-rail layout to copy. |
| [`docs/cli.md`](docs/cli.md) | The daemon, every CLI command, the socket API, remote machines, session restore. |
| [`docs/detection.md`](docs/detection.md) | How each state is derived, which transcripts are read, token counting, known limits. |
| [`docs/development.md`](docs/development.md) | Lints, tests, the end-to-end suites and the coverage gate. |
| [`docs/plan.md`](docs/plan.md) | The original design. |
| [`CHANGELOG.md`](CHANGELOG.md) | What changed in each release. |

## Development

```sh
cargo test    # fixture transcripts only, never your real ~/.claude
cargo build -p zj-herdr-plugin --target wasm32-wasip1 --release
```

CI holds formatting, `clippy -D warnings`, a real headless Zellij end-to-end
run and 95% line coverage; see [`docs/development.md`](docs/development.md).
