//! Empty Spaces left behind. A project's two Spaces are its own (holding the
//! `coordinator` tab) and its threads' (`<display name> Threads`, holding one
//! tab per thread): every kind of thread lands in the second one, so nothing
//! makes a Space per thread or per worktree any more (`docs/thread-space.md`).
//! What can still be found empty is an orphan home Space — the twin a thread
//! made by reopening the project next to its old one — and the thread Space
//! itself once its last thread and shell are gone. Both are closed here, once
//! nothing runs in them and nobody looks at them.

use crate::core::{Core, L_TAB_LABEL, Pane};
use crate::paths::Ctx;
use crate::project::{self, Project};
use crate::thread::{self, Status};

/// An empty Space that nothing uses any more.
#[derive(Debug, Clone, PartialEq)]
pub struct Space {
    pub id: String,
    pub label: String,
}

/// Spaces safe to close: orphan home Spaces (plain Spaces still carrying the
/// project's home label that no open thread and no coordinator uses) and the
/// project's thread Space once nothing is left in it. None of them may hold an
/// agent, and every pane must be an idle shell. A focused Space or pane is kept
/// unless `include_focused`.
///
/// A Space is "in use" when a live thread's **pane** is in it, and a pane says
/// that two ways: a record names it (`pane_id`, `tab_id`), or the pane names
/// itself (`hp_tab_label` is that thread's tab). Both are needed — the record is
/// written after `tab_create`, and until it is, the pane is all there is
/// (`docs/thread-space.md` §5).
///
/// Takes no lock: [`close_empty`] takes it around this call and the close, and
/// `hp sweep` reads without one.
pub fn empty(ctx: &Ctx, project: &Project, herdr: &Core, include_focused: bool) -> Vec<Space> {
    // A twin, or a thread Space, can only exist where a thread was placed
    // outside the home: without one, the cheap early return spares every
    // project's tick three lists it cannot need.
    let home_id = project.coordinator().map(|c| c.workspace_id).unwrap_or_default();
    let recorded = thread::list(project);
    let twins = recorded.iter().any(|t| !t.workspace_id.is_empty() && t.workspace_id != home_id);
    if !twins {
        return Vec::new();
    }
    let (Ok(workspaces), Ok(panes), Ok(agents)) = (herdr.workspace_list(), herdr.pane_list(), herdr.agent_list()) else {
        return Vec::new();
    };
    // The records are read **after** the three listings, so a placement that
    // finished while they ran is seen rather than missed. The lock is what makes
    // the whole step one snapshot; this ordering keeps a caller that has none
    // (`hp sweep`, which only lists) from judging with records older than its
    // panes.
    let mut in_use = Vec::new();
    // The panes of open threads, across every project: the project's thread
    // Space is shared by all of them, so a Space is "in use" when one of **their
    // panes** is in it, not when a record merely names it (`docs/thread-space.md`).
    let mut open_panes: Vec<String> = Vec::new();
    let mut open_tabs: Vec<String> = Vec::new();
    let mut open_tab_labels: Vec<String> = Vec::new();
    let mut coordinators = Vec::new();
    for slug in project::list_slugs(&ctx.root) {
        let Ok(other) = Project::load(&ctx.root, &slug) else {
            continue;
        };
        for t in thread::list(&other).into_iter().filter(|t| t.status != Status::Resolved) {
            if !t.workspace_id.is_empty() {
                in_use.push(t.workspace_id);
            }
            if !t.pane_id.is_empty() {
                open_panes.push(t.pane_id);
            }
            if !t.tab_id.is_empty() {
                open_tabs.push(t.tab_id);
            }
            open_tab_labels.push(crate::labels::thread_tab_label(&t.id));
        }
        // Every open lane's Space, not just the default lane's (§9): a lane
        // whose coordinator still lives in a home Space keeps it from the
        // orphan sweep. A closed lane protects nothing (§6.4).
        coordinators.extend(crate::lanes::open_spaces(&other));
    }
    // A pane a live thread runs in, however that is known. The second half is
    // what covers the window between `tab_create` and the record write: such a
    // pane used to look exactly like a shell somebody left behind, which is how
    // a Space was closed under a thread that had just been placed (2026-11-20).
    let live_thread_pane = |p: &Pane| {
        open_panes.contains(&p.pane_id)
            || (!p.tab_id.is_empty() && open_tabs.contains(&p.tab_id))
            || p.tokens.get(L_TAB_LABEL).is_some_and(|label| open_tab_labels.contains(label))
    };
    let mut found = Vec::new();
    // Orphan home Spaces: plain Spaces still carrying the project's home
    // label that no open thread and no coordinator uses — the twins left
    // behind when a thread reopened the project's workspace next to its old
    // one. A Space that is one worktree is not one of these: it is a thread's
    // own Space from before t-0062, closed with its last tab at resolve.
    if let Ok((settings, _)) = project.read_project_md() {
        let home = project::home_label(&settings.name, &project.slug);
        for w in workspaces.iter().filter(|w| w.worktree.is_none() && w.label == home && !in_use.contains(&w.workspace_id) && !coordinators.contains(&w.workspace_id)) {
            let own: Vec<_> = panes.iter().filter(|p| p.workspace_id == w.workspace_id).collect();
            if own.is_empty()
                || own.len() != w.pane_count
                || own.iter().any(|p| live_thread_pane(p))
                || agents.iter().any(|a| a.workspace_id == w.workspace_id)
                || (!include_focused && (w.focused || own.iter().any(|p| p.focused)))
                || !own.iter().all(|p| herdr.pane_idle_shell(&p.pane_id))
            {
                continue;
            }
            found.push(Space { id: w.workspace_id.clone(), label: w.label.clone() });
        }
    }
    // The project's thread Space (`docs/thread-space.md`): shared by every
    // thread, so it is never "in use" for one of them and is closed only when
    // nothing is left in it. A Space that still holds a pane of an **open**
    // thread is that thread's: it stays, whatever the Space is called.
    if let Some(id) = crate::threads::threads_space(project, &workspaces, &panes, &agents, None)
        && let Some(space) = workspaces.iter().find(|w| w.workspace_id == id)
    {
        let own_panes: Vec<_> = panes.iter().filter(|p| p.workspace_id == id).collect();
        if !coordinators.contains(&id)
            && own_panes.len() == space.pane_count
            && !own_panes.iter().any(|p| live_thread_pane(p))
            && !agents.iter().any(|a| a.workspace_id == id)
            && (include_focused || (!space.focused && !own_panes.iter().any(|p| p.focused)))
            && own_panes.iter().all(|p| herdr.pane_idle_shell(&p.pane_id))
        {
            found.push(Space { id: id.clone(), label: space.label.clone() });
        }
    }
    found
}

/// Closes a Space. Never `--group`: herdr refuses a Space with worktree
/// Spaces under it.
pub fn close(herdr: &Core, space: &Space) -> Result<(), crate::core::CoreError> {
    herdr.workspace_close(&space.id)
}

/// Forgets a Space on every thread of `project` that names it. The Space is
/// gone, so a record that says its tab is there is stale — every kind now
/// shares one Space, so the next placement makes a new one instead of writing
/// into an id nobody lists. The caller holds the project lock.
fn forget(project: &Project, id: &str) {
    for t in thread::list(project).into_iter().filter(|t| t.workspace_id == id && t.status != Status::Resolved) {
        let _ = thread::update_locked(project, &t.id, |t| t.workspace_id.clear());
    }
}

/// Closes `project`'s empty Spaces, one inbox item each. A Space kept because
/// something is busy or focused is retried on a later tick, silently.
///
/// The listings, the judgement and the closes are one step, under the project
/// lock: a `thread start` holds the same lock from its Space lookup to the
/// record that names its pane, so a tick cannot judge a Space on a snapshot
/// older than a placement and then close it under the new tab (2026-11-20,
/// `docs/thread-space.md` §5). The lock is the cost of that: three `hc` listings
/// (about 0.6s here) are held while a placement waits.
pub fn close_empty(ctx: &Ctx, project: &Project, herdr: &Core) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    let mut closed: Vec<Space> = Vec::new();
    {
        let _lock = match project.lock() {
            Ok(lock) => lock,
            Err(error) => {
                errors.push(error);
                return errors;
            }
        };
        for space in empty(ctx, project, herdr, false) {
            match close(herdr, &space) {
                Ok(()) => {
                    forget(project, &space.id);
                    closed.push(space);
                }
                Err(error) if error.code == "workspace_group_close_required" => {}
                Err(error) => errors.push(anyhow::anyhow!("could not close empty Space {}: {error}", space.id)),
            }
        }
    }
    // The items go out after the lock is released: `inbox::write` takes it too.
    for space in closed {
        errors.extend(crate::inbox::write(project, "space", &space.id, "closed empty Space", &format!("closed empty Space {} ({})", space.label, space.id), "").err());
    }
    errors
}

/// Closes the Space at `id` when it is still empty **now**, under the project
/// lock. `hp sweep` lists its orphans once and removes them after a
/// confirmation, so its listing can be minutes old by the time one is removed;
/// this is the same judgement the tick makes, made again at the close. Says
/// whether it closed it. No inbox item: `sweep` prints its own line.
pub fn close_one(ctx: &Ctx, project: &Project, herdr: &Core, id: &str, include_focused: bool) -> anyhow::Result<bool> {
    let _lock = project.lock()?;
    let Some(space) = empty(ctx, project, herdr, include_focused).into_iter().find(|space| space.id == id) else {
        return Ok(false);
    };
    close(herdr, &space).map_err(|error| anyhow::anyhow!("{error}"))?;
    forget(project, &space.id);
    Ok(true)
}

/// Renames the project's thread Space to `<display name> Threads` when the
/// display name changed (`hp set <project> name …`, or a `rename`). Silent, like
/// [`sync_home_label`]: the ticker runs it every tick, so it reads one Space
/// list and does nothing else unless the label is wrong.
///
/// It finds the Space the way placement does — `threads::threads_space`, the one
/// judgement — with the panes left out: this runs on a ticker tick and cannot
/// afford a pane listing (`docs/thread-space.md` §4.2). So it can be answered by
/// two sources only, "a record of this project names that Space" (which covers a
/// Space with no name yet, and one named under a display name that has since
/// changed) and "the Space carries today's name, exactly". It cannot answer with
/// another project's `<other display name> Threads`: a name ending in ` Threads`
/// alone is not this project's, and there is deliberately no second, laxer look
/// after this one — that second look is how ten projects' ticks took turns
/// renaming one Space (2026-11-20).
pub fn sync_threads_label(project: &Project, herdr: &Core) {
    let label = crate::labels::threads_space_label(&project.display_name());
    let Ok(workspaces) = herdr.workspace_list() else {
        return;
    };
    let homes = crate::coordinator::coordinator_space_ids(project);
    let Some(id) = crate::threads::threads_space(project, &workspaces, &[], &[], None).filter(|id| !homes.contains(id)) else {
        return;
    };
    if herdr.workspace_label(&id).unwrap_or_default() != label {
        let _ = herdr.name_space(&id, &label);
    }
}

/// Renames the project's home Space to the label its `PROJECT.md` name spells:
/// a name changed outside `herdr-projects rename` would otherwise leave the
/// old label in the sidebar until the next `open`. Silent, so a ticker tick
/// never prints.
pub fn sync_home_label(project: &Project, herdr: &Core) {
    let Some(record) = project.coordinator() else {
        return;
    };
    if record.workspace_id.is_empty() {
        return;
    }
    let Ok((settings, _)) = project.read_project_md() else {
        return;
    };
    let label = project::home_label(&settings.name, &project.slug);
    if herdr.workspace_label(&record.workspace_id).unwrap_or_default() != label {
        let _ = herdr.name_space(&record.workspace_id, &label);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::L_WS_LABEL;
    use crate::fake_hc::{FakeApp, FakeCore};

    /// The home Space is named by the project's display name (`REXA`), and on a
    /// Space the Herdr app draws the rename goes through the app — PR #41's host
    /// rule — as well as through hp's label, which is the record.
    #[test]
    fn a_drifted_home_space_is_renamed_to_the_display_name_on_the_app_that_draws_it() {
        let root = tempfile::tempdir().unwrap();
        let project = crate::project::create(root.path(), "REXA", vec![]).unwrap();
        let dir = project.canonical_dir().to_string_lossy().into_owned();
        let core = FakeCore::new();
        let app = FakeApp::new(&core);
        let space = app.shown_space("rexa");
        core.terminal("hpc-rexa", &dir, &[("hp_ws", space.as_str()), (L_WS_LABEL, "rexa")]);
        project
            .update_coordinator(|c| {
                c.socket = "fake-session".into();
                c.workspace_id = space.clone();
                c.cwd = dir.clone();
            })
            .unwrap();

        let herdr = Core::new("hc", "", &app);
        sync_home_label(&project, &herdr);

        assert_eq!(app.spaces.borrow()[0].1, "REXA", "the app draws the display name");
        let labels = core.terminals.borrow()[0].labels.clone();
        assert_eq!(labels.get(L_WS_LABEL).map(String::as_str), Some("REXA"), "hp's label says the same, so one name");
        assert_eq!(project.display_name(), "REXA", "not the slug");
    }

    /// A Space already showing the display name is left alone: the ticker runs
    /// this every tick.
    #[test]
    fn a_home_space_already_named_right_is_not_renamed() {
        let root = tempfile::tempdir().unwrap();
        let project = crate::project::create(root.path(), "herdr-projects", vec![]).unwrap();
        let dir = project.canonical_dir().to_string_lossy().into_owned();
        let core = FakeCore::new();
        let app = FakeApp::new(&core);
        let space = app.shown_space("Herdr Projects");
        core.terminal("hpc-herdr-projects", &dir, &[("hp_ws", space.as_str()), (L_WS_LABEL, "Herdr Projects")]);
        project
            .update_coordinator(|c| {
                c.socket = "fake-session".into();
                c.workspace_id = space.clone();
                c.cwd = dir.clone();
            })
            .unwrap();

        let herdr = Core::new("hc", "", &app);
        sync_home_label(&project, &herdr);

        let calls = app.calls.borrow().clone();
        assert!(!calls.iter().any(|c| c.contains("space.rename")), "nothing to rename: {calls:?}");
    }
}
