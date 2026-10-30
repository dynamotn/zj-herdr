# The rail

The Zellij plugin: picker, pinned rail, options, pipe commands and prompt
delivery. How it builds the tree is in [rail-internals.md](rail-internals.md).

[`docs/zellij.kdl`](zellij.kdl) has the plugin alias and keybinds:

- `Alt a` opens the floating picker. Its look borrows from zj-radar
  (the tab tree, cards, the `earlier` list), herdr (seen marks, the
  selected row lit) and abtop (context bars, short model names, task
  trees):
  - The header shows the agent count flush right, `▲` when tabs are folded
    away, and in red how many need you (`zj-herdr  ·3▲ 1!`), over a `═` rule
    along which a `◆` sweeps while anything runs. Commands and plain panes
    are not counted. Until the first snapshot arrives it reads
    `waiting for zj-herdr daemon…`, and with nothing to show, `no agents`.
  - Agents of each local Zellij session sit under the tab that holds their
    pane: the current session first with its active tab (marked `▌`) and
    every tab that has agents, tabs without agents folded into one dim
    `+N tabs` line; other sessions with the tabs that have agents. A tab
    line shows the state of its most urgent agent, its number and its name.
    Agents outside Zellij and on remote machines are grouped by host and
    session.
  - Each agent takes a line, and usually two. The first is its state glyph
    (`◆` blocked, a spinning `⠋` working, `○` idle, dim `○` idle and seen,
    `✗` idle with its last turn ended on an API error, `✓` exited, `?`
    unknown), its kind as a mark (`✳` claude, `❉` codex, `✺` opencode, `✦`
    gemini, `✴` pi; other kinds by name), the project (its directory) in
    bold, and the time in that state (`14s`, `2m`, `3h`) flush right. The
    second keeps what fits of the session's token total (`4.8M`), how full
    its context window is (`■■■□□ 62%`, `!` past 75%, `⚠` past 90%), the
    model (`opus5.5`) and the place (`#12`, `pid N` or `ssh:host`), in that
    order of priority. It is left out for an agent in a local pane when
    there is no model, token or context figure to show. A blocked agent adds
    `↳ <what it waits on>`.
  - Work an agent runs in the background (Claude's background shells and
    subagents) hangs under it on a `┊`: a spinner with its age while it
    runs (a steady `▸` for a dev server), then `●` done, `✗` failed or `○`
    killed for five minutes. A shell task carries the mark of its command
    (`⚙`, `⚗`, as below), a subagent `✳`. At most three lines are shown,
    running first, a failure always among them, and `+N more` for the rest.
    An agent whose turn ended while such work runs (a server aside) counts
    as working.
  - Commands run in this session's panes (`cargo test`, `npm run dev`, …),
    seen through Zellij's `CommandChanged` like zj-radar, get a row under
    their tab once they have run two seconds: `⚙` build, `⚗` test, `⇡`
    deploy, `❯` server (steady `▸`), `⇄` ssh, `$` anything else. Editors,
    pagers, shells and the agents themselves are left out. A command typed
    at a shell has no exit status to read, so the pane returning to its
    prompt counts as done; a `zellij run` pane's exit status marks it `✗`
    when it failed.
  - Every other terminal pane, in every local session, gets a dim
    `· $ <pane title>` row under its tab, so a pane just opened (a floating
    one included) shows at once; clicking it focuses the pane. These rows
    are not counted and never need you. Commands are only seen in sessions
    with a zj-herdr rail (Zellij sends `CommandChanged` to its own
    session's plugins); elsewhere the pane title, which shells set to the
    running command, stands in.
  - The row under the cursor is marked `►` and lit; a row that just
    started needing you flashes for three seconds.
  - When the tree does not fit, tabs that are idle (nothing running, nobody
    waiting, not in view, no cursor) fold into one `+N idle ▾` line, the
    last first.
  - Room left over fills from the bottom: with three lines free a footer
    (a rule, the tally `3 working · 2 need you`, and clickable `↵ jump  n
    next  p prev` buttons), and above it, if more lines are free,
    `─ earlier ─`: up to ten things that finished lately, newest first,
    with how long ago. They are commands that ended after running two
    seconds or more, and agents that ended a turn after working ten seconds
    or more.
  - `j`/`k` or arrows move, `Enter` jumps to the pane (switching session
    if needed), `n`/`p` cycle attention, `Esc` or `q` hides it. Click an
    agent, tab or session row to jump to it. Remote rows show their SSH
    target and do not jump.
- `Alt ]` / `Alt [` jump to the next / previous agent that needs you:
  blocked ones first, then idle ones not marked seen. Only local agents in a
  Zellij pane count. The cycle continues from the agent in the focused pane.
- `Alt }` / `Alt {` switch to the next / previous Zellij session, by name.

## Buttons

Rows end in buttons:

- `✓` on an idle agent marks it seen (`zj-herdr ack`), so it stops counting
  as needing you until it changes state.
- `⇲` on a local agent that runs outside Zellij and has a session to resume
  runs `zj-herdr adopt <id> --session <this session>`, which moves it into a
  new pane here (stopping it where it ran). Only the button does this;
  `Enter` or a click elsewhere on the row only selects it. A Claude Code
  background session has no button.
- `✕` on an unreachable machine hides it until it is reachable again.

A failed `ack`, `adopt` or `restore` shows its last stderr line in red under
the header until the next snapshot.

Tab names of other sessions come from Zellij, or from the daemon when
Zellij leaves a session out; see [rail-internals.md](rail-internals.md).

A pinned rail (`pinned "true"` in a layout) makes itself unselectable once
its permissions are answered, like Zellij's status bar, so closing the last
terminal pane still closes the tab and the session. The mouse wheel over it
moves the cursor.

## Plugin options

The plugin asks for permission to read and change application state, write
to panes (for `prompt`), read CLI pipes, and run commands (to fetch the
first snapshot with `zj-herdr list --json`, and for `zj-herdr adopt`,
`zj-herdr ack` and `zj-herdr restore`).

| Option | Default | Meaning |
|---|---|---|
| `pinned` | `false` | stay open after a jump (for a pane in a layout) |
| `deliver_prompts` | `false` | type prompts sent to this instance; `zj-herdr prompt` sets it on its own instance, so leave it unset in layouts |
| `zj_herdr_bin` | `zj-herdr` | binary used for the first snapshot, `adopt`, `ack`, `restore` (and by `start_daemon`) |
| `start_daemon` | `false` | start `zj-herdr daemon` in the background once permitted; a daemon that runs already makes the new one exit. It logs to `$XDG_STATE_HOME/zj-herdr/daemon.log` and outlives Zellij |
| `density` | `cards` | `cards`: each tab a card tinted from the terminal's own colours (needs truecolor), a blank line between cards; `comfortable`: the blank lines, no tint; `compact`: tabs flush. Any other value means `cards` |
| `rename_tabs` | `true` | name each tab of the current session after the directory of its first agent (the one in the lowest pane), as long as the tab still has Zellij's default `Tab #<n>` name or the name this instance gave it; a tab you renamed is never touched. Only the instance in the active tab renames, so one rail per tab do not race. `false` turns it off |

## Pipe commands

`zellij pipe --name zj_herdr.cmd.v1 -- <payload>`, where the payload is one
of `attention-next`, `attention-prev`, `toggle`, `select-next`,
`select-prev`, `jump`, `session-next`, `session-prev` (switch to the next
or previous Zellij session by name), or JSON: `{"cmd":"attention-next"}`
for any of those, and `{"cmd":"prompt","pane_id":N,"text":"..."}`. An
unknown payload shows an error in the rail.

When Zellij launches the plugin floating just to deliver a command, it
hides itself again afterwards, unless it is pinned, the user is using it, or
permissions are still being asked. `toggle`, `select-next` and `select-prev`
leave it as it is.

A pinned rail is unselectable, so it never gets keys; the mouse wheel over
it moves the cursor, and `select-next`, `select-prev` (move the cursor) and
`jump` (go to the agent under it) drive it from keybinds instead. Every rail
of the session receives `select-next`, `select-prev`, `jump`,
`session-next` and `session-prev`, and only the one whose own tab is active
acts. Zellij delivers a keybind's message to the instances whose URL and
configuration match, so the bind must repeat the rail's options from the
layout exactly (`name`, `payload` and `floating` are not part of the
configuration):

```kdl
bind "Ctrl Alt j" {
    MessagePlugin "zj_herdr" {
        name "zj_herdr.cmd.v1"
        payload "select-next"
        floating true          // if nothing matches, launch floating, not tiled
        pinned "true"          // the rail's own options, as in the layout
        deliver_prompts "false"
    }
}
```

[`docs/zellij.kdl`](zellij.kdl) has all three binds next to the layout
they match.

## Prompt delivery

`zj-herdr prompt` runs `zellij --session S action pipe --plugin zj_herdr
--plugin-configuration deliver_prompts=true --floating-plugin true` in the
agent's session. Zellij matches plugin instances by URL and configuration,
so this reaches one dedicated instance (loaded floating the first time, and
hidden again afterwards) and never the picker panes, which would each type
the prompt again. The plugin answers `zj_herdr.prompt.delivered` once the
text is typed, or `zj_herdr.prompt.failed: <reason>` if the pane is gone or
the instance has `deliver_prompts` off; without an answer the command fails
rather than report success. The first time the plugin loads in a session,
Zellij asks for its permissions in the floating pane, and the prompt waits
for them (it fails at once if they are denied). Set `plugin = "..."` at the
top of `config.toml` if your alias or plugin URL differs.
