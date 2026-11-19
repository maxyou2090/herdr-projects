//! Repository Spaces left behind by worktree threads. `herdr worktree create`
//! groups every worktree Space under the repository's primary Space, making
//! one (a plain shell in the main checkout) when none is open. Once no open
//! thread uses it, nothing runs in it and nobody looks at it, it is closed.

use std::path::{Path, PathBuf};

use crate::core::{Core, Workspace};
use crate::paths::Ctx;
use crate::project::{self, Project};
use crate::thread::{self, Kind, Status, Thread};

/// A recorded repository Space that nothing uses any more.
#[derive(Debug, Clone, PartialEq)]
pub struct Space {
    pub id: String,
    pub label: String,
}

fn canonical(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

/// The primary Space of `repo`: herdr's unlinked worktree grouping at its
/// main checkout.
pub fn primary<'w>(workspaces: &'w [Workspace], repo: &str) -> Option<&'w Workspace> {
    let repo = canonical(repo);
    workspaces.iter().find(|w| w.worktree.as_ref().is_some_and(|t| !t.is_linked_worktree && canonical(&t.checkout_path) == repo))
}

fn local_worktree(t: &Thread) -> bool {
    t.kind == Kind::Worktree && !t.repo.is_empty()
}

/// Records the primary Space on open local worktree threads that have none
/// yet: threads placed before this was recorded, or placed while herdr was
/// slow to list it.
pub fn record(project: &Project, herdr: &Core) {
    let missing: Vec<Thread> = thread::list(project).into_iter().filter(|t| t.status != Status::Resolved && local_worktree(t) && t.repo_workspace.is_empty()).collect();
    if missing.is_empty() {
        return;
    }
    let Ok(workspaces) = herdr.workspace_list() else {
        return;
    };
    for t in missing {
        if let Some(space) = primary(&workspaces, &t.repo) {
            let id = space.workspace_id.clone();
            let _ = thread::update(project, &t.id, |t| t.repo_workspace = id);
        }
    }
}

/// Spaces safe to close: those `project`'s worktree threads recorded — herdr
/// must still show them as the repository's primary Space with no worktree
/// Space under it — plus orphan home Spaces, plain Spaces still carrying the
/// project's home label that no open thread and no coordinator uses, plus a
/// thread's own plain Space (recorded and titled after it; tab and checkout
/// threads have one). None of them may hold an agent, and every pane must be
/// an idle shell. A focused Space or pane is kept unless `include_focused`.
pub fn empty(ctx: &Ctx, project: &Project, herdr: &Core, include_focused: bool) -> Vec<Space> {
    let threads = thread::list(project);
    let mut recorded: Vec<(String, String)> = threads.iter().filter(|t| local_worktree(t) && !t.repo_workspace.is_empty()).map(|t| (t.repo_workspace.clone(), t.repo.clone())).collect();
    // Threads' own Spaces: a plain Space named after the thread, which only
    // its own tabs should work in. A checkout thread works in the repo's main
    // checkout, a tab thread in `threads/<id>/`.
    let mut own: Vec<(String, String, std::path::PathBuf)> = threads
        .iter()
        .filter(|t| matches!(t.kind, Kind::Tab | Kind::Checkout) && !thread::own_space_id(t).is_empty())
        .map(|t| {
            let folder = if t.kind == Kind::Checkout { std::path::PathBuf::from(&t.repo) } else { project.dir().join("threads").join(&t.id) };
            (thread::own_space_id(t).to_string(), t.title.clone(), folder)
        })
        .collect();
    let mut in_use = Vec::new();
    // The panes of open threads, across every project: the project's thread
    // Space is shared by all of them, so a Space is "in use" when one of **their
    // panes** is in it, not when a record merely names it (`docs/thread-space.md`).
    let mut open_panes: Vec<String> = Vec::new();
    let mut coordinators = Vec::new();
    for slug in project::list_slugs(&ctx.root) {
        let Ok(other) = Project::load(&ctx.root, &slug) else {
            continue;
        };
        for t in thread::list(&other).into_iter().filter(|t| t.status != Status::Resolved) {
            if !t.repo_workspace.is_empty() {
                in_use.push(t.repo_workspace);
            }
            if !t.workspace_id.is_empty() {
                in_use.push(t.workspace_id);
            }
            if !t.pane_id.is_empty() {
                open_panes.push(t.pane_id);
            }
        }
        // Every open lane's Space, not just the default lane's (§9): a lane
        // whose coordinator still lives in a home Space keeps it from the
        // orphan sweep. A closed lane protects nothing (§6.4).
        coordinators.extend(crate::lanes::open_spaces(&other));
    }
    recorded.retain(|(id, _)| !in_use.contains(id) && !coordinators.contains(id));
    own.retain(|(id, _, _)| !in_use.contains(id) && !coordinators.contains(id));
    recorded.sort();
    recorded.dedup_by(|a, b| a.0 == b.0);
    own.sort();
    own.dedup_by(|a, b| a.0 == b.0);
    // A twin, or a thread Space, can only exist where a thread was placed
    // outside the home: without one, the cheap early return spares every
    // project's tick three lists it cannot need.
    let home_id = project.coordinator().map(|c| c.workspace_id).unwrap_or_default();
    let twins = threads.iter().any(|t| !t.workspace_id.is_empty() && t.workspace_id != home_id);
    if recorded.is_empty() && own.is_empty() && !twins {
        return Vec::new();
    }
    let (Ok(workspaces), Ok(panes), Ok(agents)) = (herdr.workspace_list(), herdr.pane_list(), herdr.agent_list()) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for (id, repo) in recorded {
        // The id must still name this repository's primary Space: ids are
        // reused after a server restart.
        let Some(space) = primary(&workspaces, &repo).filter(|w| w.workspace_id == id) else {
            continue;
        };
        let key = space.worktree.as_ref().map(|t| t.repo_key.as_str()).unwrap_or_default();
        let has_children = workspaces.iter().any(|w| w.worktree.as_ref().is_some_and(|t| t.is_linked_worktree && t.repo_key == key));
        let own: Vec<_> = panes.iter().filter(|p| p.workspace_id == id).collect();
        if has_children
            || own.is_empty()
            || own.len() != space.pane_count
            || agents.iter().any(|a| a.workspace_id == id)
            || (!include_focused && (space.focused || own.iter().any(|p| p.focused)))
            || !own.iter().all(|p| herdr.pane_idle_shell(&p.pane_id))
        {
            continue;
        }
        found.push(Space { id, label: space.label.clone() });
    }
    // Orphan home Spaces: plain Spaces still carrying the project's home
    // label that no open thread and no coordinator uses — the twins left
    // behind when a tab thread reopened the project's workspace next to its
    // old one. Worktree-backed Spaces belong to the primary logic above.
    if let Ok((settings, _)) = project.read_project_md() {
        let home = project::home_label(&settings.name, &project.slug);
        for w in workspaces.iter().filter(|w| w.worktree.is_none() && w.label == home && !in_use.contains(&w.workspace_id) && !coordinators.contains(&w.workspace_id)) {
            let own: Vec<_> = panes.iter().filter(|p| p.workspace_id == w.workspace_id).collect();
            if own.is_empty()
                || own.len() != w.pane_count
                || agents.iter().any(|a| a.workspace_id == w.workspace_id)
                || (!include_focused && (w.focused || own.iter().any(|p| p.focused)))
                || !own.iter().all(|p| herdr.pane_idle_shell(&p.pane_id))
            {
                continue;
            }
            found.push(Space { id: w.workspace_id.clone(), label: w.label.clone() });
        }
    }
    // Threads' own Spaces, once nothing uses them: a plain Space still
    // carrying the thread's title (herdr reuses ids after a server restart,
    // so the label and any panes are checked too). Their last tab is closed
    // at resolve, which leaves the Space itself empty.
    for (id, label, folder) in own {
        let Some(space) = workspaces.iter().find(|w| w.workspace_id == id) else {
            continue;
        };
        if space.worktree.is_some() || label.is_empty() || !crate::labels::thread_space_matches(&space.label, &label) {
            continue;
        }
        let own_panes: Vec<_> = panes.iter().filter(|p| p.workspace_id == id).collect();
        let in_folder = own_panes.iter().any(|p| !p.cwd.is_empty() && Path::new(&p.cwd).starts_with(&folder));
        if own_panes.len() != space.pane_count
            || agents.iter().any(|a| a.workspace_id == id)
            || (!include_focused && (space.focused || own_panes.iter().any(|p| p.focused)))
            || !own_panes.iter().all(|p| herdr.pane_idle_shell(&p.pane_id))
            || (!own_panes.is_empty() && !in_folder)
        {
            continue;
        }
        found.push(Space { id, label: space.label.clone() });
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
            && !own_panes.iter().any(|p| open_panes.contains(&p.pane_id))
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

/// Forgets a Space on every thread of `project` that recorded it. A tab or
/// checkout thread's `workspace_id` names the same Space as its
/// `own_workspace`, so a legacy tab record whose `own_workspace` was never
/// written is forgotten too. (A legacy checkout record's `workspace_id` names
/// the project's home instead, which is never closed here while the thread is
/// open: `empty` keeps every Space an open thread still uses.)
fn forget(project: &Project, id: &str) {
    for t in thread::list(project).into_iter().filter(|t| t.repo_workspace == id || t.own_workspace == id || t.workspace_id == id) {
        let repo = t.repo_workspace == id;
        let own = t.own_workspace == id;
        // The Space is gone, so a record that says its tab is there is stale —
        // for every kind now that one Space holds them all: the next placement
        // makes a new one instead of writing into an id nobody lists.
        let tab = t.workspace_id == id && t.status != Status::Resolved;
        let _ = thread::update(project, &t.id, |t| {
            if repo {
                t.repo_workspace.clear();
            }
            if own {
                t.own_workspace.clear();
            }
            if tab && t.workspace_id == id {
                t.workspace_id.clear();
            }
        });
    }
}

/// Closes `project`'s empty repository Spaces, one inbox item each. A Space
/// kept because something is busy or focused is retried on a later tick,
/// silently. Records of Spaces herdr no longer shows are dropped.
pub fn close_empty(ctx: &Ctx, project: &Project, herdr: &Core) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    for space in empty(ctx, project, herdr, false) {
        match close(herdr, &space) {
            Ok(()) => {
                forget(project, &space.id);
                errors.extend(crate::inbox::write(project, "space", &space.id, "closed empty Space", &format!("closed empty Space {} ({})", space.label, space.id), "").err());
            }
            Err(error) if error.code == "workspace_group_close_required" => {}
            Err(error) => errors.push(anyhow::anyhow!("could not close empty Space {}: {error}", space.id)),
        }
    }
    let left: Vec<Thread> = thread::list(project).into_iter().filter(|t| t.status == Status::Resolved && !t.repo_workspace.is_empty()).collect();
    if !left.is_empty()
        && let Ok(workspaces) = herdr.workspace_list()
    {
        for t in left.iter().filter(|t| !workspaces.iter().any(|w| w.workspace_id == t.repo_workspace)) {
            forget(project, &t.repo_workspace);
        }
    }
    errors
}

/// Renames the project's thread Space to `<display name> Threads` when the
/// display name changed (`hp set <project> name …`, or a `rename`). Silent, like
/// [`sync_home_label`]: the ticker runs it every tick, so it reads one Space
/// list and does nothing else unless the label is wrong.
///
/// It finds the Space the way placement does (`threads::threads_space`), with the
/// panes left out: a Space renaming is owed to is named by a thread's record,
/// which is cheaper and truer than a pane (`docs/thread-space.md` §4). A Space a
/// record names that carries no label at all is the one case that needs the
/// second look — hp's label never landed on it (§6.5).
pub fn sync_threads_label(project: &Project, herdr: &Core) {
    let label = crate::labels::threads_space_label(&project.display_name());
    let Ok(workspaces) = herdr.workspace_list() else {
        return;
    };
    let homes = crate::coordinator::coordinator_space_ids(project);
    let named = |w: &crate::core::Workspace| crate::labels::is_threads_space_label(&w.label) || w.label.is_empty();
    let found = crate::threads::threads_space(project, &workspaces, &[], &[], None)
        .or_else(|| {
            thread::list(project)
                .into_iter()
                .filter(|t| !t.workspace_id.is_empty() && !t.title.is_empty())
                .find_map(|t| workspaces.iter().find(|w| w.workspace_id == t.workspace_id && named(w)).cloned())
                .map(|w| w.workspace_id)
        })
        .filter(|id| !homes.contains(id));
    let Some(id) = found else {
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
