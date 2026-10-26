# Lanes: several coordinators in one project

> This is a design document for a feature that is partly built: commit 1 of §12
> exists (the registry, the five identity rules, the `Coordinators:` line and
> `lane list`), and §3.5 says exactly what is and is not in the binary today.
> Code is cited by symbol (`module::symbol()`, `src/file.rs`), never by line
> number, because every commit of this series moves them.

A **lane** is one line of work inside one project. Each lane has its own
coordinator instance, its own slice of the task list and its own inbox. The
repositories, worktrees, branches, pull-request flow, safety settings and
engineering conventions stay shared.

This document describes **plan B**: ownership, identity, visibility and the task
split. Memory partitioning (**plan C**) is listed as a later option at the end
and deliberately not designed here.

## 1. The problem

Today the only way to run two lines of work at once is two projects. The user
keeps `law` for feature work and `law-live` for production bug fixes, because
one project can only have one coordinator: while `law`'s coordinator is busy
starting threads and reading reports, there is nobody to hand a production
incident to.

Two projects cost real money:

- **Memory is copied by hand.** `~/.herdr-projects/law-live/memory/inherited-from-law.md`
  exists only to restate, in `law-live`, the engineering boundaries that live in
  `law/memory/`. Every lesson learned in one is invisible to the other until
  somebody copies it.
- **The repository lists drift.** `law/PROJECT.md` lists 15 repositories,
  `law-live/PROJECT.md` lists 7. Nine repositories that `law` uses are missing
  from `law-live`; `law-live` has one (`/Users/maxyou/web/www/law_ai`) that `law`
  does not. Every new repository has to be added twice or one line quietly works
  against a stale list.
- **Threads scatter.** `law` already holds 168 thread records; `law-live` holds
  16. `t-0012` in one project is a different thread than `t-0012` in the other,
  the sidebar shows two groups for one body of work, and "what is open in this
  area" is a question nobody can answer in one place.

The abstract version of this is "the tool does not support multiple
coordinators". The concrete version, which is what actually breaks, is this:

**A project can only remember one coordinator, so an event can be delivered to
the wrong one.** Concretely:

1. `Project::coordinator()` (`src/project.rs`) reads
   `.state/coordinator.json` — one file, one record. `update_coordinator()`
   (same file) is a read-modify-write of that same file.
2. `open` writes that record with `*c = Coordinator { ... }`
   (`coordinator::open`, `src/coordinator.rs`) — a whole-record replacement, not
   an append. A second `open` (including `open --new`) overwrites the first
   coordinator's record. The first coordinator keeps running but is no longer
   the one written down.
3. `.state/coordinators.json` (`LivePane` and `discover()` in
   `src/coordinator.rs`) looks like a registry but is only a cache: the ticker
   rebuilds every entry every tick, and the file is worthless after a Herdr
   server restart or while the ticker is stopped.
4. `is_coordinator()` (`src/coordinator.rs`) decides who is a coordinator with
   `agent.works_in(&record.cwd) || session_matches(record, agent)`. `works_in`
   (`src/herdr.rs`) compares `own_dir(cwd, foreground_cwd)` (same file) to the
   recorded directory.
5. Therefore, when two agents work in the same project home, `works_in` is true
   for **both**, and every "find the coordinator" site picks
   `max_by_key(state_change_seq)` — "whichever was most recently active". See
   `found()` (`src/coordinator.rs`) and the `running` branch of `open`
   (same file). The record gets replaced by whichever coordinator spoke last,
   and the next inbox event goes to whoever that is.
6. The directory test is already failing in production. In the `law` project
   today, the record says `cwd = /Users/maxyou/.herdr-projects/law`, but the
   coordinator pane `w1AR:p1` has `own_dir = /Users/maxyou`. `works_in` returns
   **false**; the pane is still recognised only because `session_matches`
   (`src/coordinator.rs`, pane id **and** native session id) catches it. The
   primary test is dead and the fallback is carrying the system.

Point 6 is the reason this document does not simply "key the record by pane id"
and move on: the current identity scheme is already unreliable with one
coordinator, and multiplying it by two would turn a silent failure into an
everyday one.

Point 6 is also not theoretical. When *both* tests miss, nothing says so. The
`herdr-projects` project lived through exactly that from 2026-10-18: its
coordinator's rule-1 and rule-2 signals were both gone, the project was treated
as having no coordinator, `nudged` in `.state/ticker.json` froze on the hash of
the 2026-10-18 item, and neither the ticker log nor the inbox said a word. A
finished thread never reported itself and the stall was found a week later, by
the user. §5.4 turns this class of failure into a printed line on every turn,
which is the cheapest thing in this design and the reason the three-valued
answer is acceptable at all.

## 2. Concepts

Three things, deliberately kept apart:

- **Project** — the folder `~/.herdr-projects/<slug>/`, its `PROJECT.md`
  (`name`, `goal`, `repos`, profiles, safety), `MEMORY.md`, `TASKS.md`,
  `inbox/`, `threads/`, `routines/`. One project, one root, one lock.
- **Lane** — a named line of work inside the project: `c-1` fix, `c-2`
  features. A lane owns a coordinator record, a slice of `TASKS.md`, and the
  inbox items addressed to it.
- **Coordinator instance** — one running agent pane that claims a lane. A lane
  can have zero (its coordinator is gone) or one (normal) live instance; a
  pane claims at most one lane.

```text
project: law  (~/.herdr-projects/law)
├── lane: c-1  "抢修"      goal: production incidents
│     coordinator: pane w1AR:p3, agent hp-c-law-1, profile pi-deepseek-flash-high
│     TASKS.md:  ## 抢修/Backlog   ## 抢修/Doing
│     inbox:     items with lane = c-1
│     threads:   t-0171, t-0172 …
├── lane: c-2  "收藏"      goal: bookmark features
│     coordinator: pane w1AR:p5, agent hp-c-law-2, profile pi-deepseek-flash-high
│     TASKS.md:  ## 收藏/Backlog   ## 收藏/Doing
│     inbox:     items with lane = c-2
│     threads:   t-0140 … t-0170
└── shared: PROJECT.md (repos, safety, profiles), MEMORY.md, memory/,
            worktrees, branches, PR flow, routines, conventions
```

Several lanes live in **one Space**: the project home. That is the user's
existing habit — one project space, one coordinator tab per line — and it does
not change the layout of `law`, `law-live`, `ovis` or `rexa`.

A thread belongs to a lane, not to a pane. That distinction is the whole point:
a coordinator restarts, switches profile, loses its session — the thread still
knows which lane it serves.

## 3. Decisions (settled — not for re-litigation)

These are the user's decisions, taken after reading the first draft of this
document. They are not up for re-litigation inside the implementation commits;
if a commit cannot meet one, stop and ask.

| # | Decision | 决策（已拍板） |
|---|---|---|
| 1 | **Layout follows the habit.** One project Space, one coordinator tab per lane, every coordinator working in the project home. **No per-lane subdirectories.** | **布局沿用现有习惯**：一个项目 Space，每 lane 一个协调器 tab，全部在项目 home 工作；**不引入子目录**。已有 space 布局不变。 |
| 2 | **Threads bind to a lane, not a pane.** Ownership survives a coordinator restart, a profile change or a lost session. | **线程绑 lane，不绑 pane**：协调器重启、换 profile、session 丢失后归属不丢。 |
| 3 | **Identity is self-reported, not inferred from the directory** (§5). The core of the design. | **身份自报，不靠目录推断**（§5）：本方案的核心。 |
| 4 | **Orphans are never adopted automatically.** A lane with no coordinator gets one inbox item naming its live threads; ownership moves only on the user's word. | **孤儿不自动接手**：只给一条 inbox 提醒说明还有 N 个活线程；必须用户确认才转移归属。 |
| 5 | **Taking over is an explicit command**, `hp lane assign <lane> <pane>`. No automatic handover. | **接手是显式命令** `hp lane assign`；不做自动转移。 |
| 6 | **The default is a lane.** No lane named means the default lane; an empty `lane` field reads as the default lane, so old data needs **zero migration**. | **缺省默认**：不指定就用默认 lane；`lane` 为空的老线程视为默认 lane，**零迁移**。 |
| 7 | **Layered rollout.** This document is plan B: ownership, identity, visibility, task split. Memory partitioning (plan C) is listed in §11 and not designed. | **分层改造**：本文只描述方案 B（归属 + 身份 + 可见性 + 任务分区）；记忆分区（方案 C）单列 §11，不展开。 |
| 8 | **Three-valued resolution, and visibility is a hard requirement.** `is_coordinator` cannot carry an error, so an unresolvable pane is `Ambiguous`, never a guess — and every `context` prints which rule each live coordinator matched, every turn, with the command that fixes it (§5.4). | **三态解析，可见性是硬要求**：判不了就是 `Ambiguous`，绝不猜；但 `hp context` 每轮都必须打印每个协调器命中的规则，并给出修法（§5.4）。 |
| 9 | **Lanes are closed, not deleted.** `hp lane close` retires a lane once none of its threads is alive; `hp lane rm` is not built (§6.4). | **只关不删**：`hp lane close` 在没有活线程后置为 closed；`hp lane rm` 不做（§6.4）。 |

## 3.5 What is implemented today

Only commit 1 of §12 exists: the registry, the five identity rules, the
`Coordinators:` line and `lane list`. Everything else in this document is the
design the later commits implement; the table below says which.

Concretely, in `src/`: `lanes::Registry`, `lanes::Lane`, the five rules of
`lanes::resolve` returning the three-valued `LaneResolution` (`src/lanes.rs`),
the `Coordinators:` line (`lanes::identity_line`, same file), `hp lane list
[--json]` (the `LaneCommand::List` variant in `src/cli.rs`, the only `lane`
subcommand that exists), the `hp_lane` token write (`sidebar::report_pane`,
`src/sidebar.rs`) and its clear (`sidebar::clear_pane`, same file, and
`grouping::clear`, `src/grouping.rs`), the `Agent.tokens` field
(`src/herdr.rs`), `Thread.lane` (`src/thread.rs`) and `inbox::Item.lane`
(`src/inbox.rs`) as fields with no writer, and `Project::coordinator()`
delegating to the default lane (`src/project.rs`).

| In the binary now | Designed, not built (commit) |
|---|---|
| `context`'s `Coordinators:` line | `open --lane` (§6.1, commit 4) |
| `hp lane list [--json]` | `lane adopt` / `assign` / `rename` / `close` / `open` (§6.2, §6.4, commit 4) |
| the `hp_lane` token write | `Thread.lane` / `inbox.lane` written (§4.2-4.4, commit 2) |
| `Project::coordinator()` delegating to the default lane | per-lane `seen`, `steps::State` maps (commit 2) |
| | lane-routed nudge / prompt / discover (commit 3) |
| | TASKS.md per-lane headings, `hp task` (commit 5) |
| | sidebar `· <lane label>`, grouping, lifecycle (commit 6) |
| | skill rules, doctor report (commit 7) |

## 4. Data model

### 4.1 `.state/lanes.json` — the lane registry

The registry is the source of truth. `coordinator.json` becomes a *view* of its
default lane (§10).

```jsonc
{
  "lanes": [
    {
      "id": "c-1",                 // stable, never reused, never renumbered
      "name": "fix",               // CLI handle: `--lane fix`; unique per project
      "label": "抢修",              // display name: sidebar, tab, workspace label
      "goal": "production incidents only",
      "profile": "pi-deepseek-flash-high",   // default profile for this lane
      "status": "open",            // "open" or "closed" (§6.4); closed is reversible
      "pane_id": "w1AR:p3",        // last known instance (see §5, rule 2)
      "agent_session": "/Users/…/sessions/…jsonl",
      "created": "2026-10-25T10:00:00Z",
      "closed": "",                // when `lane close` ran
      "last_active": "2026-10-25T10:42:11Z"
    },
    {
      "id": "c-2",
      "name": "books",
      "label": "收藏",
      "goal": "",
      "profile": "",
      "status": "open",
      "pane_id": "",
      "agent_session": "",
      "created": "2026-10-25T10:05:00Z",
      "closed": "",
      "last_active": ""
    }
  ],
  "default": "c-1",
  "next": 3                       // the `c-<n>` counter; never rewinds
}
```

- `id` is the identity. It is allocated as `c-<n>` from the `next` counter kept
  in the same file (like `inbox-counter.json`), never derived from the name, and
  never reused — not after a close either, so `c-2` is `c-2` forever and a
  `lane assign c-1` quoted in an old report can never hit some other lane.
- `name` is what commands take, and is what changes when the user renames a
  lane's *handle*. `label` is what humans see. `hp lane rename` changes `label`
  only (the `id` never moves), and may optionally change `name` with an explicit
  `--name`.
- `status` is `open` or `closed` (§6.4). A `closed` lane keeps its record, its
  id and its history; it stops appearing in `context`, stops taking a sidebar
  row and stops participating in nudges. Closing is not deleting.
- `pane_id` and `agent_session` are a *hint* for rule 2 of §5, refreshed by the
  ticker. They are not the identity.
- The file is written only under `project.lock()`, with the same
  `read_json`/`write_json`/`write_atomic` helpers the rest of the state uses
  (all three in `src/project.rs`).

### 4.2 `Thread.lane`

Add `lane: String` to `Thread` (`src/thread.rs`, `struct Thread`) — the lane
**id**, not its name, so renaming a lane never touches 168 thread files. Empty
on every record written before this change; empty reads as the default lane
(§10). Serialised with `#[serde(default)]` like the rest of the struct, so old
`threads/<id>.toml` files keep loading and no rewrite pass is needed.

### 4.3 `inbox::Item.lane`

Add `lane: String` to `inbox::Item` (`src/inbox.rs`, `struct Item`). Every
writer (`inbox::write`, same file) sets it: the ticker knows the thread's lane
from its record, so all thread, PR, stuck and session items inherit it. Items
with an empty lane are the default lane's.

Seen-state becomes per lane:

- `.state/inbox-seen.json` → `.state/inbox-seen/<lane>.json`
  (`inbox::seen` and `inbox::mark_seen` in `src/inbox.rs`).
  Its `live` filter (`unhandled()`) is filtered by the same lane, so one lane's
  `context` cannot mark another lane's items read.
- `inbox::done` (`src/inbox.rs`) stays global on purpose: handling an item
  is handling it, whoever does it.

### 4.4 `steps::State` per lane

`State` (`src/steps.rs`, `struct State`) holds one `nudged` hash, one
`renudged_at` and one `box_pane` field — i.e. one deliver-once opportunity and
one "which input box did I last type into" per project. Two coordinators would
share, and burn, each other's. Make them maps keyed by lane id:

```rust
pub nudged: BTreeMap<String, String>,      // lane id -> hash
pub renudged_at: BTreeMap<String, String>, // lane id -> timestamp
pub box_pane: BTreeMap<String, String>,    // lane id -> pane id
```

`box_empty_since` follows `box_pane`. `nudge()` and `aged_renudge()`
(both in `src/steps.rs`) take a lane, compute the unseen set for
that lane, and touch only that lane's entries. The "nothing is spent without a
delivery" rule from the 2026-10-07 incident stays exactly as it is; it is now
per lane, which also fixes the case where lane A has no coordinator and lane B
never gets nudged because A burned the shared hash.

Backward compatibility: `steps::load_state` accepts the old scalar shape by
moving the scalar into the default lane's slot; a missing entry is empty.

```jsonc
// .state/ticker.json, after
"nudged": {"c-1": "9f2c…", "c-2": ""},
"renudged_at": {"c-1": "2026-10-25T10:40:00Z"},
"box_pane": {"c-1": "w1AR:p3"}
```

## 5. Identity: who is this coordinator?

The current answer is "whoever works in this directory". It is wrong today
(§1.6) and it cannot be made right: panes legitimately sit outside the project
folder (a coordinator whose shell was `cd`-ed away, an `open` running a
coordinator as a child in somebody else's shell — this is the recorded incident
in `local-notes/local-dev.md:72`), and two coordinators legitimately share the
folder.

So identity is **declared**, not inferred.

### 5.1 The self-report: an `hp_lane` pane token

Herdr lets a pane carry tokens, and the plugin already uses that channel.
`sidebar::report_pane()` (`src/sidebar.rs`) calls
`herdr pane report-metadata <pane> --source herdr-projects --token k=v` with a
TTL of `TOKEN_TTL_MS = 300_000` (same file). Live panes show it:

```json
"tokens": {"hp_group": "law!0!w1AR:p1", "hp_project": "law", "hp_rank": "5"}
```

Adding one token — `hp_lane=c-1` — is enough for a coordinator to say which lane
it is. `herdr pane report-metadata --token NAME=VALUE --ttl-ms N` is the same
mechanism (`--source` is `herdr::SOURCE`, `src/herdr.rs`); the token
also expired the same way, so a stopped ticker or a dead pane leaves no stale
claim behind after the TTL.

Semantics, spelled out, because two places now write pane metadata:

- **The registry is the truth, the token is the fast path.** `lanes.json` says
  which lane exists and which pane last ran it; the token only saves a lookup
  and survives nothing. Losing the token (TTL, `clear_pane`, server restart)
  degrades to rule 2, never to a wrong lane.
- **The ticker refreshes the token every tick** for panes it resolves to a lane
  (alongside the existing `hp_group`/`hp_rank`/`hp_sub` writes in
  `grouping::apply`, `src/grouping.rs`, which already re-sends a card every
  `TOKEN_TTL_MS / 3`, same file). So the token's TTL is a liveness lease, not a
  lease on ownership. **Until commit 3 the ticker still refreshes only the panes
  its single record matches**, so in a project with several lanes a second
  lane's token expires after its TTL (five minutes) and that pane falls back to
  rules 2-5; a single-lane project is unaffected.
- **`hp_lane` never clears `hp_group`**: `sidebar::report_pane` sends
  `--clear-token` for `OLD_TOKENS` (`src/sidebar.rs`), and `hp_lane` must not
  join that list; `sidebar::clear_pane` (same file) and
  `grouping::clear` (`src/grouping.rs`) must clear it.
- The token value is the lane **id**, so a rename cannot make a token lie.
- **A lane id is unique inside one project, not across projects.** Every
  project's first lane is `c-1`, and one Herdr session holds panes of several
  projects. So a lane claim is only read against *this* project's lane table
  when the pane's own `hp_project` token — the one every report writes — is
  absent or equal to this project's slug; a pane whose report names another
  project is not this project's coordinator at all. This is a **correctness
  requirement, not an optimisation**: without it `rexa`'s coordinator would
  resolve to `law`'s default lane, and cross-project mixing is the exact thing
  lanes exist to end. The cost is bounded and one-directional: if `hp_project`
  expires (TTL, `clear_pane`, a ticker that stopped) while `hp_lane` survives,
  the pane falls through to rules 2-5 of §5.2 — a *degradation*, never a wrong
  answer. A pane that carries no `hp_project` at all (a hand-written report,
  an adopted pane) is judged by the rules, not refused for silence.

### 5.2 Resolution order

Every "who is the coordinator of this lane" question resolves with this
priority list, in order:

```text
1. pane has an hp_lane token                     -> that lane          (self-reported, strongest)
2. lanes.json matches on pane_id + agent_session -> that lane          (survives restart, tab move)
3. owner_dir is in the project home, and the project has exactly one lane
                                                 -> default lane       (old projects)
4. owner_dir is in the project home, but the project has several lanes
                                                 -> refuse; error names `hp lane adopt`
5. anything else                                 -> not a coordinator
```

Each rule and why it sits where it does:

- **Rule 1 first** because it is the only signal that means what it says. It is
  written by the process that knows; it costs one already-happening metadata
  read; it cannot be produced accidentally by a thread (a thread's `own_dir` is
  under a worktree or `threads/<id>/`, never the project home — but a token is
  positive evidence regardless of directories). It is read only for panes this
  project owns: see the `hp_project` scoping in §5.1.
- **Rule 2 second** because a token can be missing (server restart, TTL while the
  ticker was down, `open` started a moment ago). The pair `(pane_id,
  agent_session)` is already the fallback that rescues the `law` coordinator
  today (`session_matches`, `src/coordinator.rs`); the native session id is
  what stops a **recycled** pane id from counting, and it is exactly what a
  restarted `open` re-records.
- **Rule 3** is the old behaviour, kept for projects that have one lane, so
  nothing changes for them until they add a second one.
- **Rule 4 is a refusal, and that is the point.** "Works in the project home" is
  true for every coordinator, so with two lanes it carries no information. Today
  the code resolves that ambiguity with `max_by_key(state_change_seq)`
  (`src/coordinator.rs`, in `found`, `open`, `prompt` and `digest`) and silently
  picks one; that is exactly how events reach the wrong lane. The new rule: refuse the
  classification, print which panes are ambiguous, and ask for
  `hp lane adopt <lane>` (run in that pane, §6.2) or `hp lane assign <lane>
  <pane>`. **An error the user can fix beats a guess that silently misfiles
  work.** The same refusal applies wherever the answer is used: `place_tab`
  (§9), `prompt`, `nudge`, and the ticker's `discover`. The one exception is
  `is_coordinator`, a `bool` on the hot path of every tick: it keeps a triple
  `Coordinator / Ambiguous / No`, and a pane that comes back `Ambiguous` is
  counted as a coordinator for liveness (so the project is not declared
  coordinator-less and its routines keep firing) but is never given an inbox
  item, never prompted, and never assigned a lane. §5.4 is what makes that
  safe.
- **Rule 5**: an agent in a worktree is a thread; an agent nowhere near the
  project is nobody. Both are also excluded by "not in any lane's registry and
  no token".

### 5.3 Why the working directory cannot be the identity

- It fails today (the `law` coordinator's `own_dir` is `/Users/maxyou`, §1.6).
- It is shared by definition in this design: every lane's coordinator works in
  the same home folder, by decision 1.
- It drifts for innocent reasons (`local-notes/local-dev.md:72`: a coordinator
  shell left in `~/repos/herdr-projects` after a dev run; the same note records
  that this caused a silent nudge stall across all projects on 2026-10-07). With
  one lane, drift degrades to `session_matches`; with several, drift would
  degrade to "which lane?" — a randomness we refuse to introduce.
- It cannot survive a move: worktrees, `rename`, and re-opened panes all change
  directories; a lane's threads do not change lane.

The directory keeps exactly one job (rule 3): deciding that a project with a
single lane and no token still has its old coordinator.

### 5.4 `Ambiguous` must be visible every turn

The three-valued answer is only safe if a person can see it. A wrong-but-quiet
classification is how this whole area already failed once.

**The incident this rule exists for.** The `herdr-projects` project's own
coordinator sat in a state where **both** tests were dead — its pane's
`own_dir` was `/Users/maxyou`, not the project home, and its recorded session id
no longer matched — so `is_coordinator` said no, the project was treated as
having no coordinator, and nudges silently stopped. `nudged` in
`.state/ticker.json` stayed frozen on the hash of the 2026-10-18 item and never
moved again; the ticker logged nothing about it, and no inbox item appeared. It
was found only when the user asked why a finished thread had not reported
itself, a week later. Writing one inbox item *at the moment the state changes*
would have missed this exactly: there was no transition to catch, the failure
was standing still.

So visibility is a standing output, not an event:

- **`hp context` prints one `Coordinators:` line at the top, every turn, no
  exceptions**, before the goal and the settings — because it is the fact that
  decides whether the rest of the digest can exist at all. It names, for every
  live pane in the project folder:

  ```text
  Coordinators: w1D3:p1 (token, lane c-1, this pane) · w1AB:p1 (ambiguous: no token, no record match, 2 open lanes share this folder — run `hp lane adopt <lane>` in it) · lanes c-1 c-2 of 2 (default c-1)
  ```

  Each entry is `pane (rule, lane, this pane|other pane)`; `rule` is `token`,
  `record`, `dir` or `none`, and a `Known` answer to a **closed** lane adds
  `, closed`. An `ambiguous` entry carries the reason — including **how many
  open lanes are contesting the folder**, which is the one fact that tells a
  person whether a lane was closed by mistake or opened by mistake — and the
  command that fixes it. A coordinator reading its own digest must be able to
  hand the user a one-line fix, not the word "ambiguous".

  The words on the line, and what each one means — they are separate answers,
  not spellings of one another:

  | Word | What it says | The fix it comes with |
  | --- | --- | --- |
  | `token` / `record` / `dir` | a rule resolved this pane to a lane (§5.2) | none needed |
  | `ambiguous` | one of two refusals, both printed with their reason: **several open lanes** could claim the pane and no positive signal says which (`no token, no record match, 2 open lanes share this folder`), or **a token names a lane the registry does not have** (`token hp_lane=c-9 names no lane`) — a stale or hand-written claim, which says "a coordinator" but not "which" | `hp lane adopt <lane>` in that pane |
  | `none` | the pane works **in the project folder** and **no lane claims it** — nothing resolved it *and* the folder alone gave no answer (every lane is closed, so there is not one open lane for rule 3). It is never folded into the default lane; it is listed so the pane is not invisible | the entry carries no command of its own; `hp lane adopt <lane>` in that pane is what makes it explicit |
  | `degraded` | the lane recorded this pane, the pane is **alive**, and no rule matches it any more (its recorded session id changed, its shell drifted out of the folder) | `hp lane adopt <lane>` |
  | `orphan` | the lane's recorded pane is **not listed at all**, or it never recorded one (printed by `lane list`, never on this line) | `hp open <slug> --lane <lane>` |
  | `closed` | the pane resolved to a lane that has been put away (§6.4); it is nobody's coordinator for nudging, but it is shown rather than left to be discovered later as a stray agent | none |

  **`degraded` and `orphan` are not the same failure and must not share a
  fix.** Both are "this lane has no working coordinator right now", but
  `degraded` means the process is running and merely unrecognized — suggesting
  `open` there starts a *second* coordinator beside the one already working,
  which §6.4 forbids — while `orphan` means the pane is genuinely gone, and
  `open` is exactly right. This is why the line and `lane list` decide between
  the two once, before either prints (`View::lane_statuses`, `src/lanes.rs`).

  **The digest also names the pane it is being read from**, when Herdr lists
  that pane as an agent and nothing above mentions it:

  ```text
  this pane w7:p7 is no lane's coordinator (run `hp lane adopt <lane>` in it)
  ```

  Without that, the `herdr-projects` incident's own shape — a coordinator that
  matches no rule — was the one pane a digest said nothing about, which is the
  same silence this section exists to end, only seen from the near side. A pane
  outside Herdr (`HERDR_PANE_ID` unset), or one that is only a shell and no
  agent, is said nothing about.
- **`hp lane list`** prints the same fact per lane and per live pane (the
  matched rule included), plus open lanes whose coordinator is missing entirely
  and closed lanes as `closed`. Its first line **is** the `Coordinators:` line —
  both are rendered from one `View`, so the two cannot drift apart — and its
  rows name each lane's panes as `pane(rule)` (`-` when a lane has none).
  `--json` adds `goal`, `profile`, `pane_id` and an `orphan` flag per lane, set
  from the same `degraded`/`orphan` decision the text uses. Per-lane thread and
  inbox counts belong to the same command and are **not printed yet** — they
  arrive with commit 2, when `Thread.lane` and `inbox::Item.lane` start being
  written (counting them now would print a constant 0).
- **The `nudge`/`prompt`/`place_tab` paths still refuse** on `Ambiguous` (§5.2
  rule 4), and a refused nudge is logged by the ticker as a held reason
  (`nudge_held`, `src/ticker.rs`, logged when it changes in `tick_project`) —
  which is what the ticker log showed was missing in the
  incident above: the failure there was a *no coordinator* verdict, and that
  branch logs nothing at all. The refusal itself lands with commit 3: until
  then the nudge path still asks the old single-record `is_coordinator`, so an
  ambiguous pane is not yet a held reason either.
- **The ticker also writes one inbox item of kind `lane` per ambiguous pane,
  once**, so a project whose inspector you are not looking at still says
  something. That item is the least reliable of the three (it fires once, and
  its deduplication key is the state, not the tick) and is explicitly **not**
  the only source of visibility — the `context` line is. **This item lands with
  commit 3**; today the only standing visibility is the `context` line and
  `lane list`.
- **`hp doctor`** reports an ambiguous pane the same way it reports a broken
  `AGENTS.md` prefix, so a periodic `doctor` cycle surfaces it too (it already
  walks every project, in `src/doctor.rs`). **That report lands with commit 7**;
  until then `doctor` says nothing about lanes.

This costs one line per turn and is the cheapest thing in the design that would
have caught a week-long silent stall.

## 6. User-facing operations

The CLI keeps its shape: `hp` below is the printed prefix
(`<binary> --root <root>`).

### 6.1 `hp open <slug> --lane <name> [--lane-name <label>]`

*Not built yet: this section is commit 4 (§12).*

- No `--lane`: the default lane, exactly today's behaviour (`--new`, `--profile`,
  `--tab`, `--rebind`, `--session` unchanged). Projects that never use lanes see
  no difference at all.
- `--lane <name>` that does not exist: create it (`c-<next>`), with
  `--lane-name` as its label if given, else the name as the label. Then open a
  coordinator for it — a new tab in the project's home Space when the default
  lane's home Space exists, else the current workspace-creation path.
- `--lane <name>` that exists and has a live coordinator: focus that pane (the
  existing "running" branch of `open` in `src/coordinator.rs`, but scoped to the
  lane).
  `--new` starts another instance for the same lane — allowed, but the lane still
  has one record; the last one to speak wins rule 2, and rule 1 keeps the other
  one honest.
- The lane is written into the record it starts and into `lanes.json`
  (`pane_id`, `agent_session`, `last_active`), and the pane gets `hp_lane`.

### 6.2 `hp lane adopt <name>`

*Not built yet: this section is commit 4 (§12).*

For the case where the user opened a tab and started an agent by hand. Adopt
means: "the pane I am running in is a coordinator of lane `<name>`".

- Reads the current pane (`HERDR_PANE_ID`), the session (`HERDR_SOCKET_PATH`) and
  the agent's native session id from `herdr agent list`, exactly like the
  existing adoption paths: `adoptable_agent()` in `src/adopt.rs` (which already
  refuses a pane that is another project's coordinator, a thread, or not an
  agent) and `adopt_here()` in `src/coordinator.rs`.
- Refuses when the pane already claims another lane (use `lane assign` to move
  it, and say so), when the agent does not work in the project home (that is a
  thread, `hp thread adopt` is the right command), and when the pane is a plugin
  pane (`HERDR_PLUGIN_STATE_DIR` set — the popup must never become a
  coordinator, the same guard as `here_pane`, in `src/coordinator.rs`).
- On success: writes the lane's `pane_id`/`agent_session`, writes `hp_lane` into
  the pane immediately, and prints the prefix. If the project has exactly one
  lane and none of its rules fire, `adopt` also sets it as the default lane.

### 6.3 Command reference (中英对照)

Every lane command, with what it does. `hp` is the printed prefix
(`<binary> --root <root>`).

Of these, only `lane list` exists today (§3.5); the rest arrive with commit 4.

| Command | Effect | 作用 |
| --- | --- | --- |
| `hp open <slug> --lane <name> [--lane-name <label>] [--profile P] [--new]` | Open or focus that lane's coordinator. A missing lane is created; an existing live one is focused. No `--lane`: the default lane, today's behaviour. — not built yet (commit 4) | 打开或聚焦该 lane 的协调器：不存在就创建，存在且活着就聚焦。不传 `--lane` 就是默认 lane，即现有行为。 |
| `hp lane adopt <lane>` | Claim **the current pane** as lane `<lane>`'s coordinator (reads `HERDR_PANE_ID`, `HERDR_SOCKET_PATH` and the agent session). For a tab the user started by hand. — not built yet (commit 4) | 把**当前 pane** 认领为 lane 的协调器（读 `HERDR_PANE_ID`、`HERDR_SOCKET_PATH` 和 agent session），用于用户手动开 tab 起 agent。 |
| `hp lane list [--json]` | Every lane with the rule each live pane matched, thread and inbox counts, `last_active`, and `closed` lanes. **The counts come with commit 2; today's `lane list` prints neither** (every `Thread.lane` is still empty, so an implementation could only count 0, and a field that is always 0 misleads more than it says). | 列出所有 lane，含每个 live pane 命中的解析规则、线程数与 inbox 数、`last_active`，以及已 closed 的 lane。**线程数与 inbox 数随提交 2 提供；当前 `lane list` 不打印这两项**（现在所有 `Thread.lane` 都是空串，实现只能数出 0，印一个恒为 0 的字段比不印更误导）。 |
| `hp lane assign <lane> <pane>` | The explicit handover: point the lane (and its threads) at another coordinator pane. The **only** thing that moves ownership. — not built yet (commit 4) | 显式接手：把该 lane 及其线程交给另一个协调器 pane。这是**唯一**会转移归属的命令。 |
| `hp lane rename <lane> <label> [--name <name>]` | Change the display label, and optionally the CLI handle. The `id` never changes, so thread and inbox ownership never move. — not built yet (commit 4) | 改显示名（可选改命令里的 handle）。`id` 不变，所以线程与 inbox 的归属不变。 |
| `hp lane close <lane>` | Retire a lane: refused while any of its threads is `open`/`starting`; otherwise `status = closed`. A closed lane gets no `context` section, sidebar row or nudge, and its record and id stay. The coordinator process is left alone (§6.4). — not built yet (commit 4) | 收起来：该 lane 还有 open/starting 线程就拒绝并列出；全部 resolve 后 `status = closed`。closed 的 lane 不再有 context 分区/侧栏行/nudge，记录和 id 保留；**不杀协调器进程**（§6.4）。 |
| `hp lane open <lane>` | Undo a close: `status = open` again, and nothing else — no agent is started and no session is resumed. The output names the next step. — not built yet (commit 4) | 恢复一个 closed 的 lane：只改 `status = open`，**不启动代理、不续 session**，输出会给出下一步。 |

Ownership moves belong to the user. Neither `open`, nor `adopt`, nor the ticker
ever rewrites `Thread.lane`:
`thread start` writes the *starting coordinator's* lane once, at creation.

### 6.4 `hp lane close <lane>` — closing, not deleting

*Not built yet: this section is commit 4 (§12).*

A line of work ends before it stops existing: the incident is over, the
feature shipped, the repository moved. There has to be a way to put a lane
away. A way to *delete* one has to answer "what happens to its threads"; a way
to *close* one only has to answer "are they alive", and that is decidable. So
close exists and `lane rm` does not.

```text
hp lane close <lane>
  - the lane has any open or starting thread  -> refuse, listing the live thread ids
  - all of its threads are resolved           -> status = closed, `closed` = now
  - closed lane: no lane section in `context`, no sidebar row, no nudge, no items
    (a still-running pane of it still shows on the `Coordinators:` line, §5.4)
  - the registry record, its id and its history stay; ids are never reused
  - the coordinator process is not touched (see "Close does not stop the coordinator")
  - `hp lane list` shows it as closed; `hp lane open <lane>` reopens it
```

Details that decide the edge cases:

- **"Alive" means `Open` or `Starting`**, for local and remote threads alike —
  the same predicate the ticker already uses for "open threads"
  (`open_threads`, `src/ticker.rs`, which filters
  `Status::Open | Status::Starting`). A lane with zero threads closes
  immediately: an empty lane is the easiest thing to close and the easiest to
  reopen.
- **Refusing lists the blockers**, so the user can act on them without a second
  command:

  ```text
  lane `收藏` (c-2) still has 3 open threads: t-0140, t-0152, t-0163
  resolve them first (`hp thread resolve c-2 t-0140`), then run `hp lane close c-2`
  ```

- **Closing does not touch threads, worktrees or branches.** All threads are
  resolved *before* a close can succeed, so there is nothing left to orphan. A
  close that races a `thread start` loses: the start either lands before the
  check (the close then refuses) or after the lane is closed (the start is
  refused and says so, because `--lane` naming a closed lane is an error, not an
  implicit reopen — reopening is an explicit command).
- **A closed lane still owns its history.** `Thread.lane` keeps pointing at the
  id, and `lane list --json` still returns resolved threads' counts, so
  "which line was that work in" stays answerable long after the lane is put
  away.
- **A closed lane is not an orphan.** §7's "lane X still has N live threads"
  reminder keys on *open* lanes with *live* threads; a closed lane has none by
  construction, so it can never trigger it. Reopening a closed lane that somehow
  has live threads (a hand-edited registry) puts it straight back under §7's
  rules.

#### Close does not stop the coordinator, and open does not start one

Same reason as decision 2: `close` answers "are the threads alive", so it does
not also reach over and change process state. Both directions follow from that.

**`hp lane close` does not kill the coordinator.** The pane may still be doing
something else, or the user may simply want the line out of the way. After a
close, if that pane is still alive it is still that lane's coordinator — the
registry keeps its `pane_id` and `agent_session`, and rule 2 of §5.2 still
resolves it. What changes is reachability: the lane has no section in `context`, no
sidebar row and no nudging, so **it receives no new events**. Its pane still
appears on `context`'s `Coordinators:` line, marked `closed` (§5.4) — the one
place where "put away but still running" stays visible.

The combination has to be stated plainly, because it looks contradictory:

```text
lane closed + its coordinator pane still alive
  = a workspace that is put away but still running
```

That is a legitimate state, not a bug: the coordinator keeps whatever work it
was mid-way through, and stops being woken with new inbox items. A user who
wants it actually stopped closes the pane, or runs `hp thread stop` on a thread
— closing a lane is not that command, and `lane close` says so in its output
rather than silently killing a pane somebody is looking at.

**`hp lane open` only flips `status = open`.** It starts no agent and resumes no
session. Its output names the next step, because "open again" and "running" are
not the same thing:

```text
lane `收藏` (c-2) is open again; its coordinator is not running.
Start or resume one with `hp open law --lane c-2`.
```

When that lane's coordinator pane is in fact still alive (the combination
above), the output says so instead and does **not** suggest `open` — suggesting
it would start a second coordinator beside the one already running:

```text
lane `收藏` (c-2) is open again; its coordinator is already running in pane w1AB:p1.
```

**Which session a reopen resumes is `open`'s existing decision, not the lane's.**
`lane open` records no "resume this" hint. `open` already decides: it resumes
only when the new launch is not `--new`, the agent kind matches the recorded one
and the profile matches the recorded profile (the resume filter in
`coordinator::open`, `src/coordinator.rs`); otherwise it starts fresh. Lanes
inherit exactly that rule — a second resume policy would be a second source of
truth about a conversation.

#### Default lane election

Closing the default lane is allowed. A project always has a default lane, so one
is elected when the current default closes:

```text
default lane election, when the current default is closed:
  1. among open lanes, the one with the newest `last_active`
  2. tie -> the lowest id (c-1 before c-2, creation order)
  3. no open lane left -> reopen c-1 (a project always has a default lane)
```

- **Why `last_active`**: it is the only field that means "recently in use", and
  that is the closest thing to "the line you are working on" — closer than
  "declared first", which is only a file order. `c-1` wins ties only because a
  stable, predictable id beats a coin flip.
- **What the default lane is for, and what it is not.** It decides exactly two
  things: where an `open` with no `--lane` goes, and who receives the orphan
  reminder of §7. It does **not** own threads, does not gate what `context`
  prints, and grants no privileges — a lane is not more or less a lane for being
  the default. `lane list` marks it so nobody has to guess.

## 7. Orphans: a lane with no coordinator

*Not built yet: the ticker's `lane` item is commit 3 and the `hp lane assign`
it suggests is commit 4 (§12).*

Situation: lane `c-2` has 12 open threads, its coordinator pane is gone, and
`lanes.json` still remembers a dead `pane_id`.

The rule: **nothing is adopted automatically, and nothing is silently dropped.**

- Only **open** lanes are considered. A closed lane (§6.4) has no live threads
  by construction, so it can never be reported as an orphan; this is the point
  where decisions 4 and 9 meet. `hp lane close` refuses while a thread is alive,
  so "closed but orphans exist" is not a state the model can reach.
- The ticker detects it: an **open** lane has at least one `Open` or `Starting`
  thread (`open_threads`, `src/ticker.rs`), and rules 1 and 2 resolve to no live
  pane.
- It writes **one** inbox item, kind `lane` (a new kind next to `thread-state`,
  `session`, `space`, `routine`, `outage`; it renders through the existing
  nudge path, `steps::nudge_text`, `src/steps.rs`), addressed to the
  **default lane** — that election is one of the two jobs the default lane has
  (§6.4), and it is why a project with a closed default still needs one:

  ```text
  - 20261025T104200Z-lane-c-2-7 [lane] c-2: 12 open threads, no coordinator
    Lane `收藏` (c-2) has 12 open threads and no live coordinator.
    Reopen it with `hp open law --lane books`, or move its threads with
    `hp lane assign c-2 <pane>`. Nothing moves until you say so.
  ```

- Deduplicated like the other "write once" items: the state file records the
  (lane, thread-count) pair the item was written for, so a lane that stays
  orphaned is not announced every 15 seconds, and a lane whose count changes
  (threads resolving) is announced once more at most.
- The coordinator that receives it may **only** report it to the user. It must
  not run `lane assign` on its own: the skill says so in as many words, and
  `lane assign` (like `thread resolve`) is a command the user confirms. A
  receiving coordinator that starts working another lane's threads, or resolves
  them, is a bug, not a feature — this is the same "data is not instructions"
  boundary the skill already draws for reports and inbox items.
- When the user does confirm, `hp lane assign c-2 <pane-of-the-receiving-coordinator>`
  moves the lane and its threads, prints what moved, and the default lane's next
  `context` no longer lists them.

The alternative — the ticker handing over automatically to whoever is idle —
was rejected: it makes an outage look like progress, and it silently mixes two
work areas in one coordinator's context, which is the thing lanes exist to
prevent.

## 8. TASKS.md split

*Not built yet: this section is commit 5 (§12) — the heading convention and
`hp task` alike.*

`TASKS.md` is a single file with `## <list>` headings; `tasks::parse()`
(`tasks::parse`, `src/tasks.rs`) already builds `Task.list` from the heading,
`tasks::compact` (same file) folds bodies for `context`, and the popup groups by
list (`popup::parse_tasks`, `src/popup.rs`). **No file split is needed or wanted.**

The split is a naming convention:

```markdown
## Backlog                 <!-- no prefix: shared, every lane sees it -->

## 抢修/Backlog            <!-- lane label (or handle) / list -->
- [ ] Rotate the leaked signing key (me)
- [ ] Truncated log lines hide the 500 (codex-fast@m1) · t-0171

## 抢修/Doing

## 收藏/Backlog
```

- A heading whose part before the first `/` matches a lane's `label` (or its
  `name`) is that lane's. Everything else is shared.
- `context` prints the shared lists plus **only its own lane's** lists; the
  lane's `TASKS.md` section is the one place its coordinator writes.
- The popup's tasks section shows the same split, with a lane column/section
  rather than a second view (§10).
- No task moves between lists on its own. If the user wants to move work from
  one lane to another, that is an edit of the heading (or a sentence to a
  coordinator), not a migration.
- A **closed** lane's headings (`## 收藏/Backlog`) stay where they are and stop
  being printed to anyone's `context`; opening files by hand still shows them, so
  closing a lane never hides or deletes a task. Reopening the lane brings its
  lists back without an edit.

### The write path is the risky part

The module comment at the top of `src/tasks.rs` says it plainly: *"The
coordinator is the only writer; this module only reads it."* Today the
coordinator edits the file with its own tools, and the only thing keeping two
writers apart is `project.lock()`
(`Project::lock`, `src/project.rs`), which no text edit takes.

With several coordinators that becomes a real race: two agents rewriting one
file, one of them losing the other's section, and no record of it.

Recommendation, to be treated as **the highest-risk change in this design and
committed separately**: add a narrow write command —

```sh
hp task add <slug> <list> "<title>" [--owner O] [--lane L] [--notes TEXT]
hp task move <slug> "<title>" <list>
hp task done <slug> "<title>"        # ticks the box; keeps the line
```

Each is one `project.lock()`-protected read-modify-write of `TASKS.md` that
preserves every other section byte-for-byte. The skill then says "never edit
`TASKS.md` directly; use `hp task`", and the popup's existing "send a sentence
to the coordinator" actions in `src/popup.rs` keep working unchanged.

Until `hp task` exists, lanes must not be rolled out with more than one
coordinator writing tasks: a lost task section is silent data loss, and it is
the kind of loss nobody notices for a week.

## 9. Sidebar, grouping, lifecycle

*Not built yet: this section is commit 6, except the `Coordinators:` line in its
last bullet, which commit 1 already prints (§12).*

- **Display name.** `sidebar::coordinator_display()` (`src/sidebar.rs`)
  returns `"<project name>␣"` + `HEAD_MARK` for every coordinator, so two
  coordinators are two identical rows. It becomes `"<project> · <lane label>"` +
  `HEAD_MARK`. With one lane the row stays exactly as it is today (no
  `· <lane>` for the default lane in a single-lane project; a project with lanes
  always shows them, including the default, so the rows are distinguishable).
- **Sort key.** `grouping::coordinator_key()` (`src/grouping.rs`) is
  `"<slug>!0!<pane>"`, i.e. coordinator order is pane order.
  It becomes `"<slug>!0!<lane-rank>!<pane>"` so coordinators sort by lane
  declaration order (default first) and panes break ties inside a lane. `!0!`
  stays, so the `plan()` owner map (same file) and the `!0!`/`!1!`
  split between coordinator and thread keys need no change. `lane-rank` must be
  fixed-width (`{rank:02}`): the keys are sorted as plain strings, and `10`
  sorts before `2` otherwise.
- **Home Space.** `ProjectPart.home` (`src/grouping.rs`) stays one Space:
  every lane's coordinator lives in the project home by decision 1. `part()`
  (`part`, `src/ticker.rs`) already collects coordinators' `workspace_id`s, so a
  lane opened in another Space (a `--kind`-style escape hatch later) still shows.
  `spaces::empty` (`src/spaces.rs`) must exclude **every** lane's
  `workspace_id` from its "orphan home" sweep, not just `coordinator().workspace_id`
  (same file), otherwise it will offer to close the Space of a
  live lane.
- **Lifecycle.** `lifecycle::close_workspaces()` (`src/lifecycle.rs`) closes
  `coordinator().workspace_id`; it must iterate the lanes
  (`alive_panes`, same file, likewise). `archive` must not leave a
  lane's coordinator behind; `delete` is unchanged (whole project).
- **Closed lanes leave the views.** `part()` (`src/ticker.rs`) builds the
  sidebar from live coordinators and recorded panes, so a closed lane's row
  simply disappears with its pane — nothing has to be added — but
  `spaces::empty` must not offer to close the home while an open lane still
  lives in it (the Home Space bullet above), and `overview` / `needs-you`
  must filter on `status == open` when they iterate lanes.
- **`hp context` must be lane-aware**, and its header carries the identity line
  from §5.4 on **every** run. It currently prints the whole digest
  (`digest()`, `src/coordinator.rs`). After the change the first lines are
  `Coordinators: …` (which rule each live pane matched, and the fix for an
  ambiguous one), then the lane's header (`lane c-1 · 抢修`), the lane's tasks,
  the lane's inbox, the threads of the lane — and a one-line summary of the
  other open lanes ("c-2 收藏: 12 open, 3 unhandled") so a coordinator can say
  "that is the other line's" without reading it. **Closed lanes get no section
  of any of it** — no tasks, no inbox, no threads — but a still-running pane of
  a closed lane stays on the `Coordinators:` line (marked `closed`, §5.4) and in
  `lane list`.
  `context` also passes the lane to `inbox::mark_seen`.
  `skill/COORDINATOR.md` is the actual program here; it has to say all of this
  (§12).

## 10. Compatibility and migration

- **Empty `lane` = default lane.** On every read path: `Thread.lane`,
  `inbox::Item.lane`, the task heading's missing prefix.
- **Old `coordinator.json` is the default lane's view.** `Project::coordinator()`
  (`src/project.rs`) keeps returning `Option<Coordinator>`; it becomes "the
  default lane's record". So do its 63 call sites in `src/` — they keep working
  for single-lane projects without a rewrite, which is what makes the rollout
  incremental (§12, the commit plan).
- **`lanes.json` is created lazily.** First `open` on a project with no lane
  registry writes
  `{"lanes":[{"id":"c-1","name":"default","label":"<project name>","status":"open",…}],"default":"c-1","next":2}`
  from the existing `coordinator.json` (`pane_id`, `agent_session`, `profile`).
  Nothing to run, nothing to migrate, nothing that can half-finish.
- **The old CLI is unchanged.** `open`, `context`, `thread`, `inbox`, `popup`,
  `needs-you`, `overview`, `archive`, `rename`, `doctor` all keep their
  behaviour for single-lane projects.
- **`rename <slug> <new-slug>`** keeps working: it rewrites `coordinator.json`
  paths (`src/rename.rs`) and moves the folder. It only has to iterate the
  lanes where it iterates one record: reopen each lane's coordinator in the new
  folder, re-report the `hp_lane` tokens, and rename the home Space once.
- **Downgrade.** A file written by a lane-aware binary still loads in an old
  one: `#[serde(default)]` on the new fields, unknown keys ignored. The old
  binary drives `coordinator.json` and ignores `lanes.json` — which means it
  will happily re-record the default lane and ignore the others. That is
  acceptable *only* while a project has a single lane; say so in the docs and in
  the release note.

## 11. Not doing (and the later option C)

Explicitly out of scope for plan B:

- **Memory partitioning (plan C).** Today `thread::brief_for` (`src/thread.rs`)
  inlines the whole `MEMORY.md` plus every `memory/*.md` into every brief, capped
  at `MEMORY_CAP_CHARS = 32_000` (`src/thread.rs`; the cap is applied inside
  `brief_for`). `law` is at 14 KB of `MEMORY.md` and 29 memory files, so a
  lane-specific memory split (e.g. `memory/<lane>/` or a `lane:` tag on each
  file) is where this goes next —
  including the "not inlined because over the cap" line in `brief_for` (`src/thread.rs`),
  which two lanes will hit faster than one. Not designed here.
- **Routines per lane.** `routine::Routine` (`src/routine.rs`) has no lane
  field; every routine fires for the project. A `pr-followup` for one lane's
  threads is a reasonable want; it is a separate change (routine state
  `States`, `routine::load_all`, the `pr` trigger's thread filter).
- **A popup view per lane.** The popup's sections would need a lane scope
  (like the existing project scope). Today the proposal is only "tasks show
  their lane" (§8).
- **Per-lane Spaces.** Decision 1 keeps every coordinator in the project home.
  `--kind tab` style escape hatches for a coordinator would be a new placement
  mode, not part of this.
- **Lane deletion.** `hp lane rm` is **not built**, deliberately: deleting needs
  an answer for "what happens to its open threads" and there isn't one. Closing
  (`hp lane close`, §6.4) answers the only question that is decidable — "are
  they alive" — and keeps the record. A closed lane is reversible; a deleted one
  would not be.

## 12. Risks, and how the work is cut

Three risks are shared by every commit below and cannot be removed by any one of
them:

**(a) Identity degrades silently, and this is not hypothetical.** This project
already lived through it. The `herdr-projects` coordinator sat with both tests
dead — its pane's `own_dir` was `/Users/maxyou` rather than the project home,
and its recorded session id no longer matched — so `is_coordinator` answered *no*,
the project was treated as having no coordinator, and nudges stopped. `nudged`
in `.state/ticker.json` froze on the hash of the 2026-10-18 item and never
changed again; the ticker logged nothing (the *no coordinator* branch logs
nothing at all) and no inbox item was written. A finished thread did not report
itself, and it was found a week later only because the user asked why.

The standing-failure shape is what makes this dangerous: there is no transition
to catch, so a mitigation that fires "when the state changes" fires never.
Under lanes the same shape returns with more ways in — two coordinators can both
degrade to the default lane and the sidebar looks calm.

Mitigation, all of it in §5.4 and none of it optional: `hp context` prints the
`Coordinators:` line **every turn** with the matched rule, the lane and (for an
ambiguous pane) the `hp lane adopt` command that fixes it; `lane list` prints
the same per pane; ambiguous panes are never nudged, prompted or assigned; the
ticker additionally writes one `lane` item per ambiguous pane, and that item is
**not** the only source of visibility — a once-only item is precisely what
missed the week-long stall. Making the `Coordinators:` line part of commit 1
rather than of this document is what turns a design rule into something the
running binary says.

**(b) Shell cwd drift can drop a coordinator back to the default lane.**
`local-notes/local-dev.md:72` records this as a real, twice-observed incident
(a coordinator shell left in another checkout; nudge silently stalled for all
projects). Under lanes the stakes go up: rule 3 would map a drifted coordinator
onto the *default* lane even though it is serving another one — if its token
expired and its recorded session id changed at the same time. Mitigation: the
ticker re-reports `hp_lane` every tick from the registry (so a drifted pane keeps
its token), rule 3 only fires for single-lane projects, and anything else
refuses.

**(c) Test debt in `src/scenarios.rs` may exceed the implementation.**
`src/scenarios.rs` is 3 734 lines with 141 functions, 28 of which call
`project.coordinator()` / `update_coordinator`, and it is the integration suite
that proves most behaviour ("389 unit + 12 integration" in the memory notes).
Every one of the six decision points in §5.2 changes what those scenarios
assert. Estimating the lane work without estimating this rewrite is how the
project gets a half-lane binary: budget it inside commit 1 and 2, not as a
cleanup at the end.

### Suggested commits (each one runs, tests and reverts on its own)

1. **Data model + registry + identity.** `lanes.json`, `Thread.lane`,
   `inbox::Item.lane`, the §5.2 resolver as one function
   (`resolve_lane(project, agent) -> LaneResolution`, three-valued), the
   `Coordinators:` header line in `context` (§5.4), `hp_lane` token writes in
   `sidebar::report_pane`, `lane list`. `Project::coordinator()` delegates to the
   default lane. No behaviour change for single-lane projects; the scenario
   suite is adapted here. The header line belongs in **this** commit, not in the
   docs one: it is the only mitigation that would have caught the week-long
   stall, and it is worth having before any lane exists at all.
2. **Ownership.** `Thread.lane` and `inbox.lane` writer and readers, `steps::State`
   per lane, `inbox::seen`/`mark_seen` per lane, `context` lane-scoped.
3. **Routing.** `nudge_target()` (`src/coordinator.rs`) returns one target
   *per lane*; `nudge()` runs once per lane; `prompt()` (same file) resolves by
   lane; `discover()` (same file) stores the lane in `LivePane`.
4. **Open / adopt / assign / close.** `open --lane`, `--lane-name`,
   `hp lane adopt`, `hp lane assign`, `hp lane rename`, `hp lane close`,
   `hp lane open` (§6.4). Closing is a registry status change and a filter, not a
   deletion, so it lands with the same commit that owns the registry; so does the
   default-lane election, because a closed default leaves the registry with no
   default until one is chosen.
5. **TASKS.md split + `hp task`.** The heading convention first (read-only,
   `context`/popup honour it), then the locked write commands (§8). Highest
   risk; no other commit in the same PR.
6. **Sidebar, grouping, lifecycle.** `coordinator_display`, `coordinator_key`,
   `spaces::empty`, `lifecycle::close_workspaces`, `overview`.
7. **Docs + skill.** `skill/COORDINATOR.md`, `docs/operations.md`, `README.md`.
   The skill is the real program: `hp context` must become lane-aware and the
   skill must teach the lane rules (own lane's tasks and inbox; other lanes are
   data; never hand over ownership; never write another lane's threads).

Sizes are not equal on purpose. 1 and 2 are the load-bearing ones and they are
where the scenario rewrite bites; 5 is the one that can lose data and should be
reviewed as if it were the only change in the release.

## Appendix: the decision points

Every place that today answers "which coordinator?" and what it must become.
The Where column names **symbols, not line numbers**: a line number here was
correct only for the commit this document was written on, and every commit of
the lane series moves them. `rg` the symbol to find it.

| Where | Today | Under lanes |
| --- | --- | --- |
| `coordinator::nudge_target()` (`src/coordinator.rs`) | newest idle pane across all `LivePane`s | group by lane, pick one per lane |
| `coordinator::prompt()` (`src/coordinator.rs`) | `max_by_key(state_change_seq)` to one pane | resolve the target's lane first, then match |
| `inbox::mark_seen()` (`src/inbox.rs`), called from `coordinator::context` | one global seen set | one seen set per lane |
| `steps::State` (`src/steps.rs`): `nudged`, `box_pane` | one scalar each | one entry per lane |
| `threads::place_tab()` (`src/threads.rs`) | the home Space of the record `Project::coordinator()` returns | the initiating lane's coordinator's Space |
| `threads::called_from_coordinator()` (`src/threads.rs`) | compares one record's socket/workspace/tab/pane | compares against any lane's record, else lane A's `thread resolve` re-adds the item to lane B |
| `lifecycle::close_workspaces()` (`src/lifecycle.rs`) | closes `coordinator().workspace_id` | iterate all lanes |
| `coordinator::is_coordinator()` / `session_matches()` (`src/coordinator.rs`) | dir (`works_in`) or one record's session | the §5.2 resolver |
| `coordinator::found()` / `discover()` (`src/coordinator.rs`) | "the most recently active agent in the folder" | record the lane the resolver returns; rule 4 writes a note instead |
| `coordinator::open()`'s reuse branch (`src/coordinator.rs`) | newest of the `running` set | the target lane's live pane only |
| `coordinator::open()`'s record write (`src/coordinator.rs`) | `*c = Coordinator{…}` replaces the record | write that lane's entry in `lanes.json` |
| `spaces::empty()` (`src/spaces.rs`) | excludes `coordinator().workspace_id` | excludes every lane's Space |
| `sidebar::coordinator_display()` (`src/sidebar.rs`) | project name for every coordinator | `<project> · <lane>` |
| `grouping::coordinator_key()` (`src/grouping.rs`) | `<slug>!0!<pane>` | `<slug>!0!<lane-rank>!<pane>` |
| `coordinator::digest()` (`src/coordinator.rs`), reached through `coordinator::context()` | prints state, never says who the coordinator is | first line is the §5.4 `Coordinators:` line, every turn |
| `tick_cheap()`'s `coordinator_missing` (`src/ticker.rs`) | a missing coordinator only feeds `session_lost`; nothing is logged or written | an ambiguous pane is logged as a held reason and written as one `lane` item |
| `ticker::open_threads()` (`src/ticker.rs`) | `Open \| Starting` per project | the same predicate per lane, and the basis of `lane close`'s refusal and of §7's orphan test |
