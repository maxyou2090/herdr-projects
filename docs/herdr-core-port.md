# The herdr-core port: how this binary got here

`hp` is a client of [herdr-core](https://github.com/herdrdev/herdr-core). It used
to be a plugin of Herdr 1 (the TUI), and Herdr 1's plugin API is gone: this
document is the record of the port — what was kept, what was rebuilt, what was
deleted, what broke along the way, and what is still open. It is written for
whoever picks the work up next, including a future session of an agent with no
memory of it.

Read [`operations.md`](operations.md) for how the binary behaves today and
[`getting-started.md`](getting-started.md) for the first project. This page is
about the port itself.

## 1. What changed underneath, in one table

| Herdr 1 gave a plugin | herdr-core gives `hp` | `hp` therefore |
| --- | --- | --- |
| A plugin manifest (`herdr-plugin.toml`), `herdr plugin install/link/action`, `[[startup]]`, `[[panes]]` | nothing | is an ordinary installed program that calls `hc`; the ticker is a LaunchAgent |
| Workspaces, tabs, panes as first-class objects | terminals with a name, labels, a folder and a state | builds a Space, a tab and a pane out of terminals and labels |
| An agent registry (`agent list`, `agent start`, `agent prompt --wait`, `agent read`, `agent status`, blocked detection) | a program and its own OSC 7501 status | types `hp-agent <kind> …` into a terminal, reads status from 7501, reads screens with `hc read` |
| Pane metadata tokens (`pane report-metadata`), an agent view (`agent.view.set`) with filters and sorting | `hc label k=v` | tokens are labels (`hp_project`, `hp_display`, `hp_lane`); the agent view and its sorting are gone |
| A sidebar, a popup, an action menu, key bindings, a tab-bar entry | nothing: core draws nothing | every surface is a command; `overview`, `context`, `list`, `thread list` print what a window used to draw |
| `worktree create/open/remove`, Spaces grouped under a repository | nothing | plain `git worktree add/remove` under `~/.herdr/worktrees/<repo>/<branch>`, the path Herdr 1 used |
| Sessions with socket files (`session list --json`, `HERDR_SOCKET_PATH`) | one core per `HC_SESSION`, no registry | a session is a name; a socket path recorded by Herdr 1 reads as the default session |
| `notification show` | nothing | desktop notifications were removed; an inbox item is the record |
| Harness hooks (Claude Code, Codex, Droid, Gemini, Copilot) injecting progress | nothing: a program's status is only what it prints as OSC 7501 | `hp report --activity … [--done]`, run by the agent, plus `hp-agent`'s own reading of the pane for the harnesses that print no status; no hook code is left |

The binary keeps the *engine* (projects, lanes, threads, tasks, inbox, briefs,
routines, pull requests, safety settings, the ticker) and replaces the *host*.

## 2. The shape of the program now

```
you (hd* shell functions)
        │
        ▼
hp (this binary) ──hc──▶ herdr-core ──▶ terminals running pi, hp-agent, shells
        │                    ▲
        │                    └── herdr-app (the desktop window, its own client)
        └── files: ~/.herdr-projects/<slug>/…, ~/.config/herdr-projects/config.toml
```

Two binaries and one process:

| Piece | What it is | Talks to core by |
| --- | --- | --- |
| `hp` | the command line; everything the old plugin's commands, actions, popup and sidebar did | `hc` subcommands only |
| `hp-agent` | a wrapper core runs around an agent: reports `idle` on start, `done`/`error` on exit, and — for a harness that prints no status of its own — `working`/`blocked`/`idle` read from the agent's own screen | writing `ESC ] 7501 ; … ST` to its own stdout |
| the ticker | `hp ticker run`, one loop per projects root, kept alive by launchd | the same `hc` calls |

There are exactly **three contact points** with core, and all of them are public
interfaces:

1. `hc` subcommands (`run`, `ls`, `get`, `label`, `send`, `keys`, `read`, `wait`,
   `kill`, `rm`, `server status`, `--version`);
2. labels (`hp_ws`, `hp_tab`, `hp_agent`, `hp_kind`, `hp_session`, `hp_worktree`,
   `hp_project`, `hp_display`, `hp_lane`, and `herdr.space` for the app);
3. OSC 7501 status written by `hp-agent` (or by any program that reports).

Nothing writes into core's own configuration.

## 3. The mapping the adapter lives by

`src/core.rs` is the only module that speaks to core. It presents the shapes the
rest of the binary uses (a Space, a tab, a pane, an agent) and builds them out of
terminals and labels:

| Concept here | What it is on core |
| --- | --- |
| Space | every terminal carrying `hp_ws=<id>` |
| tab | every terminal carrying `hp_tab=<id>` (one terminal per tab) |
| pane | one terminal; its **name** is the pane id (`hp-<slug>-<suffix>`) |
| agent | a terminal labelled `hp_agent=<name>` and `hp_kind=<kind>` |
| agent status | the terminal's 7501 `status.state` (`idle`, `working`, `done`, `blocked`, `error`) |
| agent session id | the `hp_session` label — **read, never written by core** |
| a worktree thread | a terminal labelled `hp_worktree=<checkout path>` |
| "workspace close" | `hc kill` + `hc rm` on every terminal of the Space |

Rules that follow from this, worth keeping:

- **hp names a coordinator's session itself.** Core has no notion of a native
  agent session, and nothing in `hc` or the app writes that `hp_session` label
  (a Herdr 1 integration used to). So waiting for core to report one left every
  coordinator record without a session, and closing a Space then running `open`
  again opened a brand-new conversation (t-0082, 2026-11-21). `open` now makes
  the id for a coordinator that has nothing to resume
  (`agents::session_id`, `hp-<12 hex>`), writes it into `coordinator.json` and
  the lane **before** the agent starts, and launches with it
  (`agents::fresh_session_args`): pi's `--session-id <id>` opens the session
  with that id and creates it when it is missing, so the id alone is what a
  later `open` resumes by. Harnesses whose CLI has no such flag (codex, agy, and
  every kind not verified by hand) are unchanged: no id is recorded, and a
  restart starts anew. The label is still read (`Agent::agent_session`), so an
  upstream that starts reporting it needs no change here.

- **A pane id is a terminal name, never an address.** Core addresses (`t7`) are
  reused after a terminal ends and after a core restart; a name we choose does
  not. Records (`coordinator.json`, `threads/*.toml`) store the name.
- **Ids get a random suffix, never a count.** `hpw-<slug>-<6 hex>`,
  `hpt-…`, `hp-<slug>-<6 hex>`. Counting was the original implementation and it
  broke things: after a terminal closed, its count was handed to the next one,
  so a thread's tab id could equal the coordinator's — and closing the thread
  closed the coordinator with it.
- **A thread's three names live in one place** (`src/labels.rs`): the tab is
  the thread's **id** (`thread_tab_label`), the Space is its **title**
  (`thread_space_label`), the pane's `hp_display` is `<id> <title>` with one
  space (`thread_display`). The app draws a row as `<tab> <Space>`, so a thread
  reads `t-0047 线程命名 tab 用 id`: the id once, the title once, no `·`. The
  Space label stays the title on purpose — a restart recognises a thread's own
  Space by it (`thread::recorded_own_space`, `threads::place_tab`,
  `spaces::empty`), so changing the tab name does not disturb placement. The
  coordinator's names (`coordinator` tab, the project's Space label) are not
  these and did not change.
- **A recorded session is a name, not a path.** `Session { socket }` holds the
  `HC_SESSION` value; a Herdr 1 socket path (`/…/herdr.sock`) reads as the
  default session (`core::session_key`). This compatibility shim is what made
  the restored backup usable without rewriting a thousand records. `--socket`
  and `HERDR_SESSION` are gone: core never sets either, so the only sources are
  `--session` and `HC_SESSION` (`paths::resolve_session`).
- **"Which pane am I?" is `HC_TERMINAL_ID`, and nothing else.** Core hands a
  process the *address* of its terminal; records store the *name*. The two are
  tied by the live list (`core::pane_of_terminal`, `core::Agent::in_terminal`),
  and `paths::Env::this_terminal` / `this_session` are the only readers. Herdr 1
  exported `HERDR_PANE_ID`, `HERDR_TAB_ID`, `HERDR_WORKSPACE_ID`,
  `HERDR_SOCKET_PATH`, `HERDR_SESSION` and `HERDR_PLUGIN_STATE_DIR`; core sets
  none of them, so every check that read them was permanently false — `open`'s
  "start in this terminal", the lane a `context` reads, `lane adopt`, and
  `thread resolve`'s coordinator test all silently stopped working. Nothing in
  `src/` reads a `HERDR_*` variable for identity any more, `overview` included:
  the project of the terminal a command runs in (t-0040) is the `hp_project`
  label this binary writes on the pane (`labels::report_pane`), else the Space
  that pane is in — a Space is the terminals carrying one `hp_ws` label, since
  core has no Space of its own — read from `panes`/`agent list` and the project
  records, never from a variable.
- **"Is the session alive" is a call, not a file test.** Every `Path::new(&record.socket).exists()`
  left in the code was a bug (see §6).

## 4. What was deleted, and why

| Deleted | Why |
| --- | --- |
| `herdr-plugin.toml`, the plugin build/install path | no plugin system; `scripts/install.sh` builds and links instead |
| `src/popup.rs` (2128 lines) | a popup only a host can show |
| `src/actions.rs` | the action menu was a host surface |
| `src/setup.rs` (966 lines) | `configure`/`unconfigure` existed to edit Herdr 1's config, hooks and keys |
| the sidebar's config edits, key handling and view builders | the sidebar is gone; the file is `src/labels.rs` now, ~130 lines of wording and labels |
| `src/grouping.rs` (223 lines) | it ordered the sidebar's agent and Space lists by writing `hp_group` tokens; nothing reads them now |
| `src/scenarios.rs` (6109 lines, 213 cases) | the whole suite faked Herdr 1's CLI (`pane list` JSON, `report-metadata`, `workspace create`): it compiled, but could not have run |
| the test modules that used that fake (`adopt`, `assign`, `brief`, `lifecycle`, `overview`, `settings`, `spaces`, parts of `lanes`/`steps`/`ticker`, `tests/cli.rs`'s `progress_hooks`) | same reason |
| `agent.view` (`focus`, `unfocus`), `workspace.move` | window features core does not have |
| the percentage in `hp report` | nothing drew it any more; `--done` carries the only meaning it had (`done` vs still working) |
| the per-harness hook machinery (`progress::eligible/respond/reminder`, `instructions`, the `managed` pane test, `Record::reminded_at`/`session_started_at`/`session_id`) | no harness hooks are installed any more, so nothing called them; `hp report` is the whole path |
| `adopt-workspace` (`cli.rs`, `adopt::adopt_workspace`, `AdoptWorkspace`) | it carried Herdr 1 action arguments (`--pane`, `--workspace-cwd`, `--session`) and had no caller; `hp thread adopt` is the command |
| `src/notify.rs` (`Notifier`, `Sound`) and `Herdr::notification_show` | the user does not want desktop notifications; inbox items are the record |
| `mute` in `PROJECT.md` | it only silenced notifications; a leftover line in an older project reads as ignored |

`docs/history/manual-test-herdr1.md` holds the Herdr 1 acceptance script
itself, together with `docs/history/herdr-notes-herdr1.md` (both indexed in
`docs/history/README.md`). They are a record of what was verified on that host,
not a checklist for this one.

## 5. What the port verified, and how

Each of these was run for real on a machine with core 0.1.21 and the app 0.1.16:

| Behaviour | Evidence |
| --- | --- |
| `hp open` creates a coordinator | a terminal appears in core running `hp-agent pi --session <jsonl>`, `coordinator.json` gets workspace/tab/pane ids |
| `hp open` focuses a running coordinator | second call prints `coordinator is running in pane …` |
| `hp open --lane` does not start a second one | after §6's fix, a bare `--lane` focuses the default lane's coordinator |
| `hp thread start` | a record, a named terminal, labels, and the ticker launching the agent |
| the ticker launches pending threads | `launch_attempts` goes 0 → 1 and the agent starts (this needed the session-check fix) |
| a thread runs to review and resolves | the agent finished its turn, the thread showed `Ready for review`, `thread resolve` closed its tab |
| `hp ticker start/stop/status` | a LaunchAgent is written, bootstrapped and booted out; `status` reports the running version |
| rename, sweep | `rename --dry-run`, a real rename, a rename deferred to the ticker, `sweep --dry-run` and a real sweep |
| the app shows it | `herdr-app ls` shows a Space named after the project with `coordinator` and thread-id tabs |
| `cargo test` | 275 unit tests, 18 more in `hp-agent`, 7 CLI tests, plus the adapter tests against the fake core |
| `hp-agent` reports the middle of a turn | a script drawing claude-shaped screens under the real `hp-agent`, read back with `hc get`: `idle` → `working` → `blocked` → `working` → `idle` → the terminal `finished` |
| a harness that reports itself is not double-reported | every pi pane in `hc ls` shows pi's own `working`; `hp-agent` stays silent for `pi` (`watch_kind`) |

## 6. The traps, in the order they bit

Written down because each one cost real time and each will look like a good idea
again:

1. **`hc run --json` answers with the short row, not the terminal.** It has no
   labels; only `hc get` returns the whole record. The adapter creates, then
   `hc get`s the new terminal by name. Same for `hc ls`: it prints one short row
   per terminal, so listing labels costs one `hc get` per terminal (acceptable:
   a session holds a handful of terminals).
2. **`Path::new(record.socket).exists()` silently disabled the ticker.** With
   sessions now being names, every project looked unreachable, so the tick pass
   returned "nothing to do" and no pending thread was ever launched. Any leftover
   file test on a session is this bug again.
3. **Reported folders are physical.** core reports `/private/tmp/x` for a
   terminal started in `/tmp/x`, so identity checks must compare the recorded
   folder, not the canonicalized one (this is the same quirk Herdr 1 had).
4. **Count-based ids collided** (see §3). If a future feature needs ids, use the
   random-suffix helper.
5. **`hc send` refuses a `blocked` program.** That is a feature: it maps to
   `agent_blocked`, and it is safer than Herdr 1, which merged a prompt into
   half-typed text. `refused_before_typing` still matches on it.
6. **`hc read` returns plain text, with no SGR.** `prompt_box` first told a
   placeholder from typed text by dim/reverse styling, which on core it could
   never see. It now reads the box by its glyphs, its borders and its known
   placeholders only (t-0061), so a captured screen and a real read are judged
   the same. The cost is one shape it cannot split: Claude Code's rotating
   `Try "…"` suggestion is recognised by its shape, so a draft that is exactly
   one quoted `Try "…"` would read as a placeholder. See §8.
7. **The app needs time, and will not empty a Space.** `herdr-app ls` does not
   show a fresh space or tab for a moment; `tab.close` refuses the last tab of a
   space. So: create the Space, create our own terminal, close the placeholder
   tab afterwards, and retry `ls`/`rename`/`close` (30 × 60 ms).
8. **`hp` must not open a project by itself when it is an agent.** `open` from a
   shell currently starts the coordinator in that shell; keep the `person` flag
   in mind when adding paths.
9. **Replacing the binary in place is unsafe on macOS** — `mv`, not `cp`, over a
   running binary (this predates the port and still applies to the ticker).

## 7. Data: what the port did to a restored backup

A Herdr 1 backup restored into `~/.herdr-projects` carries state that only made
sense to Herdr 1. The one-off migration (destructive, done once, by hand) was:

- each project's `.state/lanes.json`: keep the default lane, clear its `pane_id`;
- delete `.state/coordinator.json` and `.state/coordinators.json` (rebuilt by the
  next `open`);
- each `threads/t-*.toml`: clear `workspace_id`, `repo_workspace`,
  `own_workspace`, `tab_id`, `pane_id`; set `status = "resolved"` where it was
  open, keeping every report, `memory/`, `TASKS.md`, `MEMORY.md` and `PROJECT.md`;
- move `.state/ticker.json` and `.state/lock` (and the root `.ticker.lock`) into
  `.state.pre-core/`, so stale pane ids and nudge hashes never vote again.

`scripts/migrate-from-herdr1-state.py` does exactly this and is safe to re-read
before running it; `.state.pre-core/` can be deleted whenever it stops being
interesting.

## 8. What is still open

Ordered by what a user would notice first:

1. **`hp-agent` reports what a pane's screen can prove, not the whole turn.**
   Fixed for the harnesses that report, and partial for the rest:
   - **A harness that reports itself needs nothing.** pi prints the Program
     Status Protocol's own records — `working` during a run, `blocked` for its
     own dialogs and logins, `done`/`error`/`idle` around them — and core shows
     them as they arrive: every pi pane on this machine reads `working`
     mid-turn today. A wrapper that spoke for the same terminal would fight it,
     so pi is listed in `SELF_REPORTING` in `src/hp_agent.rs` and left alone.
   - **A harness that prints nothing is watched from outside.** For the kinds
     this binary can read a screen for (`claude`, `codex`, `cursor`, `gemini`,
     `opencode`) `hp-agent` polls its own pane with `hc read` every two seconds
     and reports what `src/agent_state.rs` can prove: `blocked` on a trust or
     review screen (its phrase, on a screen whose input box is not empty, on
     two reads in a row), `working` while the screen is changing, `idle` once an
     empty input box has sat still for three reads (six seconds). The wrapper's
     own words are unchanged: `idle` at start, `done`/`error` at exit.
     `HP_AGENT_WATCH=0` turns the polling off.
   - **What is still missing.** A kind whose layout `prompt_box` cannot read
     (`kimi`, `qwen`, the one-off agent CLIs) reports nothing but
     `idle`/`done`/`error`: there is no honest `blocked` for a screen this
     binary cannot parse. A turn that prints nothing *and* redraws no
     elapsed-time line for six seconds reads as `idle`; a false `idle` can
     delay a brief by a tick but never types into a busy pane, because
     `brief::deliver` re-reads the box before sending. `prompt_box` reads the
     box from plain text now (t-0061): placeholders, borders and prompt glyphs,
     with every unknown layout left `Unknown` — which the ticker, the brief and
     this watcher all treat as "not empty". And the ticker still reads screens
     itself for the trust-screen check before a brief: redundant for watched
     kinds, the only cover for the rest.
   - **Why not the PTY relay this port first planned.** A relay would have to
     own the terminal: `openpty`, raw mode, window-size and `SIGWINCH`
     forwarding, and a VT screen model to keep `prompt_box`'s geometry — all of
     it to reconstruct information core already keeps and this process can read
     with one `hc` call. Its failure mode is the agent's interactive experience,
     the one thing this wrapper must never break, and it would cost a
     terminal-emulator dependency. Reading the pane gets the same states with no
     terminal access at all; `src/agent_state.rs` holds that state machine and
     §5's last rows are the evidence.
2. **[closed] `docs/lanes.md` carried its Herdr 1 wording** (sidebar, popup,
   action menu). Closed by t-0049: those sentences now name a core term (Space,
   tab, terminal, label) or a command (`hp overview`, `hp context`, `hp lane
   list`), §9 is "Labels, grouping, lifecycle", and the port note at the top says
   the same. The lane model is untouched — only the words for what draws it.
3. **Tab order in the app is creation order.** Putting "needs you" first means
   `herdr-app do tab.move`, which is not implemented.
4. **[closed] Remote machines are deleted, not ported.** `hc --on <machine>` was the
   only mapping `Herdr::on_machine` had, and `hp assignable` printed
   `Machines: none` on a real machine, so the client dropped the feature:
   `remote.rs`, `--machine`, `Thread::machine`, the SSH/scp/rsync copies, the
   per-machine outage memory and `profile@machine` owners are gone. A record or
   a `TASKS.md` line written when it existed still loads; an `@machine` owner is
   refused with a clear error. Nothing on a remote is untested: there is no
   remote path left.
5. **[closed] `hp update` has no release feed, and is not meant to get one.**
   It says which build this is and how to move it (`git pull`, `cargo build
   --release --locked`, `ticker stop && ticker start`), and `--check` says the
   same without changing anything. Decided on 2026-11-17: nothing is published,
   install is from a checkout ([`going-public.md`](going-public.md), `## Done`,
   "No prebuilt binaries"), so there is no feed to invent. It reopens only if
   publishing is decided.
6. **Test coverage is thinner than the old suite.** 275 unit tests (5 of them
   the fake core's own), 18 more compiled into `hp-agent` (which shares
   `prompt_box`, `trust_screen` and `agent_state` with it), 7 CLI tests and 5
   adapter tests against `src/fake_hc.rs` (a fake core that answers the `hc`
   commands this binary runs). The fake models terminals, labels, status,
   `send`, `read`, `kill`; it does not model worktrees, the app, or waiting.
   Growing it is the cheapest way to buy coverage back.
7. **[closed] The branch has landed.** `port/herdr-core` is the fork's default
   branch (`origin/port/herdr-core`), and PRs #26–#30 are merged into it: the
   remote-machine deletion and dead code (#27), the three always-false checks
   (#28), the ticker that cannot die silently (#30) and the routine/sidebar/
   `hp-agent` rename (#29). It is still not published anywhere as a product: no
   release feed, no crate, no install other than `scripts/install.sh`.

## 9. Where the code is

| File | Role |
| --- | --- |
| `src/core.rs` | the whole adapter: shapes, ids, labels, status, worktrees, app-vs-core creation |
| `src/app.rs` | the desktop window: `status`, `space.new`, `tab.new`, `ls`, `tab.rename`, `tab.close`, `space.close`, with retries |
| `src/hp_agent.rs` | the OSC 7501 wrapper: `idle` at start, `done`/`error` at exit, and a two-second screen read for the harnesses that print no status |
| `src/agent_state.rs` | the states a pane's screen proves (`working`, `blocked`, back to `idle`); compiled into `hp-agent` alone |
| `src/fake_hc.rs` | a fake core for tests (a `Runner`, so the real adapter is exercised) |
| `src/ticker.rs` | the loop, the LaunchAgent, the launch/brief/nudge passes |
| `src/coordinator.rs` | `open`: lanes, reuse, priming, `context` |
| `src/threads.rs`, `src/thread.rs` | starting, steering, resolving threads |
| `src/lanes.rs` | lanes: registry, resolution rules 1–5, adoption |
| `src/progress.rs` | `hp report`, the per-pane record, `hp progress` |
| `src/labels.rs` (was `src/sidebar.rs`) | the words (`needs you`, `review`, `12m quiet`) and the labels that carry them |

## 10. Reproducing a development loop

```bash
cd ~/repos/herdr-projects
cargo test --locked                 # 275 + 18 + 7
cargo build --release --locked
hp ticker stop && hp ticker start   # the ticker is a daemon: a rebuild does not reach it
hp doctor
```

`hp open` reads the binary fresh every time, so a rebuilt `hp` is live
immediately for commands; only the ticker needs the restart. For a scratch
machine, `HERDR_APP_BIN`, `HC_BIN_PATH`, `HC_SESSION` and `HERDR_PROJECTS_ROOT`
redirect the app, the core, the session and the projects root.
