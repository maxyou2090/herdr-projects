# Getting started

`hp` runs a project across several coding agents on top of
[herdr-core](https://github.com/herdrdev/herdr-core). This page takes you from a
fresh checkout to a thread that reports back.

## 1. Build and install

```bash
git clone https://github.com/maxyou2090/herdr-projects
cd herdr-projects
sh scripts/install.sh
```

`scripts/install.sh` builds `hp` and `hp-agent` with cargo and links both into
`~/.local/bin` (or `$XDG_BIN_HOME`). Nothing is installed into herdr-core: the
binary only calls `hc`.

Check the machine:

```bash
hp doctor
```

It reports the core it found (`hc 0.1.21` or later), the tools the features need
(`git`, `gh`, `rsync`, `ssh`), the projects root and every project's priming
files. `hp doctor --fix` repairs what it can: the `~/.local/bin/hp` link and the
`AGENTS.md` priming of each project.

Start the ticker, and let the machine keep it:

```bash
hp ticker start      # writes ~/Library/LaunchAgents/dev.hp.ticker.plist and loads it
hp ticker status
```

The plist has `RunAtLoad` and an unconditional `KeepAlive`: launchd starts the
ticker after a reboot and after any exit, so a crashed or killed loop comes
back on its own, and every exit leaves its reason in `~/.herdr-projects/.ticker.log`.
`hp doctor` has a `ticker agent` line for a plist that is missing, out of date
or not loaded; `hp doctor --fix` rewrites and reloads it.

## 2. Create a project

```bash
hp new billing --repo ~/dev/app
```

`~/.herdr-projects/billing/` now holds:

- `AGENTS.md` — read by any agent started in the folder: it is the coordinator,
  and it primes itself with `hp skill` and `hp context`
- `PROJECT.md` — name, repositories, profiles, how long a finished
  thread waits before it is resolved, whether the ticker nudges
- `TASKS.md` — the task list, written only through `hp task`
- `MEMORY.md` and `memory/` — what the project keeps between threads

Every file is plain text, outside git, and safe to read.

## 3. Open the coordinator

```bash
hp open billing
```

This starts an agent in a new terminal of the project's own Space, in the project
folder — or right in the terminal you ran it from, when that is a core terminal
with no agent in it (`--tab` always asks for a new tab). Run it again and it
focuses the running coordinator instead (`hp open
--new` starts a second one). `--lane <name>` opens another line of work with its
own coordinator, task sections and inbox.

The coordinator is an ordinary agent: talk to it in its terminal. It answers,
starts threads, and reads/writes the project's files.

## 4. Start a thread

Ask the coordinator for something ("add invoice PDF export"), or start it
yourself:

```bash
printf 'Add a PDF export for invoices.\n' > /tmp/task.txt
hp thread start billing --title "PDF export" --repo ~/dev/app --kind worktree --task-file /tmp/task.txt
```

The thread gets its own worktree and branch, the same instructions and memory,
and its own progress. `hp thread list billing` shows it; `hp overview billing`
groups it with everything else by what needs you.

Settled threads report into `threads/<id>.md` and land in the coordinator's
inbox; the ticker nudges the coordinator once when something new is waiting.

## What is not here

herdr-core draws nothing, so this binary has no sidebar, popup, action menu or
key bindings. The words Herdr 1's sidebar used are printed instead:

| Instead of | Run |
| --- | --- |
| the sidebar | `hp overview <project>` |
| the projects popup | `hp overview`, `hp thread list`, `hp task`, `hp routine list` |
| the popup's settings | `hp set`, `hp safety`, `hp profile`, `hp routine toggle` |
| clicking a pane | `hc attach <terminal>` (printed by `hp open`) |

## Next

- [`operations.md`](operations.md) — settings, safety, cleanup, other machines
- [`lanes.md`](lanes.md) — several coordinators in one project
