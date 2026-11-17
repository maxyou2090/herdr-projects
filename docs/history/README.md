# History: the Herdr 1 write-ups

These documents were written while `herdr-projects` was a Herdr 1 plugin. They
describe a host that is gone — the sidebar, the `prefix+a` popup, plugin
actions, `pane report-metadata` tokens, `herdr machine list` and socket paths.
Nothing in `docs/history/` is a current instruction; it is kept because the
observations still explain *why* the port looks the way it does.

| File | What it holds |
| --- | --- |
| [`manual-test-herdr1.md`](manual-test-herdr1.md) | The Herdr 1 acceptance list, stage by stage, with how each check was witnessed (builder, unit test, or a person looking at the client). |
| [`herdr-notes-herdr1.md`](herdr-notes-herdr1.md) | What the builder verified about Herdr 0.9.x: sessions and sockets, dialogs, worktrees, the `update` flow, remote machines, and the 0.2.0 redesign. |

## Moved here from `docs/`

Two files stayed in `docs/` as signposts after the port: each said in a
paragraph what it had been and linked to its archive. That paragraph is now
here and the files are gone (2026-11-17).

- `docs/manual-test.md` → [`manual-test-herdr1.md`](manual-test-herdr1.md): the
  acceptance script of the Herdr 1 build (sidebar, popup, action menu, tab bar,
  `herdr machine`), kept as a record because none of that exists on herdr-core.
- `docs/herdr-notes.md` → [`herdr-notes-herdr1.md`](herdr-notes-herdr1.md): what
  the builder verified about Herdr 0.9.x — sessions and sockets, `pane
  report-metadata`, popup actions, `herdr machine`, the 0.2.0 redesign. For what
  the port verified instead, see [`../herdr-core-port.md`](../herdr-core-port.md).

Current documents: [`../herdr-core-port.md`](../herdr-core-port.md),
[`../operations.md`](../operations.md), [`../lanes.md`](../lanes.md),
[`../getting-started.md`](../getting-started.md).
