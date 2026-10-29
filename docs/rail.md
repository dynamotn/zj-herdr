# The rail

The Zellij plugin: picker, pinned rail, options, pipe commands and prompt delivery.

[`docs/zellij.kdl`](zellij.kdl) has the plugin alias and keybinds:

- `Alt a` opens the floating picker. Its look borrows from zj-radar
  (the tab tree, cards, the `earlier` list), herdr (seen marks, the
  selected row lit) and abtop (context bars, short model names, task
  trees):
  - The header shows the agent count, `▲` when tabs are folded away, and
    in red how many need you (`zj-herdr  ·3▲ 1!`), over a `═` rule along
    which a `◆` sweeps while anything runs.
  - Agents of each local Zellij session sit under the tab that holds their
    pane: the current session first with its active tab (marked `▌`) and
    every tab that has agents, tabs without agents folded into one dim
    `+N tabs` line; other sessions with the tabs that have agents. Agents
    outside Zellij and on remote machines are grouped by host and session.
  - Each agent takes two lines. The first is its state glyph (`◆` blocked,
    a spinning `⠋` working, `○` idle, dim `○` idle and seen, `✗` its last
    turn hit an API error, `✓` exited), its kind as a mark (`✳` claude,
    `❉` codex, `✺` opencode, `✦` gemini, `✴` pi; other kinds by name), the
    project (its directory) in bold, and the time in that state flush
    right. The second keeps what fits of the model (`opus5.5`), the
    session's token total (`4.8M`), how full its context window is
    (`■■■□□ 62%`, `!` past 75%, `⚠` past 90%) and the pane (`#12`, `pid N`
    or `ssh:host`). A blocked agent adds `↳ <what it waits on>`.
  - Work an agent runs in the background (Claude's background shells and
    subagents) hangs under it on a `┊`: a spinner with its age while it
    runs (a steady `▸` for a dev server), then `●` done, `✗` failed or `○`
    killed for five minutes; at most three lines, a failure always shown.
    An agent whose turn ended while such work runs (a server aside) counts
    as working.
  - Commands run in this session's panes (`cargo test`, `npm run dev`, …),
    seen through Zellij's `CommandChanged` like zj-radar, get a row under
    their tab once they have run two seconds: `⚙` build, `⚗` test, `⇡`
    deploy, `❯` server (steady `▸`), `⇄` ssh, `$` anything else. Editors,
    pagers and shells are left out. A command typed at a shell has no exit
    status to read, so the pane returning to its prompt counts as done;
    a `zellij run` pane's exit status marks it `✗` when it failed.
  - Every other terminal pane, in every local session, gets a dim
    `· $ <pane title>` row under its tab, so a pane just opened (a floating
    one included) shows at once; clicking it focuses the pane. These rows
    are not counted and never need you. Commands are only seen in sessions
    with a zj-herdr rail (Zellij sends `CommandChanged` to its own
    session's plugins); elsewhere the pane title, which shells set to the
    running command, stands in.
  - The row under the cursor is marked `►` and lit; a row that just
    started needing you flashes for three seconds.
  - When the tree does not fit, idle tabs (nothing running, nobody
    waiting, not in view) fold into one `+N idle ▾` line, the last first.
  - Room left over fills from the bottom: with three lines free a footer
    (a rule, the tally `3 working · 2 need you`, and clickable `↵ jump  n
    next  p prev` buttons), and above it `─ earlier ─`, up to ten things
    that finished lately, newest first: agents that ended a turn and
    commands that ended.
  - `j`/`k` or arrows move, `Enter` jumps to the pane (switching session
    if needed), `n`/`p` cycle attention, `Esc` hides it. Click an agent,
    tab or session row to jump to it. Remote rows show their SSH target
    and do not jump.
- Rows end in buttons: `✓` on an idle agent marks it seen (`zj-herdr
  ack`), so it stops counting as needing you until it changes state; `⇲`
  on an agent that runs outside Zellij and has a session to resume runs
  `zj-herdr adopt <id> --session <this session>`, which moves it into a
  new pane here (stopping it where it ran) — only the button does this,
  `Enter` or a click elsewhere on the row only selects it; `✕` on an
  unreachable machine hides it until it is reachable again.
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
to panes (for `prompt`), read CLI pipes, and run commands (to fetch the
first snapshot with `zj-herdr list --json`, and for `zj-herdr adopt`,
`zj-herdr ack` and `zj-herdr restore`).
Plugin options:

| Option | Default | Meaning |
|---|---|---|
| `pinned` | `false` | stay open after a jump (for a pane in a layout) |
| `deliver_prompts` | `false` | type prompts sent to this instance; `zj-herdr prompt` sets it on its own instance, so leave it unset in layouts |
| `zj_herdr_bin` | `zj-herdr` | binary used for the first snapshot (and by `start_daemon`) |
| `start_daemon` | `false` | start `zj-herdr daemon` in the background once permitted, unless one runs already; it logs to `$XDG_STATE_HOME/zj-herdr/daemon.log` and outlives Zellij |
| `density` | `cards` | `cards`: each tab a card tinted from the terminal's own colours (needs truecolor), a blank line between cards; `comfortable`: the blank lines, no tint; `compact`: tabs flush |
| `rename_tabs` | `true` | name each tab of the current session after the directory of its first agent (in pane order), as long as the tab still has Zellij's default `Tab #<n>` name or the name this instance gave it; a tab you renamed is never touched. Only the instance in the active tab renames, so one rail per tab do not race |

Pipe commands (`zellij pipe --name zj_herdr.cmd.v1 -- <payload>`):
`attention-next`, `attention-prev`, `toggle`, `select-next`,
`select-prev`, `jump`, `session-next`, `session-prev` (switch to the next
or previous Zellij session by name), and `{"cmd":"prompt","pane_id":N,"text":"..."}`.

A pinned rail is unselectable, so it never gets keys; the mouse wheel over
it moves the cursor, and `select-next`,
`select-prev` (move the cursor) and `jump` (go to the agent under it) drive
it from keybinds instead. Every rail of the session receives them, and only
the one whose own tab is active acts. Zellij delivers a keybind's message
to the instances whose URL and configuration match, so the bind must repeat
the rail's options from the layout exactly (`name`, `payload` and
`floating` are not part of the configuration):

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
