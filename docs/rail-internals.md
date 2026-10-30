# How the rail works

What the plugin does with a snapshot, in `crates/plugin/src/app.rs`,
`ui.rs` and `activity.rs`, and the pure logic in `crates/core/src/view.rs`.
What the user sees is in [rail.md](rail.md).

## From snapshot to tree

The daemon pushes `zj_herdr.state.v1` ([architecture.md](architecture.md)).
The plugin keeps the last one as `raw`. `refresh_view` turns it into what is
shown, on every snapshot, pane or session update, and animation tick:

1. Machines hidden with `✕` are dropped (a hidden machine comes back once it
   is reachable, or gone).
2. Background sessions of the Claude Code daemon, which have no pane of
   their own, are attached to the pane whose terminal title carries their
   session name (`attach_background`), replacing the client row there.
3. The commands this instance has seen in its session's panes, and a dim row
   for each terminal pane that has neither an agent nor a command, are added
   to the local machine as pseudo-agents (kinds `build`, `test`, `deploy`,
   `server`, `remote`, `command`, `pane`). They are not counted.

`view::tree` then builds the rows: machines (local first), then sessions,
then tabs, then agents.

- A machine gets one row; an unreachable one says why and offers `✕`.
- Within a machine, agents are ordered blocked first, then idle, working,
  unknown, done, the longest in a state first. Sessions come in the order
  their most urgent agent has; on the local machine the current session is
  first.
- A local session with a layout lists its tabs in position order and puts
  each agent under the tab whose panes include its pane, in pane id order. A
  tab row carries the state of its most urgent agent. Agents that fit no tab
  (outside Zellij, remote machines, a pane no tab lists) follow as plain
  rows under their session.
- In the current session a tab is shown when it is the active one or holds
  an agent; every other tab folds into one `+N tabs` row after the session's
  tabs. In other sessions only tabs with agents are shown.

`ui::render` lays the rows out as lines, each with the column ranges a click
on it hits (`Hits`), buttons before the row itself.

## Where tab names come from

The plugin takes tabs and panes from two sources:

- **Its own session**: `TabUpdate` and `PaneUpdate`, which Zellij sends to
  every plugin about its own session. These know the active tab.
- **Other sessions**: `SessionUpdate`, which lists every session's tabs and
  panes. Zellij does not always include every session in it.

For a local session that `SessionUpdate` leaves out, or lists without tabs,
`view::with_agent_tabs` builds a layout from the daemon's `pane.tab` on each
agent: `{"position","name"}`, filled by the daemon from
`zellij --session S action list-panes --json --tab` (every session that has
agents, at most every 5 s). Such a layout has only the tabs that hold
agents, and no active tab. The two sources are never mixed for one session:
a session with tabs from Zellij ignores `pane.tab`.

## The `↳ waiting` line

For a `blocked` agent, the detector sends `waiting`: the pending tool call's
name and, for a shell tool, its command, on one line of at most 60
characters ([detection.md](detection.md)). The agent's second line, the
details, holds model, tokens, context and place. The `↳` line is a third line
under it, drawn only while the agent is blocked and `waiting` is set. A click
on it is a click on the agent.

## Tab renaming

With `rename_tabs` on (the default), the plugin renames the tabs of its
session after the directory of their agents. `view::tab_renames` returns the
tabs to rename:

- Only a tab named `Tab #<n>` (Zellij's default) or the name this instance
  gave it before. A tab the user renamed is never touched, and a rename the
  plugin already made is not repeated.
- The name is the last component of the cwd of the first agent in the tab:
  the lowest pane id among agents that have not exited, have a cwd and run
  in a pane of this session.

The rename waits for the permissions, and only the instance whose own tab is
active acts. A session has a rail per tab, and all of them see the same
state; one actor avoids racing renames. The instance finds its own tab from
`PaneUpdate`, as the tab holding its plugin id, and the active tab from
`TabUpdate`. It remembers the names it gave by tab id, re-keyed by position
as tabs move.

## Header and footer tallies

Both count agents only. The header (`·N▲ M!`) shows the number of agents
(commands and bare panes are left out), `▲` when any tab was folded into
`+N idle ▾`, and in red `M!`, the agents that need you: blocked, or idle and
not marked seen. The footer tally (`N working · M need you`) uses the same
numbers, with `working` the agents in state `working`. The header is always
there; the footer, with the `earlier` list above it, fills the space the tree
leaves, and appears when three or more lines are free.

## Folding tabs

When the lines do not fit the pane, `render` folds whole tab cards, last
first, into one `+N idle ▾` row where the first of them was. A tab is never
folded if it is the active tab, holds the cursor, or holds an agent that
works or needs you. The header shows `▲` while anything is folded. The view
scrolls so the cursor row stays visible.

## Pinned rail and keys

A pinned rail is a pane in a layout. Once its permissions are answered and
its tab holds a terminal pane, it makes itself unselectable, so Zellij closes
the tab when the last terminal pane exits. Unselectable means no keys, so
keybinds drive it with pipe commands, and the mouse wheel moves its cursor.

Every plugin instance of a session whose URL and configuration match a
`MessagePlugin` receives it. `select-next`, `select-prev`, `jump`,
`session-next` and `session-prev` are acted on only by the instance whose own
tab is active (`acts_on_rail_command`); one that does not know its tab yet
acts as well, so a freshly launched instance is not deaf. Others, such as
`attention-next`, act in whichever instance gets them.

Zellij picks the targets by plugin URL and configuration, not by name. The
configuration is every key of the plugin block except those `MessagePlugin`
consumes itself (`name`, `payload`, `floating`). A bind that omits `pinned
"true"` therefore matches the unpinned alias instead, or matches nothing and
launches a new floating instance. So the bind repeats the rail's options
exactly.

A command that reaches a plugin which was not loaded makes Zellij launch it
floating. After the command it hides itself unless it is pinned, someone is
using it (a key was pressed since it was shown), or permissions are pending,
since that is where Zellij asks for them. An attention command that arrives
before any state is kept and runs when the first snapshot lands.

## Adopt

The `⇲` button is the only way to adopt. A click on it runs `zj-herdr adopt
--session=<this session> -- <id>`, and only if the agent is adoptable: local,
outside Zellij, with a session id, not `done`, not a background session.
`Enter` and clicks elsewhere on the row only select it and, for an agent in a
pane, jump.

The plugin has no armed state: nothing prevents a second click. The daemon
does. It keeps the set of pids being moved and refuses a second `adopt` of
the same pid with "already being moved". Behind a click, the daemon checks the
session runs and the pid is still that agent, sends SIGTERM, waits up to 5 s
for the agent and its native helper to exit, and only then opens the resume
command in a new pane ([cli.md](cli.md)). A failure shows as the last stderr
line in red under the header.

## The restore banner

`↻ restore N agents` shows when `zj-herdr restore --session=S --dry-run
--json` says it would reopen anything. The plugin runs that dry run once it
knows its session name and has permissions, then again each time it becomes
visible. The command's context carries the session, and an answer for a
session this instance has left is ignored. A failed or unparsable dry run
counts as nothing to restore.

Clicking the banner sets the count to 0 (so it hides at once) and runs
`zj-herdr restore --session=S`. The daemon's restore skips records whose
agent session is already running, so a second click, or a click on the
banner of another tab's rail afterwards, opens nothing twice. A resumed
Claude counts as running from the moment it starts, before it has written to
its transcript.

## Prompt delivery

`zj-herdr prompt` asks the daemon, which checks that the agent is local, in a
pane and running, appends a carriage return unless told not to, and runs
`zellij --session S action pipe --plugin <alias> --plugin-configuration
deliver_prompts=true --floating-plugin true --name zj_herdr.cmd.v1 --
<json>`. Zellij finds, or loads floating, the one instance with that
configuration, and the instance acts only because `deliver_prompts` is on. A
picker or pinned rail never types a prompt.

The instance calls `write_chars_to_pane_id` and answers on the CLI pipe:
`zj_herdr.prompt.delivered`, or `zj_herdr.prompt.failed: <reason>` when
`deliver_prompts` is off, the pane is gone, or permissions were denied. If the
permissions are still pending, the pipe stays blocked and the prompt is
queued until they are answered. The daemon reads the answer and treats
anything else, including none, as a failure.

## What the rail sees by itself

`CommandChanged` events give the foreground command of each pane of this
session. A command that is a shell, an interactive program or an agent is not
shown. Others become rows after running two seconds. Without an exit status
for a command typed at a shell, the pane returning to its prompt means done;
a `zellij run` pane's exit status marks failure. The `earlier` list takes
these commands, and agents that ended a turn after working ten seconds or
more; it keeps ten entries, a new one replacing an older one for the same
command or agent.
