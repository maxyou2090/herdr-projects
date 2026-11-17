# Operations and development

How hp works, what it writes where, and what its safety settings do and don't stop.

Read [`herdr-core-port.md`](herdr-core-port.md) for why the binary is shaped this way
and what is still open.

## How it works

- **It relies on herdr-core (`hc`) and nothing else.** No plugin is needed or called. Pull requests open in your browser, text files open in this terminal's `$EDITOR`.
- **A project is a folder.** `~/.herdr-projects/<slug>/` holds `AGENTS.md`, which tells any agent started in that folder that it is a coordinator and which commands to run. `CLAUDE.md` is a link to it. One project can run several independent lines of work — **lanes** — each with its own coordinator instance, its own `TASKS.md` sections (`## <lane>/<list>`) and its own inbox; the project's repositories, worktrees, branches, pull-request flow, safety settings, memory and instructions stay shared. A project with no `lanes.json` keeps the old single-coordinator behaviour. [`lanes.md`](lanes.md) describes the model; `open --lane` starts a lane.
- **The coordinator is an ordinary agent** following a skill (`hp skill` prints it). The binary does not route messages, plan work or decide anything.
- **The binary does mechanics.** Starting a thread, copying reports, cleaning up after a resolve: each is one deterministic subcommand. It talks to core through `hc`, and everything core does not have — a Space, a tab, an agent — it builds out of core terminals and labels (`hp_ws`, `hp_tab`, `hp_agent`).
- **Agents report their own progress.** `hp report --activity "..." [--done]`, run by the agent in its own terminal, writes one small JSON file per pane under `<root>/.progress/`; the ticker uses it for a thread's group and five minutes of activity. There is no percentage. Thread briefs and the coordinator skill carry the instructions, so any agent can report, and nothing has to be installed for it. herdr-core reads a program's own status from OSC 7501 too.
- **Files are the record, prompts are nudges.** Threads write a report file, the ticker writes events to an inbox folder, and the coordinator reads state with `context` at the start of every turn. A missed prompt loses nothing.
- **One ticker per projects root** checks every 15 seconds: coordinators (any agent in a project folder, also one started by hand in a project never opened), thread state and groups, the labels a pane carries, pending prompts, changed reports, pull requests (every two minutes), the registry audit (every 30 minutes), routines, auto-resolve.
- **Tools are found even under a bare `PATH`.** The binary appends `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin` and `~/.cargo/bin` to its own `PATH`, so a ticker started by launchd finds `gh` and `rsync`.
- **Cleanup is part of the flow, never forced.** Resolving a thread removes its worktree (Herdr and git refuse a dirty one, and the plugin never forces) and, once its pull request is merged, its local branch. Reports and library files always stay. Text from reports, pull requests and command output is never placed in a prompt.

## Where things live

```
~/.herdr-projects/<project>/
  PROJECT.md              settings (TOML between +++ lines) and your standing instructions
  AGENTS.md, CLAUDE.md    who is the coordinator, by working directory; written by the binary
  MEMORY.md, memory/      project memory; the coordinator's
  memory/decisions.md     why work was not started, or was handed to another lane;
                          append-only and shared by every lane's coordinator, and
                          never inlined into a brief (only MEMORY.md is)
  TASKS.md                the task list; shared `## <list>` and per-lane `## <lane>/<list>` sections, with optional indented notes
  routines/<name>.md      routines, including pr-followup.md and coordinator-sweep.md; the coordinator's
                          a scheduled prompt fires only while something needs a
                          person: a thread waiting on you or ready for review, or
                          an unhandled inbox item; `routine list` counts quiet
                          runs, and editing the prompt re-arms it
  uploads/                files you give the threads
  scratch/                the coordinator's temporary files
  threads/<id>.toml       thread record          threads/<id>.md       home copy of its report
  threads/<id>.task.md    the task and every forwarded prompt (## Follow-ups): an entry
                          marked `(the user's own words, forwarded unchanged)` is the
                          user's message as they wrote it, one without the mark is the
                          coordinator's own words
  threads/<id>.next.md    Next lines the coordinator added    threads/<id>/  a no-repo thread's folder
  inbox/, inbox/done/     events for the coordinator
  library/<id>/           home copy of files a thread produced
  .state/                 status, coordinator record, live coordinators, ticker state, lock
  .state/lanes.json       the lane registry (exists only once a project has lanes)
  .state/inbox-seen/<lane>.json  each lane's read state; a project without lanes keeps inbox-seen.json
  .state/ticker.json      ticker state; with lanes, `nudged`, `renudged_at`, `box_pane` and
                          `box_empty_since` are maps keyed by lane id instead of one value
~/.herdr-projects/.ticker.lock  .ticker.log  .progress/  .trash/
~/.config/herdr-projects/config.toml             yours: root, profiles, safety tables
~/.config/herdr-projects/approved-routines.json  written only by `routine approve`
```

Every thread works from `<its working directory>/.herdr-project/<project>-<id>/`: `brief.md` (written by the binary), `report.md` and `library/` (written by the agent). In a git repository that folder is in `info/exclude`, so nothing in it is committed. Git therefore treats it as clean and removing a worktree deletes it, which is why a resolve keeps the worktree when the final copy home was partial.

`PROJECT.md` settings, all changeable from chat or with `hp set <project> <key> <value>`: `name` (the workspace label), `goal`, `repos` (`repos.add PATH`, `repos.remove PATH`), `coordinator_profile` and `thread_profile` (the default [profiles](#agent-profiles); the old names `coordinator_agent` and `thread_agent` still read), `max_parallel_threads` (1000, no practical cap), `auto_resolve_days` (7), `nudge` (`true`).

## Commands

| Command | What it does |
| --- | --- |
| `new <name> [--goal] [--repo PATH[@MACHINE]]... [--thread-profile P] [--coordinator-profile P]` | Create a project folder. The profiles default to `[defaults]` in config.toml, else `claude`. |
| `open <project> [--profile P] [--new] [--lane NAME] [--lane-name LABEL] [--session N] [--tab] [--rebind]` | A coordinator agent in a terminal of the project's own Space. Run it from a shell in one of the session's core terminals and no agent occupies that terminal, the coordinator starts **in that terminal**; `--tab` forces a new tab instead. A running coordinator is focused instead. `--lane` opens or focuses that lane's coordinator and creates the lane when it does not exist; `--lane-name` sets a new lane's label. |
| `context <project> [--peek]` | The digest the coordinator reads every turn; with lanes, the shared part plus the reader's own lane. |
| `task add <project> <list> "<title>" [--owner O] [--lane L] [--notes T]`, `task move <project> "<title>" <list>`, `task done <project> "<title>"`, `task delete <project> "<title>"`, `task thread <project> "<title>" <thread-id> [--owner O]` (or `--clear` to remove the id) | Safe `TASKS.md` writers: one locked read-modify-write, every byte outside the changed task preserved. `--lane` writes `## <lane>/<list>`; without it the list is shared. `task delete` removes the line and its notes, keeping an empty heading; `task thread` records the owner and thread id on that one line. |
| `assignable <project> [--refresh] [--check OWNER]` | Who TASKS.md tasks may be assigned to (see [Task owners](#task-owners)); `--check` refuses an owner that is not valid. |
| `coordinator prompt <project> --text-file F` | A sentence to the coordinator. |
| `thread start <project> --title T [--repo PATH] [--kind worktree\|tab\|checkout\|home] [--profile P] [--base REF] --task-file F [--from-task TITLE]` | New thread; `-` reads the task from standard input. `--kind` is the placement (default: worktree with `--repo`, home without; `tab` gives the thread a Space of its own; `checkout` gives it a Space of its own on the repo's main checkout; `home` is a tab of the project's own Space on `threads/<id>/`); `--profile` is the agent, one the project allows. `--from-task` adds a TASKS.md task's notes and takes `--profile` from its owner. |
| `thread prompt [--verbatim]`, `thread next [--line N \| --add TEXT]`, `thread stop`, `thread restart [--profile P]` | Steer a thread. Prompts are recorded in its task file; `--verbatim` marks one as the user's own words, forwarded unchanged, so a later reader can tell it from the coordinator's own text. |
| `thread read [--lines N]`, `thread keys [KEY]... [--text T]`, `thread brief` | Answer a thread's pane without going there: print what it shows (a trust dialog, a question menu, a permission prompt), then type text and press keys (`up`, `down`, `enter`, `esc`, `tab`, a digit). `thread keys` refuses a trust screen when `trust_screens` is `user`. `thread brief` sends a brief the ticker has not delivered yet (the ticker waits for a settled, empty input box, counts a brief sent only once the agent starts working, never types it twice, and after three unconfirmed tries leaves an inbox item). |
| `thread list/show [--json]`, `thread ack`, `thread adopt` | Look at threads. |
| `thread audit [<project>] [--json]` | Cross-check the thread registry against the core's terminals, read only (see [Cleanup](#cleanup)): a record that says resolved while its agent or pane is still there, an open record whose pane is gone and which cannot restart itself, and an agent that says `working` while neither its report nor its record has moved for 90 minutes. Prints `nothing unusual` and exits 0 when there is nothing; findings also exit 0, so a sweep can call it for every project. The ticker runs the same check every 30 minutes by itself and writes one `thread-audit` item per finding, so a quiet project is audited too. |
| `thread resolve [--keep-worktree] [--discard-uncopied] [--skip-copy] [--reopen]` | Final copy home, then clean up. Archives the thread's pending inbox items, and refuses to call the close clean when its agent is still there (see [Cleanup](#cleanup)). |
| `sweep <project> [--dry-run] [--yes]` | Remove what nothing uses any more. |
| `set <project> <key> <value>`, `routine list/toggle/approve`, `safety show/yolo/set` | Settings, routines and safety (see below). |
| `profile list [--project P \| --names]`, `profile resolve [NAME]`, `profile add/edit/remove`, `profile allow threads\|coordinator NAMES... [--project P] [--all]`, `profile default threads\|coordinator NAME` | [Agent profiles](#agent-profiles). Everything but `list` and `resolve` needs a person at a terminal. |
| `pause`, `resume`, `archive`, `unarchive`, `delete [--force]` | Project lifecycle. |
| `rename <project> <new-slug> [--name NAME] [--dry-run]` | A new slug (folder name), and with `--name` a new display name. Refused while a thread is not resolved. While agents run in the project folder (its coordinator, which may run it itself) the ticker takes over: once they are idle (at most 5 minutes) it closes their panes, renames, reopens the coordinator in the new folder resuming its conversation, and prompts it with a note; `thread start` is refused meanwhile. The folder moves in one step; the thread records, the coordinator record, `AGENTS.md`, the project's `[safety]` table (yolo, profile lists), routine approvals and the home Space's label follow it. Branches and worktrees keep `hp/<old>/` and `sweep` still finds them; the coordinator's conversation follows (Claude Code's transcript is copied to the new folder; other harnesses resume by id, one that cannot starts anew). It lists what it could not update (git refs), then runs `doctor`. Run it again to finish one that stopped halfway. |
| `overview [project]` | Every thread grouped by what needs you, in this terminal. |
| `report --activity TEXT [--done]`, `progress [--pane ID]` | An agent's own word on what it is doing, and the record of it. The bundled `autoproject` skill is linked once by hand in `~/.agents/skills/`. |
| `open-file <path>`, `open-url <url>` | Open a text file in this terminal's `$EDITOR`, or a PR in the browser. |
| `ticker start \| run \| stop \| status`, `doctor [--fix]`, `skill` | Housekeeping. `ticker start` installs a LaunchAgent (`RunAtLoad` plus an unconditional `KeepAlive`, so a reboot and *any* exit — including the five-minute idle exit — start the loop again; an outdated plist is rewritten and reloaded). Every exit writes its reason to `.ticker.log`, panics included. `doctor` has a `ticker agent` line for a plist that is missing, out of date or not loaded, and `doctor --fix` rewrites and reloads it. `doctor --fix` also relinks `~/.local/bin/hp` and repairs priming files; a file or another link is left alone. | |
| `update [--check]` | Nothing to fetch: this binary is built from a checkout. Prints the build's version and checkout, then `git pull`, `cargo build --release --locked`, `ticker stop && ticker start`; `--check` prints the same and changes nothing. |

## Lanes

A lane is one line of work inside a project: its own coordinator pane, its own `TASKS.md` sections and its own inbox, over the project's shared repositories, worktrees, branches, memory and safety settings. A project without `.state/lanes.json` behaves exactly as it did before lanes.

| Command | What it does |
| --- | --- |
| `open <project> --lane <name> [--lane-name <label>]` | Open or focus that lane's coordinator; a missing lane is created. No `--lane`: the default lane, the old behaviour. |
| `lane list <project> [--json]` | Every lane, its status and default, the rule each live pane matched, thread and inbox counts, and any open lane whose coordinator is missing. |
| `lane adopt <lane> [--project <slug>]` | Claim the current pane as that lane's coordinator (run it in the pane). For a tab you started by hand. |
| `lane assign <lane> <pane> [--project <slug>]` | The explicit handover: point the lane at another coordinator pane. The only command that moves ownership. |
| `lane rename <lane> <label> [--name <name>] [--project <slug>]` | Change the display label and optionally the CLI handle; the id never changes. |
| `lane close <lane> [--project <slug>]` / `lane open <lane> [--project <slug>]` | Retire a lane once its threads are resolved / reopen it. `close` never kills the coordinator and `open` never starts one. |

**Lane ids are unique inside one project, not across projects** — every project's first lane is `c-1`. So `lane adopt/assign/rename/close/open` take `--project <slug>`; without it the binary uses the project whose home the current pane works in, else the one project that has that lane, and refuses when several do. `lane list` and `task` take the project as their positional `<project>` argument.

`hp task` is the only safe writer for `TASKS.md`: a lane's coordinator writes its own `## <lane>/<list>` sections and the shared `## <list>` sections, never another lane's. The skill enforces the convention; the command enforces the lock.

## Groups

Every thread is in one group, shown by `hp overview` and in the digest, needs-you first:

1. **Waiting on you** (`needs you`): a failed start, a pane that is gone before a report, a launch stuck on a dialog, the agent blocked on a question or permission for 30 seconds, or the agent's own report `Waiting for you` while it is not working.
2. **Ready for review** (`review`): a report you haven't acknowledged, or a report with an open pull request, while the agent is not working.
3. **Landing**: an open pull request that is approved.
4. **Working**: the agent works, a launch is under way, or the agent reported progress under 100% in the last five minutes.
5. **Idle**, then **Resolved**.

Threads idle for `auto_resolve_days` are resolved (and cleaned) after a final copy home.

## No interface

herdr-core draws nothing and this binary adds no drawing: there is no sidebar, no
popup, no action menu and no key binding. `hp overview <project>` prints the same
grouping in a terminal, and `hc attach <terminal>` takes you to a pane.

## Safety settings

They live in `~/.config/herdr-projects/config.toml`, per project and as an all-projects default; a project's own value wins, then the default, then the built-in value. Change them in a terminal; `safety show <project>|--global` prints them and where each comes from.

```sh
hp safety yolo billing on                       # or off, or default (use the all-projects value)
hp safety yolo --global on                      # every project without its own value
hp safety set billing routine_commands on
hp safety set billing start_threads default
```

`safety yolo` and `safety set` refuse unless standard input and output are a terminal, and ask `y/N` before writing. What they write:

```toml
[safety.default]                   # all projects
yolo = true

[safety."/Users/you/.herdr-projects/billing"]
yolo = false                       # on: threads start without asking, agents skip permission prompts
start_threads = "propose"          # or "auto": the coordinator starts threads without asking
trust_screens = "user"             # or "coordinator": who answers trust screens in thread panes (unset: follows yolo)
coordinator_agent_args = []        # extra arguments for every coordinator's agent CLI
thread_agent_args = []             # extra arguments for every thread's agent CLI
routine_commands = false           # true lets approved routines run shell commands
```

**Yolo mode** is the one switch for "never stop to ask": `start_threads` becomes `auto` whatever it says, and every coordinator and thread launches with its own harness's skip-permissions flag, added after the profile's own arguments: Claude Code `--dangerously-skip-permissions`, Codex `--dangerously-bypass-approvals-and-sandbox`, Gemini CLI and Qwen Code `--yolo`, Cursor Agent `--force`, OpenCode `--auto`, Copilot CLI `--allow-all-tools`, Amp `--dangerously-allow-all`, Pi nothing (it never asks). Other kinds have no known flag: their agents still ask (`safety show` says so for the project's kinds). Routine commands are not part of yolo mode: a routine command runs with no agent in the loop, so it stays behind `routine_commands` and a per-command approval.

**Trust screens** are an agent's "trust this folder?" dialog, its restricted-folder chooser, and its hooks or settings review (Codex's "Hooks need review"). The harness saves the answer for every later session in that folder, so `trust_screens` says who gives it: `coordinator` (it answers them with `thread keys` for the thread's own folder) or `user` (it tells you, and `thread keys` refuses while one shows). Unset, it follows yolo mode: on, the coordinator; off, you. Whatever the setting, nothing is typed into a trust screen: a thread's brief waits, and `thread prompt` refuses, while one shows, even when Herdr reads the pane as idle, and the thread shows `needs you`.

A change reaches agents launched after it. Running agents keep the flags they started with until restarted: `hp thread restart` a thread, or quit the coordinator and `open` it again (it resumes its session). A running coordinator sees a new `start_threads` at its next `context`.

`thread_agent_args` and `coordinator_agent_args` are from before profiles. They were written for the harness of the project's default profile, so they now reach only a built-in profile of that harness: Claude flags stay with Claude threads and no longer break a Codex thread. Move them into a profile of your own (below) and remove them.

**When the old arguments reach a launch, and when they are dropped.** One condition decides it (`profiles::launch_args`): the profile being launched has no `[profiles.NAME]` table of yours — it is a **built-in** — *and* its harness is the one the project's default profile names (`thread_profile` for threads, `coordinator_profile` for coordinators). With `thread_profile = "claude"` and `thread_agent_args = ["--dangerously-skip-permissions"]`, a thread launched as the built-in `claude` gets the flag; a coordinator launched as the built-in `claude` gets `coordinator_agent_args`. They are silently dropped for a thread on another harness (the built-in `codex`), for a thread on a profile of your own (`[profiles.deep]` with `agent = "claude"`: a profile carries its own `args`), and for a coordinator whose default profile names another harness — which is why adding a flag and seeing no effect usually means the launch went to a different harness or to a named profile. The old arguments come first on the command line, then the profile's own `args`; yolo mode's flag comes last. A running agent keeps the flags it started with, so a change shows up at the next `thread restart` or `open`. `hp context`'s `Safety:` line prints the values exactly as configured, `thread_agent_args=["--dangerously-skip-permissions"] coordinator_agent_args=[]`, and does not say whether any launch used them; `hp profile resolve NAME` prints a profile's own arguments only. The durable fix is the same for both cases: put the flags in a profile (`args = ["--dangerously-skip-permissions"]`) and start threads and coordinators with that profile.

## Agent profiles

A profile is a named launch setup: a harness (any Herdr agent kind), a model, a reasoning effort, extra arguments and a one-line description. The coordinator only ever chooses a profile by name (`thread start --profile deep`, `open --profile claude`), and `context` lists the ones it may use with their descriptions, so it can pick one per task. It can never pass a launch flag itself: `--agent-arg` is gone. Yolo mode still adds the harness's skip-permissions flag on top of a profile's arguments.

```toml
[profiles.luna]
agent = "omp"
args = ["--config", "~/.omp/agent/luna.yml"]   # `~/` is expanded for local agents
description = "Cheap tier for small, clear tasks"

[profiles.deep]
agent = "codex"
model = "gpt-5.5"
effort = "high"
description = "Hard debugging and design"

[profiles.claude]                  # replaces the built-in `claude`
agent = "claude"
args = ["--add-dir", "~/dev/shared"]

[defaults]                         # what `new` writes into a new PROJECT.md
thread_profile = "claude"
coordinator_profile = "claude"

[safety.default]                   # every project without its own list
thread_profiles = ["claude", "luna", "deep"]   # absent: every profile
coordinator_profiles = ["claude"]

[safety."/Users/you/.herdr-projects/billing"]
thread_profiles = ["luna"]         # this project's own list wins
```

- **Built-ins.** Every Herdr agent kind is a profile of the same name with no arguments, so `thread_profile = "codex"` works with no config at all. Lists show the built-ins whose CLI is on `PATH` and looks signed in. Herdr exposes neither (its `integration.list` sees only hook files and some binaries), so the binary checks itself, offline: credential files and variables such as `~/.codex/auth.json`, `~/.claude.json`'s account, `~/.gemini/oauth_creds.json`, `CURSOR_API_KEY`. A kind with no known check counts once installed.
- **Model and effort** become the harness's own flags: `--model NAME` for every harness; effort as `--effort` (Claude Code: low, medium, high, xhigh, max; Copilot CLI: none to max), `-c model_reasoning_effort="…"` (Codex: none to ultra), `--thinking` (pi, oh-my-pi: off to max). Cursor puts effort in the model id (`gpt-5.6-sol-xhigh`), and Gemini CLI and OpenCode have no launch flag for it: use the model id or `args`.
- **Allow-lists.** A project's own `thread_profiles` / `coordinator_profiles` wins, then `[safety.default]`'s, then every profile. A profile off the list is refused at `thread start`, `thread restart` and `open`, and the ticker checks again at every launch: a thread whose profile you removed or disallowed since fails with the reason and an inbox item. The project's defaults (`thread_profile`, `coordinator_profile` in PROJECT.md) must be allowed too; `doctor` says when one is not.
- **Who changes what.** Profiles and lists live only in config.toml, which the coordinator never writes. `profile add/edit/remove/allow/default` refuse without a person at a terminal, as `routine approve` does; `profile add/edit/remove` and `profile allow` write them. The default profiles in PROJECT.md are ordinary settings the coordinator may change when you ask, only to an allowed profile. This is a soft boundary: an agent that fakes a terminal or edits config.toml by hand is stopped only by its own permission prompts.
- **Old threads and coordinators.** A thread started before profiles keeps its harness and its stored model flag. `open` resumes or reuses a coordinator only when it runs the same profile; one started before profiles counts as the built-in of its kind.

## Task owners

Each task in `TASKS.md` has at most one owner, in brackets after its title. The thread running it comes after, as status:

```
- [ ] Write the release notes (me)
- [ ] Rename the settings keys (codex-fast)          a profile, on this machine
- [ ] Look into Safari logouts                       unassigned
- [ ] Review the contract (Priya)                    a person, only when you name them
- [ ] Fix login (claude) · t-0007                    delegated: t-0007 runs it
```

- **Agent owners.** The coordinator assigns only the names on the `Assignable:` line of `hp context`: the thread profiles this project allows on this machine. `hp assignable <project>` prints them again, and `assignable --check "<owner>"` says whether one is valid.
- **Checked owners.** The coordinator checks a profile owner with `assignable --check` before writing it and refuses one that is not valid.
- **People.** `me`, or any name or text you give (`(Elias)`, `(Priya Rao)`), is written only when you name that owner. A bare name that is no profile here is a person: shown as written and never delegated.
- **Delegating.** `thread start --from-task "<title>"` starts the thread with the owner's profile (`--profile`); a flag that contradicts the owner is refused. An `@machine` owner from an older `TASKS.md` is refused by name: threads only start on this machine, so change the owner to a profile here (or `me`).
- **Old lines.** `(agent)` reads as unassigned and `(agent → t-0007)` as this machine's default profile with thread t-0007.

## The allow-list for your coordinator

The coordinator runs the binary every turn, so allow-list it in your agent by subcommand, never the bare binary. `context` prints the exact prefix (`Commands: <binary> --root <root>`); the patterns must start with it. For Claude Code, in the project folder's `.claude/settings.local.json`:

```json
{ "permissions": { "allow": [
  "Bash(<binary> --root <root> skill:*)",
  "Bash(<binary> --root <root> context:*)",
  "Bash(<binary> --root <root> assignable:*)",
  "Bash(<binary> --root <root> report:*)",
  "Bash(<binary> --root <root> inbox done:*)",
  "Bash(<binary> --root <root> list:*)",
  "Bash(<binary> --root <root> lane list:*)",
  "Bash(<binary> --root <root> task add:*)",
  "Bash(<binary> --root <root> task move:*)",
  "Bash(<binary> --root <root> task done:*)",
  "Bash(<binary> --root <root> task delete:*)",
  "Bash(<binary> --root <root> task thread:*)",
  "Bash(<binary> --root <root> routine list:*)",
  "Bash(<binary> --root <root> thread list:*)",
  "Bash(<binary> --root <root> thread show:*)",
  "Bash(<binary> --root <root> thread audit:*)",
  "Bash(<binary> --root <root> thread prompt:*)",
  "Bash(<binary> --root <root> thread next:*)",
  "Bash(<binary> --root <root> thread read:*)",
  "Bash(<binary> --root <root> thread brief:*)",
  "Bash(<binary> --root <root> thread keys:*)",
  "Bash(<binary> --root <root> thread ack:*)",
  "Bash(<binary> --root <root> thread restart:*)"
] } }
```

- Allow `thread start` only where you've set `start_threads = "auto"`. Left off the list, every thread start meets your agent's own permission prompt.
- With `thread keys` on the list, the coordinator answers a thread's questions and permission prompts itself, by the skill's rules, and its trust screens when `trust_screens` is `coordinator`. Remove it to confirm each answer first.
- With lanes, the split is: **allow** `context`, `lane list`, `task add`, `task move`, `task done`, `task delete`, `task thread` and the thread's read-only commands; **keep behind your agent's permission prompt** `lane adopt`, `lane assign`, `lane close`, `lane open`, `lane rename`, `thread resolve`, `sweep` and `routine approve`. The first group is read plus safe write; the second changes ownership or lifecycle — the same level as `thread resolve`. The thread steering commands in the list above (`thread prompt`, `thread next`, `thread keys`, `thread ack`, `thread restart`) stay allowed as before.
- Never allow `lane adopt`, `lane assign`, `lane close`, `lane open`, `lane rename`, `thread resolve`, `sweep`, `delete`, `rename`, `archive`, `routine approve.

## What the safety settings do and don't stop

- **They are soft.** Agents have a shell. The guards are the skill text, your agent's permission prompts, keeping `config.toml` and approvals outside every agent's working directory, and `routine approve`, `safety yolo` and `safety set` refusing without a terminal and a typed confirmation. An agent's shell commands have no terminal, so it cannot flip them by running the CLI; one that fakes a terminal (`script`) or edits `config.toml` directly is stopped only by its own permission prompts, which yolo mode turns off.
- **The coordinator answers threads' prompts.** With `thread keys` it approves plainly in-task permission prompts once, accepts trust screens for the thread's own folder when `trust_screens` is `coordinator`, and asks you about the rest. That is the skill's judgement, not a hard rule (except that `thread keys` refuses trust screens when `trust_screens` is `user`); take `thread keys` off the allow-list to see each one first.
- **A thread can impersonate you.** Any thread agent can prompt the coordinator's pane through Herdr. The skill's rule that a go-ahead must name the threads lowers the risk; it does not remove it.
- **An approved routine command covers the command text only.** `./check.sh` keeps its hash while the script changes.
- **Prompt injection is reduced, not removed.** No GitHub text reaches a prompt from the plugin, but threads read pull request comments themselves with `gh`, and memory is inlined into every later brief.
- **Cost.** Every thread is a full agent session, and each nudge and each `context` spends coordinator tokens.

## Nudges

- **Nudges** (`nudge = true`, the default for new projects) prompt a coordinator with one line saying what happened (`[hp inbox] t-0040 PR merged; t-0043 blocked on a prompt`) once a set of new inbox items arrives. On Herdr 0.9.1 a prompt merges with text you have half-typed, so the ticker only prompts a coordinator whose state has not changed and been idle for 60 seconds and whose input box, read from its screen, has looked empty for 10 seconds, and picks the one that changed most recently when several qualify. A box it cannot read (a menu, a scrolled view, a harness other than Claude, Codex, Cursor, Gemini, OpenCode and Pi) counts as not empty, and the nudge waits. `thread prompt` refuses to type into a thread's box that holds a draft. `nudge = false` turns this off; the items stay in the inbox, which is the record a coordinator reads with `context`.

## Routines

A file `routines/<name>.md` with TOML front matter; the body is the prompt.

- `schedule = "every <N>m|h|d"` or `"daily HH:MM"` (local time): the coordinator gets the body as an inbox item when it is due; while that item is unhandled, later runs add none. A due prompt with nothing that needs a person changed since the item it last fired on is a quiet run: it is counted by `routine list` and nothing is delivered, because a thread that is merely working and writing reports, a report already read, and an item that appeared and was handled again all leave it where it was. With no coordinator running (any agent in the project folder counts), a due run does nothing, runs no command and is not made up later; `routine list` and `doctor` show it as `skipped: no coordinator`. `routine list` shows each routine's last and next run. An optional `command` runs (`sh -c`, in the project folder, 60 second timeout) only when `routine_commands = true` and you have run `hp routine approve <project> <name>` in a terminal; its output reaches the coordinator capped at 4,000 characters inside a fence labelled as untrusted.
- `on = "pr"`, optionally `events = ["opened", "checks-failed", "review", "merged"]`: fired by the ticker's pull request poll. The body goes to the thread whose pull request changed, as a prompt, with facts the binary generates (how many checks fail, how many comments, the `gh` commands to read them). It needs no coordinator, only the open thread.
- Every project has `routines/pr-followup.md` (`checks-failed`, `review`): it tells the thread to fix failing checks and address review comments. Turn it off with `hp routine toggle <project> <name> --off`; `doctor --fix` puts it back if the file is missing.
- Every project has `routines/coordinator-sweep.md` (`every 20m`, no command): it prompts the coordinator to list the ready-for-review threads and check their reports were handled, close seen-but-not-done inbox items, read the last line of every idle thread's pane, and run `thread audit` as a by-hand cross-check. The ticker audits the registry itself every 30 minutes (see Cleanup), so this step is the manual look, not the safety net. Turn it off the same way; `doctor --fix` puts it back if the file is missing.

## Cleanup

- **Resolve on merge**: the ticker resolves a thread whose pull request merged only when its agent is neither working nor waiting on you, its own progress says `Done` (an agent that reports no progress: it has written a report since the merge or 10 minutes have passed), and no other pull request its report names is still open. A thread that merged its own pull request can still tag, deploy, land more pull requests and write its final report.
- **Resolve** (chat, or `thread resolve`) copies the report and library home, then removes the worktree with `git worktree remove` and `git worktree prune`, deletes the local branch if the pull request is merged (it asks GitHub once more first, and finds a pull request by the thread's branch when the report has no `PR:` line), and closes a tab thread's tab. Once no open thread uses the repository's primary workspace that Herdr grouped the worktree under, and it has no agent and only idle, unfocused shells, it is closed too, with one inbox item; the ticker retries one it had to keep. An adopted pane is left alone. The inbox item lists what was removed and what was kept, and why.
- **A resolve archives the thread's own inbox items.** `thread-state`, `pr` and `thread-error` items whose subject is the thread and that were written before the resolve started move to `inbox/done/`; its `resolved` event and anything an ongoing event writes are untouched. The output counts them (`archived 3 pending items about t-0139`). The ticker does the same once an item about a resolved thread is an hour old, so items left over from before this existed do not sit in the inbox forever being re-announced.
- **A resolve checks that the agent really went away.** After closing the tab or removing the worktree it lists agents again (twice, so a cached entry is not an alarm): an agent of the thread that is still listed with its pane still there means the close did not take effect. The thread is still resolved, but the record keeps what was seen (`stray_agent`, shown by `thread list` and `thread show`), an inbox item says so, and the command exits with an error naming the pane. Nothing is killed: whether a surviving process is worth stopping is your call.
- **`thread audit`** cross-checks the registry against the session: `resolved-but-running` is the shape of the thread that ran 22.5 hours after it was resolved; `registered-pane-missing` is a record nothing can restart (an adopted pane above all); `working-but-stale` is an agent that has claimed to work for 90 minutes with nothing written (three aged-reminder windows — one long step easily passes one). The command changes nothing: no pane closed, no item written, findings and a clean project both exit 0.
- **The ticker runs the same audit by itself every 30 minutes** (`AUDIT_EVERY_SECS`, a third of the 90-minute staleness window, so a thread is reported within 30 minutes of crossing it). It used to happen only on the sweep routine's step 4, which meant it never happened in a quiet project: a prompt routine is suppressed when nothing needs a person, and a thread that already says `resolved` never needs one — exactly the thread the audit exists for. Each finding becomes one `thread-audit` inbox item, written in the thread's lane, with an aged reminder every 30 minutes while it sits unhandled; a finding that stands is not written twice (the coordinator archiving the item does not make it news again), and one that goes away is forgotten so its return is reported again. A clean project gets no item, no counter and no prompt. A thread whose session cannot be listed is not audited, and its finding is kept rather than forgotten. Paused and archived projects are skipped entirely (the ticker does not tick them), and a project without a live coordinator still gets the item: it is the record you see in `list` and the next `context`. The first tick of a project only records the clock, like a routine's first run, so upgrading does not write a burst for ten projects at once.
- **`thread-audit` is its own item kind** and deliberately not one of `thread-state`, `pr`, `thread-error`: those are archived when their thread resolves, which would delete the finding about a thread that outlived its resolution. It does count as work for a person (`thread-state` does not), so a sweep routine sees a new one as news.
- **Merged pull requests and auto-resolve** clean up the same way.
- **Sweep** lists and removes worktrees on `hp/<project>/` branches with no open thread, branches of resolved threads whose pull request merged, tabs of resolved threads, working folders of tab threads resolved longer than `auto_resolve_days` ago, empty repository workspaces its threads' worktrees were grouped under, and handled inbox items older than 30 days. `doctor` shows the same list. Sweep covers local repositories.
- **Archive** closes the project's workspace and its threads' workspaces and hides the project from the projects picker and overviews (`list` still shows it, marked `archived`); nothing is deleted, and `unarchive` reopens it. Archive never closes a repository's primary workspace.
- **Delete** moves the folder to `.trash/`.

## Laptop-closed operation

Install herdr-core and hp on an always-on machine, keep the projects root there and open the project there: its ticker runs on that machine. From a laptop, `ssh` to it and attach with `hc attach`, or install the app there. Nothing is copied between the two; the project lives where the ticker runs.

## Development

```bash
cargo test                       # unit tests and scenarios against a scripted fake runner
scripts/dev-server               # a throwaway `hp-dev` Herdr session with a scratch root
scripts/dev-hp <subcommand>      # the binary against <repo>/.dev-root; pass --session hp-dev to open/doctor
scripts/dev-herdr <args>         # herdr against that session
HOME=<scratch> XDG_CONFIG_HOME=<scratch> ...  # a whole scratch config dir
```

Never develop against your default session, `~/.herdr-projects` or your real `config.toml`: `doctor --fix` writes the binary's own link, priming files and the skill link for the harnesses it finds, and it does so for the binary and root it runs as. [`history/herdr-notes-herdr1.md`](history/herdr-notes-herdr1.md) records what was verified about Herdr 1, and [`history/manual-test-herdr1.md`](history/manual-test-herdr1.md) lists its acceptance checks, including the visual ones only a person can confirm.
