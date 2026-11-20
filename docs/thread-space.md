# The project's thread Space

A project keeps **two** Spaces in herdr: its own (`<display name>`, holding the
`coordinator` tab) and one for all of its threads (`<display name> Threads`,
holding one `t-00xx` tab per thread). No thread has a Space of its own.

This page is the reference for that rule: where a thread's tab goes, what the
Space is called, how it is found again, when it is closed, and what would break
if any one of those were done the obvious way. It was implemented in t-0062
(`src/threads.rs`, `src/core.rs`, `src/spaces.rs`, `src/labels.rs`); the second
step — dissolving the `tab`/`home` kinds into one `local` — is in §9.

## 1. Why

Before this, each thread opened a Space of its own and that Space was named after
the thread — a long Chinese sentence for most of this project's threads. The
sidebar was therefore a flat list of unrelated long titles, one per thread, and
the project's own Space was one line among them.

herdr is two levels deep on purpose: a Space holds tabs. The coordinator already
uses that shape (one project Space, a `coordinator` tab). Threads use it the same
way now:

```
Spaces                                  tabs
├─ Herdr Projects                        coordinator
├─ Herdr Projects Threads                t-0058
│                                        t-0059
│                                        t-0060  …
├─ Law Office                            coordinator
└─ Law Office Threads                    t-0304
                                         t-0305
```

The thread's title still exists — in the record, in `hp_display` (the pane label
and every row `hp list`, `hp overview` and `hp context` print), in the brief, in
the branch name — but no longer in a Space name. A thread's tab is its id
(`labels::thread_tab_label`).

**This supersedes the 2026-11-17 decision** that a thread's Space is its title
(`memory/preferences.md`). The user changed it on 2026-11-19: the sidebar was
drowned in parallel long titles, and "project Space → thread Space → thread tab"
is what herdr's two levels are for. `tight_cjk` stays for display strings, not for
Space names (§4).

## 2. Where a thread's tab goes

Every kind lands in the same Space; only the folder and what is cleaned up
afterwards differ.

`--kind` answers one question — **where does the thread work** — and the Space
is the same for all of them. `tab` and `home` used to be two more names for
what `local` is (one with a Space of its own, one a tab of the project's); they
are gone, and both still parse from an old record and from the command line.

| `--kind` | folder (the tab's `cwd`) | Space | tab | `hp_worktree` label |
| --- | --- | --- | --- | --- |
| `worktree` (default with `--repo`) | `~/.herdr/worktrees/<repo>/<branch>`, from `git worktree add` (`core::worktree_add`) | `<display name> Threads` | `t-00xx` | the worktree path |
| `checkout` | the repository's main checkout (`record.repo`) | same | `t-00xx` | none |
| `local` (default without a repo; `tab`/`home` are its old names) | `<root>/<slug>/threads/<id>/` | same | `t-00xx` | none |
| `adopted` | whatever the pane already runs in | unchanged | unchanged | unchanged |

Two things follow:

- **A thread no longer needs the project open.** The old `tab`/`home` placement
  refused with "the project's workspace is not open; run `open <slug>` first".
  That guard is gone: a thread needs a reachable session, not the project's own
  Space (a Space of its own cannot be a twin of anything now). `thread start`
  still refuses when the session cannot be reached at all
  (`threads::require_session`).
- **The project's own Space holds exactly one pane**, the coordinator's. That is
  what `docs/lanes.md` assumes when it treats a lane's home Space as the
  coordinator's, and it is what makes `coordinator::project_workspace` reliable.

## 3. The names

| thing | name | where |
| --- | --- | --- |
| project Space | `<display name>` (`project::home_label`) | `hp_ws_label` on its terminals, the app's `space.rename` |
| thread Space | `<display name> Threads` (`labels::threads_space_label`) | same |
| thread tab | `t-00xx` (`labels::thread_tab_label`) | `hp_tab_label`, the app's `tab.rename` |
| pane (one tab) | `hp-<slug of the tab label>-<6 hex>` | core's terminal name |
| thread row | `t-00xx <tightened title>` (`labels::thread_display`) | `hp_display` |

- **`tight_cjk` is not used for the Space name.** It drops a space that touches a
  Han character, which is right for a title (`今天 1 月 abc 课程` →
  `今天1月abc课程`) and wrong for `法律事务所 Threads` → `法律事务所Threads`. The
  Space name is Latin-suffixed on purpose, so it keeps its space and the display
  names inside it are untouched.
- **Uniqueness.** A project's two Spaces differ by the suffix, so a project
  literally named `X Threads` gives `X Threads` and `X Threads Threads` — no
  collision inside one project. Across projects the labels repeat (`Law Office
  Threads` twice), exactly as they already do for project Spaces: a Space is
  identified by its panes and its id, never by its label alone (§5).
- **A display name that changes** (`hp set <project> name …`, or `rename …
  --name`) is repaired by the ticker: `spaces::sync_threads_label` runs next to
  `sync_home_label` on every slow tick, and `rename` calls it once itself. It
  renames the Space it finds — it never makes a new one — so a rename cannot
  leave a `Demo Threads` behind beside a `Law Office Threads`.
- **`is_threads_space_label`** is `label.ends_with(" Threads")`. That is how the
  Space is recognised while its label is out of date, and it is why the suffix
  is a constant (`labels::THREADS_SUFFIX`) rather than a format string spread
  over three modules.

## 4. How the Space is found, and what must not be mistaken for it

`threads::threads_space(project, workspaces, panes, agents, record)` answers "is
there a thread Space, and which". In this order, first match wins:

1. **the Space `record` names** (its `workspace_id`), when that id is still
   listed and is a thread Space. This is what makes a restart land where the
   thread was, even after the display name changed and the label has not caught
   up yet.
2. **the Space another open thread of the project names.** A new thread joins
   them instead of opening a Space beside them.
3. **the Space carrying today's label** (`<display name> Threads`).
4. **any Space this project's thread panes are in** — a pane carries
   `hp_project=<slug>` (`labels::report_pane`). This is the fallback for a Space
   the app made whose `hp_ws_label` is empty or stale: the panes are what say
   whose it is, and a label change cannot touch them.
5. **`None`** — there is none, and the caller creates the first one.

A Space is usable (all of the above require it) only when it is

- **not one of the project's coordinator Spaces** — the recorded
  `workspace_id`, the Space the recorded coordinator pane is in
  (`coordinator::project_workspace`), and every open lane's Space
  (`lanes::open_spaces`). A coordinator pane also carries `hp_project`, so the
  pane rule in 4 would otherwise happily hand the project's own Space to a
  thread;
- **not a worktree Space** (`workspace.worktree.is_none()`, see §7.4);
- **not labelled with a thread's title** — that is a thread's *own* Space from
  before this change (below).

**Never a thread's own Space from before.** Such a Space is labelled with the
thread's title (`labels::thread_space_matches`), and a restart must abandon it,
not join it: its tab is closed at resolve, and `spaces::empty` recycles the Space
once its panes are gone (§6.4). The one residue: a thread whose *title* ends in
` Threads` is indistinguishable by label from a thread Space, so its old Space may
be joined once. That is cheap, one-off, and does not move any work.

**Why not "just create it when it is missing".** Because the failure is silent
and compounding: every judgement call that misses opens one more Space per
thread, which is the situation this change exists to end, with the added
confusion that the Spaces now share one name. So the order above prefers
*anything* recorded over the label, and the label over creation. There is
deliberately no "renamed project → new Space" path.

## 5. Lifecycle

**Create.** `threads::place` looks for the Space; when there is none it takes
the project lock, looks again, and only then calls `core::workspace_create` with
the thread's own folder and its `t-00xx` tab. The second look under the lock is
what stops two threads started at the same moment from opening two Spaces: the
first one's Space is listed by core (and labelled) before the second gets the
lock. The Space's own folder is the first thread's folder; every later tab
carries its own `cwd`, so the Space's folder means nothing after the first
thread (it is not used by any code).

**Reuse (restart).** `thread restart` with a dead pane goes through
`place_tab`/`place` again, which finds the Space by rule 1 above — the record's
own id — so a restart never opens a second Space. A *worktree* restart places
the tab only (`core::worktree_add` is not called; the folder is already there)
and re-marks `hp_worktree` on the new pane.

**Close (one thread).** `threads::clean` closes the thread's tab
(`threads::close_place`), and the Space only when that tab was **its last pane**:
`others == 0` is counted over the pane list taken before the close. Resolving one
of three threads in a Space therefore closes one tab and nothing else.

**Recycle (backstop).** `spaces::empty` reports the thread Space only when

- it still has at least one pane, every one of them is in it (`own_panes.len() ==
  space.pane_count`, so a hidden pane is never missed),
- no pane of any **open** thread of any project is in it (`open_panes`),
- no agent is in it, it is not focused (unless `include_focused`, which only
  `sweep` passes), and every pane is an idle shell.

That covers what the close path cannot: an app placeholder shell, or a shell a
person opened there. It can never close a Space with a live thread in it, and it
never looks at the label — a Space whose label stopped matching something is
still somebody's.

**Forget.** `spaces::forget` clears the `workspace_id` of every unresolved
thread naming the closed Space, so the next placement creates a new one instead
of writing into an id nothing lists.

**A Space with no panes is invisible.** `Core::workspace_list` derives Spaces
from terminals, so a Space whose last pane is gone is not listed at all: the
recycle sweep cannot see it, and `Core::workspace_close` on it is a no-op.
Closing it therefore happens where the last pane is closed
(`threads::close_place`), not in a sweep. In practice a headless Space vanishes
on its own; one the app draws is closed by the app when its last tab goes, and
the `workspace_close` we attempt is harmless.

## 6. The parts that would break if done the obvious way

### 6.1 `Core::worktree_remove` is keyed on a pane, never on a Space

`core::worktree_remove(pane)` reads the worktree path off **that pane's**
`hp_worktree` label and runs `git worktree remove` on it. It used to take a Space
and use "the first terminal in it", which is exactly right while a Space holds
one worktree and a data-loss bug the moment two do: resolving thread B would
remove thread A's folder. This is the one change here that can destroy work, so
it is tested on its own, with real git (`threads::tests::
a_worktree_removal_in_a_shared_space_takes_that_worktree_and_no_other`: two
worktrees of one repo, two panes in one Space, the **second** one resolved).

Same reasoning in two other places:

- `threads::worktree_pane(record, panes)` finds the pane by its `hp_worktree`
  label first, and only then by the recorded pane id + folder. A pane that
  happens to `cd` into the worktree does not count, and neither does another
  thread's pane in the same Space.
- `sweep` finds an orphan worktree's pane by the label as well, and its
  `Orphan::Workspace` (which used to *close a Space* for a resolved thread) is
  now `Orphan::Pane`, which closes that one pane. Closing a Space there would
  have taken every other thread down with it.

### 6.2 `spaces::empty` judges the shared Space by panes, not by its label

See §5. The old rule was "a plain Space labelled with a thread's title, with no
agent and only idle shells" — the thread-Space rule above never consults a
title, and the "in use" test had to change with it: a shared Space is named by
*every* open thread's `workspace_id`, so "is a record naming it" is always true
and must not be the test. It is the **panes** (`open_panes`) that decide.

### 6.3 Reuse cannot be "the Space's label is the thread's title"

That check is gone from the placement (`thread::recorded_own_space` was deleted).
Reuse now asks three questions instead — is the recorded id still listed, is it
not the project's Space, is it not a worktree Space — plus "is it a thread
Space", answered by the label suffix and by the panes. A restart after a display
name change therefore still reuses (rule 1 of §4), and a restart in a project
whose label is `Demo Threads`, `hpc`'s earlier name, still reuses.

### 6.4 A Space is a worktree Space only when all of its panes agree

`Core::workspace_list` used to mark a Space as a worktree Space when **any**
terminal in it carried `hp_worktree`. With threads sharing one Space, that made
the thread Space a worktree Space — with the path picked from whichever pane was
listed first — and the orphan-home rule and the thread-Space rule each draw a
different wrong conclusion from that. It now marks a Space only when **every**
terminal names the same worktree, and `Workspace::worktree` is simply that path
(`Option<String>`). A Space of one worktree (the old shape) is unchanged.

(Related, and gone with step 2: `spaces::primary` looked for herdr 1's
"repository Space" grouping — a Space with `worktree.is_linked_worktree` false.
Nothing in the herdr-core port ever set that flag, so the repository-Space rule
never fired; it and `Thread::repo_workspace` are deleted, not ported. §9.)

### 6.5 `tab_create` on an app Space must write the Space's name too

The app branch of `Core::tab_create` used to write `hp_ws`, `hp_tab`,
`hp_tab_label` and `herdr.space`, but not `hp_ws_label`. Since `workspace_list`
takes a Space's label from its terminals, a Space whose first-listed terminal was
made that way read as `""`, the label lookup missed it, and the next thread
opened a second Space. `tab_create` now looks the label up from any terminal of
the Space (and `workspace_list` takes the first *non-empty* label it sees) and
writes it in both branches.

## 7. Red tests

Every rule above has a test that fails if the rule is undone. The list follows
the order of this page:

| what | test |
| --- | --- |
| a new thread lands in the thread Space, its tab is `t-00xx`, never the project's Space (`local`, `checkout`) | `threads::tests::a_new_thread_lands_in_the_project_s_thread_space` |
| a second thread joins it; two Spaces exist in total | `threads::tests::a_second_thread_joins_the_same_space_instead_of_opening_another` |
| a restart reuses it (no Space per restart) | `threads::tests::a_restart_reuses_the_thread_space_rather_than_opening_a_new_one` |
| a resolve closes one tab and leaves the Space, and `spaces::empty` agrees | `threads::tests::a_resolve_closes_the_tab_and_keeps_the_space_the_other_threads_are_in` |
| the Space is closed with its last thread | `threads::tests::the_thread_space_is_closed_only_when_its_last_thread_leaves` |
| `worktree_remove` takes the target worktree only (real git, two panes in one Space) | `threads::tests::a_worktree_removal_in_a_shared_space_takes_that_worktree_and_no_other` |
| a `kind = "tab"` / `"home"` record still parses, starts, and lands in the thread Space | `threads::tests::a_record_from_before_the_change_still_starts_in_the_thread_space` |
| `kind = "tab"` / `"home"` read as `local`, are written as `local`, and `--kind` still takes them | `threads::tests::the_old_kind_names_are_local`, `tests::the_old_kind_names_still_work_and_say_so` |
| `thread show` on a `kind = "home"` / `"tab"` record says `local`, and `context` still sees the thread | `tests::a_record_from_before_the_merge_still_reads_and_says_local` |
| `thread start --help` promises no Space of its own and names the three placements | `tests::thread_start_help_describes_where_a_thread_works_not_a_space_of_its_own` |
| no repo means `local`, and `--repo` alone means `worktree` | `threads::tests::placement_follows_the_request_then_the_repo` |
| a display-name change renames the Space instead of opening a second one | `threads::tests::the_thread_space_follows_the_display_name_instead_of_being_left_behind` |
| the finder's order: record → other threads → label → panes, and never a home/one-worktree/old-own Space | `threads::tests::the_thread_space_is_named_by_the_record_the_label_or_the_panes` |
| a Space holding two worktrees is not a worktree Space; one alone still is | `fake_hc::tests::a_space_holding_two_worktrees_is_not_a_worktree_space` |
| a tab created in an app Space carries the Space's name | `fake_hc::tests::a_tab_created_in_an_app_space_carries_the_space_s_name` |
| a record naming another project's Space is not trusted; a thread Space is made | `threads::tests::a_thread_record_naming_another_projects_space_is_not_trusted` |
| a legacy `tab` thread's own Space is abandoned, not joined | `threads::tests::a_legacy_tab_thread_s_space_is_left_behind_and_its_tab_joins_the_thread_space` |
| a checkout thread restarts into the thread Space, still on the main checkout | `threads::tests::a_restarted_checkout_thread_joins_the_thread_space_on_the_repository` |

## 8. The app side

- **Many tabs in one Space is the intended shape**, but it is new for the app:
  a Space with ten `t-00xx` tabs has a tab bar the window has to scroll, and the
  sidebar now shows one line per project instead of one per thread. Nothing in
  `hp` can fix a rendering problem there; the rollback is one commit (this one)
  and one `cargo build`, which is why the change is confined to
  `threads_space`/`place`/`close_place`/`worktree_remove`/`workspace_list`/`empty`
  plus one label function.
- **Naming goes through the host that draws it.** A Space the app made (`sp…`)
  is renamed with `terminal space.rename` (`core::name_space`) and its tab with
  `tab.rename`; hp's own labels are written either way, and they are the record.
  A headless Space (`hpw…`) has no app name at all: it appears when the window is
  next opened with that Space in the session.
- **`hp_display`** is unchanged: a thread's pane still says `t-00xx <tightened
  title>`, so `hp list`, `hp overview`, `hp context` and a terminal's own label
  still name the thread. The tab alone says `t-00xx`.
- **The project's Space is not touched** by any of this: `open`, `lane open` and
  `sync_home_label` behave as before.

## 9. What is left

**Step 2 landed with this change: `tab` and `home` are one `local`.** They had
described a difference that no longer existed ("a Space of its own" vs "a tab of
the project's Space"), and `--kind` had stopped deciding anything about the
sidebar at all. Now:

- `Kind` is `Worktree`, `Local`, `Checkout`, `Adopted`. Records serialise the
  kind as `local`.
- `#[serde(alias = "home", alias = "tab")]` on `Local` keeps every record
  written under an old name readable — the record is read on every tick, and a
  value that failed to parse would make the thread unreadable: `thread list`,
  the brief, the ticker and its own restart would all stop seeing it.
- `Kind::parse` still takes `tab` and `home`, maps them to `Local` and says so
  on stderr. Deprecating rather than rejecting is deliberate: a person's habit
  is not worth a refusal, and nothing about the placement differs.
- Every match that said `Kind::Tab | Kind::Home` says `Kind::Local`
  (`place_and_brief`, `place_tab`, `restart_plan`, `threads::clean`, `sweep`,
  `overview`'s place column). `spaces::empty`'s legacy own-Space rule and
  `thread::own_space_id` are gone with the fields that fed them (below).
- `hp thread start --help` says where each kind works and names no Space:
  `worktree` is its own worktree and branch, `checkout` the repository's main
  checkout, `local` a folder of its own under the project's `threads/<id>/`.

Deprecation, not removal, is what the two aliases and the two `parse` arms are
for. They can go in a later release, when no record in any project says `tab` or
`home`; nothing else depends on them.

**Deleted as inert (not ported, not replaced):**

- `spaces::primary` — herdr 1's "repository Space", the Space herdr grouped a
  repository's worktree Spaces under. It looked for a Space whose
  `worktree.is_linked_worktree` was false, and nothing in the herdr-core port
  ever sets it (herdr-core has no worktree grouping), so it never found one.
- `spaces::record`, `Thread::repo_workspace` and the repository-Space rule in
  `spaces::empty` — they existed only to remember and later close that Space.
- `thread::own_space_id`, `Thread::own_workspace` and the legacy own-Space rule
  in `spaces::empty` — they closed a thread's *own* pre-t-0062 Space once its
  panes were gone. Nothing has written `own_workspace` since t-0062, and a Space
  that still holds a thread's panes is closed with its last tab at resolve
  (`close_place`); the rule had nothing left to find.
- `WorkspaceWorktree` — its only readers were the three above; what is left of
  it is `Workspace::worktree: Option<String>`, "this Space *is* this worktree".

**Still true, and not a loose end:** `threads::clean` closes the Space when the
resolved tab was the last pane. If an app refuses that close (`space.close`), the
note says so and the next `spaces::empty` pass (idle shells only) retries it. Not
observed; worth a look on the first real resolve.

## 10. Migration

Nothing is migrated, and nothing needs to be:

- **restart** places the tab in the thread Space (the old Space is not joined:
  §4);
- **resolve** closes the tab, and `close_place` closes the Space when that tab
  was its last pane;
- **nothing** reopens a `threads/*.toml`: a record's `kind` is read, never
  rewritten, so a `.toml` saying `kind = "tab"` or `"home"` keeps saying it and
  keeps working (it reads as `local`; the record is written back as `local` the
  next time anything updates it).

Leaving them alone is cheaper and safer than moving tabs under a person: a
restart is a thing the user asks for, a rewrite of every record in ten projects
is not. A legacy Space — one Space per thread, named after its title — stays
until that thread is resolved: its tab is not joined again (§4), and it goes with
its last pane.

## 11. Where this differs from the first draft of the change

The first draft of this work (t-0062, 2026-11-19) listed five places to touch and
three traps. Five of those six are right in substance and wrong in detail; the
corrections are what §6 is built on.

1. **The worktree path had two entry points, not one.** The draft pointed at
   `worktree_open` for restarts, but creation went through `worktree_create`,
   which also made the Space (`workspace_create` + `hp_worktree`). Both were
   replaced by `worktree_add` (git only) and `worktree_mark` (the label), and the
   tab is placed by the ordinary `place`.
2. **"Recycle the shared Space when no pane is left in it" cannot be a sweep.**
   `Core::workspace_list` derives Spaces from terminals, so a Space with no panes
   is not listed: the sweep cannot see it and `workspace_close` on it is a no-op.
   The close therefore happens in `close_place`, where the last pane is still
   known, and `spaces::empty` is the backstop for leftover shells (§5).
3. **A third trap: the Space's own classification.** `workspace_list` called a
   Space a worktree Space when *any* pane said so, which the shared Space
   inevitably does (§6.4).
4. **A fourth trap: `tab_create` on an app Space dropped the Space's name**
   (§6.5). Without it, the label lookup misses and every thread opens a Space.
5. **Two more places keyed on "the thread's Space":** `sweep`'s worktree pane
   lookup and its `Orphan::Workspace` (§6.1), and the "in use" set inside
   `spaces::empty`, which must count panes rather than records (§6.2).
6. **"When it cannot be found, refuse and tell the user to `open`" is wrong for
   the first thread.** A project with no thread Space has nothing to find, so
   refusing would block every project's first thread. The rule kept from that
   advice is the part that matters: find **before** creating, through five
   sources (§4), and never create because a label merely looked odd.
