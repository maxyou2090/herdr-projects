# hp — projects for herdr-core

Run a whole project across several coding agents without briefing each one by
hand. You talk to a **coordinator** agent for each line of work; the
coordinator starts a separate agent for each task in its own git worktree and
branch, gives every one of them the same instructions and memory, and
collects what needs you. A project can run several independent **lanes**, each
with its own coordinator, its own `TASKS.md` sections and its own inbox over the
same repositories, memory and settings.

`hp` is a client of [herdr-core](https://github.com/herdrdev/herdr-core): core
runs the terminals and keeps them alive, `hp` decides what to run in them and
keeps the project's files. There is **no interface**: core draws nothing and so
does this binary. Everything is a command, and the things a window would show are
printed:

```bash
hp overview law          # every thread grouped by what needs you
hp context law           # the digest the coordinator reads each turn
hp thread list law       # records with live state
```

## What it is

| Piece | What it does |
| --- | --- |
| `hp open <project>` | A coordinator agent in a terminal of the project's own Space; reuses a running one |
| `hp thread start` | A new agent in its own worktree and branch (or a tab, a checkout, a folder of its own) |
| `hp report` | An agent's own word on what it is doing; the ticker groups threads by it |
| `hp ticker` | One background loop per projects root: launches pending threads, sends briefs, watches reports, pull requests, routines and audits |
| `hp-agent` | A tiny wrapper core runs around an agent, so what it is doing shows up as OSC 7501 status: its life, plus `working`/`blocked` read from the pane for harnesses that print no status |

Everything a coordinator may do is a command: there is no hidden channel, and a
project keeps working from a terminal, a script or another agent.

## Install

```bash
git clone https://github.com/maxyou2090/herdr-projects
cd herdr-projects
sh scripts/install.sh          # cargo build --release, links hp and hp-agent into ~/.local/bin
hp doctor                      # checks core, tools, the root and every project
hp ticker start                # a LaunchAgent keeps the loop alive across reboots
```

Requirements: Rust 1.89 or later to build, `herdr-core` 0.1.21 or later (`hc` on
`PATH`), `git` and `gh` for the pull-request features.

Then create and open a project:

```bash
hp new billing --repo ~/dev/app
hp open billing
```

`hp new` writes `~/.herdr-projects/billing/` with `AGENTS.md` (which tells any
agent started there that it is the coordinator), `PROJECT.md`, `TASKS.md` and
`MEMORY.md`.

## Working with it

- **Talk to the coordinator** in its terminal. Ask for a task and it starts a
  thread; you answer questions and review reports instead of managing agents.
- **`hp overview <project>`** is the window that would have been: needs you,
  ready for review, working, landing, idle, resolved. `hp overview` without a
  project lists only the ones in use — a live coordinator, or a thread that is
  open or starting — and `hp overview --all` lists every project in the root.
- **A project is in use or deleted**, and nothing else. Not needing one for a
  while is closing its Space and running no coordinator there; working on it
  again is `hp open <slug>`, and a project that is really finished is
  `hp delete <slug>` (its folder goes to `.trash/`).
- **`hc attach <terminal>`** takes you to any pane; `hp open` prints it when a
  coordinator is already running.
- **The skill** (`hp skill`) is what the coordinator follows; it ships in
  `skill/` and is linked into `~/.agents/skills/autoproject` for harnesses that
  read that folder.

## Documents

- [`docs/getting-started.md`](docs/getting-started.md) — the first project, end to end
- [`docs/operations.md`](docs/operations.md) — how it works, what it writes, settings, cleanup
- [`docs/lanes.md`](docs/lanes.md) — several coordinators in one project
- [`docs/thread-space.md`](docs/thread-space.md) — where a thread's tab goes: the project's one thread Space
- [`docs/herdr-core-port.md`](docs/herdr-core-port.md) — the port itself: what was kept, rebuilt and deleted, the traps, and what is still open
- [`docs/history/`](docs/history/README.md) — the Herdr 1 write-ups, kept as history: the acceptance list and what the builder verified about that host

## License

MIT.
