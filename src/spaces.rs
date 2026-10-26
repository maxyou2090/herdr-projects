//! Repository Spaces left behind by worktree threads. `herdr worktree create`
//! groups every worktree Space under the repository's primary Space, making
//! one (a plain shell in the main checkout) when none is open. Once no open
//! thread uses it, nothing runs in it and nobody looks at it, it is closed.

use std::path::{Path, PathBuf};

use crate::herdr::{Herdr, Workspace};
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
    t.kind == Kind::Worktree && !t.is_remote() && !t.repo.is_empty()
}

/// Records the primary Space on open local worktree threads that have none
/// yet: threads placed before this was recorded, or placed while herdr was
/// slow to list it.
pub fn record(project: &Project, herdr: &Herdr) {
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
/// tab thread's own plain Space (recorded and titled after it). None of
/// them may hold an agent, and every pane must be an idle shell. A focused
/// Space or pane is kept unless `include_focused`.
pub fn empty(ctx: &Ctx, project: &Project, herdr: &Herdr, include_focused: bool) -> Vec<Space> {
    let threads = thread::list(project);
    let mut recorded: Vec<(String, String)> = threads.iter().filter(|t| local_worktree(t) && !t.repo_workspace.is_empty()).map(|t| (t.repo_workspace.clone(), t.repo.clone())).collect();
    // Tab threads' own Spaces: a plain Space named after the thread, which
    // only its own tabs should work in.
    let mut own: Vec<(String, String, String)> = threads
        .iter()
        .filter(|t| t.kind == Kind::Tab && !thread::own_space_id(t).is_empty())
        .map(|t| (thread::own_space_id(t).to_string(), t.title.clone(), t.id.clone()))
        .collect();
    let mut in_use = Vec::new();
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
    // A twin can only exist where a thread was placed outside the home:
    // without one, the cheap early return spares every project's tick three
    // lists it cannot need.
    let home_id = project.coordinator().map(|c| c.workspace_id).unwrap_or_default();
    let twins = threads.iter().any(|t| t.kind != Kind::Worktree && !t.workspace_id.is_empty() && t.workspace_id != home_id);
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
    // Tab threads' own Spaces, once nothing uses them: a plain Space still
    // carrying the thread's title (herdr reuses ids after a server restart,
    // so the label and any panes are checked too). Their last tab is closed
    // at resolve, which leaves the Space itself empty.
    for (id, label, thread_id) in own {
        let Some(space) = workspaces.iter().find(|w| w.workspace_id == id) else {
            continue;
        };
        if space.worktree.is_some() || label.is_empty() || space.label != label || space.label.ends_with(crate::grouping::HOME_MARK) {
            continue;
        }
        let own_panes: Vec<_> = panes.iter().filter(|p| p.workspace_id == id).collect();
        let folder = project.dir().join("threads").join(&thread_id);
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
    found
}

/// Closes a Space. Never `--group`: herdr refuses a Space with worktree
/// Spaces under it.
pub fn close(herdr: &Herdr, space: &Space) -> Result<(), crate::herdr::HerdrError> {
    herdr.call(&["workspace", "close", &space.id], crate::herdr::CALL_TIMEOUT).map(|_| ())
}

/// Forgets a Space on every thread of `project` that recorded it. A tab
/// thread's `workspace_id` names the same Space as its `own_workspace`, so
/// a legacy record whose `own_workspace` was never written is forgotten too.
fn forget(project: &Project, id: &str) {
    for t in thread::list(project).into_iter().filter(|t| t.repo_workspace == id || t.own_workspace == id || (t.kind == Kind::Tab && t.workspace_id == id)) {
        let repo = t.repo_workspace == id;
        let own = t.own_workspace == id;
        let tab = t.kind == Kind::Tab;
        let _ = thread::update(project, &t.id, |t| {
            if repo {
                t.repo_workspace.clear();
            }
            if own {
                t.own_workspace.clear();
            }
            // A legacy tab record has no `own_workspace`; its `workspace_id`
            // names the same Space. Only the closed one is cleared: the tab
            // may have moved elsewhere since.
            if (own || tab) && t.workspace_id == id {
                t.workspace_id.clear();
            }
        });
    }
}

/// Closes `project`'s empty repository Spaces, one inbox item each. A Space
/// kept because something is busy or focused is retried on a later tick,
/// silently. Records of Spaces herdr no longer shows are dropped.
pub fn close_empty(ctx: &Ctx, project: &Project, herdr: &Herdr) -> Vec<anyhow::Error> {
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

/// Renames the project's home Space to the label its `PROJECT.md` name spells:
/// a name changed outside `herdr-projects rename` would otherwise leave the
/// old label in the sidebar until the next `open`. Silent, so a ticker tick
/// never prints.
pub fn sync_home_label(project: &Project, herdr: &Herdr) {
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
    if let Ok(current) = herdr.workspace_label(&record.workspace_id)
        && current != label
    {
        let _ = herdr.workspace_rename(&record.workspace_id, &label);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::runner::fake::ok;
    use crate::scenarios::{World, agent_json, pane_json};

    struct Fixture {
        world: World,
        project: Project,
        repo: String,
        /// `workspace list` entries, changeable mid-test.
        workspaces: Rc<RefCell<Vec<String>>>,
        /// Panes whose foreground process is not the shell.
        busy: Rc<RefCell<Vec<String>>>,
    }

    fn primary_json(id: &str, repo: &str, focused: bool) -> String {
        format!(r#"{{"workspace_id":"{id}","label":"repo","focused":{focused},"pane_count":1,"worktree":{{"repo_key":"{repo}/.git","checkout_path":"{repo}","is_linked_worktree":false}}}}"#)
    }

    fn child_json(id: &str, repo: &str) -> String {
        format!(r#"{{"workspace_id":"{id}","label":"x","pane_count":1,"worktree":{{"repo_key":"{repo}/.git","checkout_path":"/wt/{id}","is_linked_worktree":true}}}}"#)
    }

    /// A plain Space carrying the project's home label: a twin left behind by
    /// a tab thread that reopened the workspace next to its old one.
    fn home_json(f: &Fixture) -> String {
        let (settings, _) = f.project.read_project_md().unwrap();
        let label = crate::project::home_label(&settings.name, &f.project.slug);
        format!(r#"{{"workspace_id":"w8","label":"{label}","pane_count":1}}"#)
    }

    /// A resolved worktree thread that recorded `w9`, the repository's primary
    /// Space, now one idle shell in the main checkout.
    fn fixture() -> Fixture {
        let world = World::new();
        let project = world.project("demo", "a.sock");
        let repo_dir = world.home.path().join("repo");
        std::fs::create_dir(&repo_dir).unwrap();
        let repo = repo_dir.to_string_lossy().into_owned();
        let r = repo.clone();
        world.thread(&project, world.home.path(), |t| {
            t.repo = r;
            t.status = Status::Resolved;
            t.repo_workspace = "w9".into();
        });
        *world.panes.borrow_mut() = format!("[{},{}]", world.coordinator_pane(&project), pane_json("w9", "w9:t1", "w9:p1", &repo));
        let workspaces = Rc::new(RefCell::new(vec![r#"{"workspace_id":"w1","label":"Demo","pane_count":1}"#.to_string(), primary_json("w9", &repo, false)]));
        let list = workspaces.clone();
        world.runner.on_fn(|c| c.display().contains("workspace list"), move |_| Ok(ok(&format!(r#"{{"result":{{"workspaces":[{}]}}}}"#, list.borrow().join(",")))));
        let busy = Rc::new(RefCell::new(Vec::<String>::new()));
        let running = busy.clone();
        world.runner.on_fn(
            |c| c.display().contains("process-info"),
            move |c| {
                let pane = c.args.last().cloned().unwrap_or_default();
                let group = if running.borrow().contains(&pane) { 200 } else { 100 };
                Ok(ok(&format!(r#"{{"result":{{"process_info":{{"pane_id":"{pane}","shell_pid":100,"foreground_process_group_id":{group},"foreground_processes":[{{"pid":{group},"name":"zsh"}}]}}}}}}"#)))
            },
        );
        world.runner.on("workspace close", ok(r#"{"result":{}}"#));
        Fixture { world, project, repo, workspaces, busy }
    }

    fn close(f: &Fixture) -> usize {
        let ctx = f.world.ctx();
        let herdr = Herdr::new(f.world.env.herdr_bin(), f.project.coordinator().unwrap().socket, &f.world.runner);
        assert!(close_empty(&ctx, &f.project, &herdr).is_empty());
        f.world.runner.count("workspace close")
    }

    /// A project with one tab thread that recorded its own plain Space w8
    /// (empty in herdr: its last pane is gone) and no other threads.
    fn tab_fixture(status: Status, pane_count: usize, label: &str) -> Fixture {
        let world = World::new();
        let project = world.project("demo", "a.sock");
        world.thread(&project, world.home.path(), |t| {
            t.kind = Kind::Tab;
            t.status = status;
            t.title = "Research".into();
            t.repo.clear();
            t.worktree_path.clear();
            t.workspace_id = "w8".into();
            t.own_workspace = "w8".into();
        });
        *world.panes.borrow_mut() = format!("[{}]", world.coordinator_pane(&project));
        let workspaces = Rc::new(RefCell::new(vec![
            r#"{"workspace_id":"w1","label":"Demo","pane_count":1}"#.to_string(),
            format!(r#"{{"workspace_id":"w8","label":"{label}","pane_count":{pane_count}}}"#),
        ]));
        let list = workspaces.clone();
        world.runner.on_fn(|c| c.display().contains("workspace list"), move |_| Ok(ok(&format!(r#"{{"result":{{"workspaces":[{}]}}}}"#, list.borrow().join(",")))));
        let busy = Rc::new(RefCell::new(Vec::<String>::new()));
        let running = busy.clone();
        world.runner.on_fn(
            |c| c.display().contains("process-info"),
            move |c| {
                let pane = c.args.last().cloned().unwrap_or_default();
                let group = if running.borrow().contains(&pane) { 200 } else { 100 };
                Ok(ok(&format!(r#"{{"result":{{"process_info":{{"pane_id":"{pane}","shell_pid":100,"foreground_process_group_id":{group},"foreground_processes":[{{"pid":{group},"name":"zsh"}}]}}}}}}"#)))
            },
        );
        world.runner.on("workspace close", ok(r#"{"result":{}}"#));
        Fixture { world, project, repo: String::new(), workspaces, busy }
    }

    fn items(f: &Fixture) -> Vec<String> {
        crate::inbox::unhandled(&f.project).into_iter().map(|i| i.summary).collect()
    }

    #[test]
    fn an_empty_space_is_closed_after_the_last_thread_once() {
        let f = fixture();
        assert_eq!(close(&f), 1);
        let calls = f.world.runner.calls.borrow();
        let closed = calls.iter().find(|c| c.display().contains("workspace close")).unwrap();
        assert!(closed.args.ends_with(&["close".to_string(), "w9".to_string()]), "{}", closed.display());
        assert!(!closed.args.contains(&"--group".to_string()));
        drop(calls);
        assert_eq!(items(&f), ["closed empty Space repo (w9)"]);
        assert_eq!(thread::load(&f.project, "t-0001").unwrap().repo_workspace, "");
        // Forgotten: a later id reuse is never closed.
        assert_eq!(close(&f), 1);
    }

    #[test]
    fn a_busy_shell_or_an_agent_keeps_it_silently() {
        let f = fixture();
        f.busy.borrow_mut().push("w9:p1".into());
        assert_eq!(close(&f), 0);
        f.busy.borrow_mut().clear();
        *f.world.agents.borrow_mut() = format!("[{}]", agent_json("w9", "w9:t1", "w9:p1", &f.repo, "", "idle"));
        assert_eq!(close(&f), 0);
        assert!(items(&f).is_empty());
        assert_eq!(thread::load(&f.project, "t-0001").unwrap().repo_workspace, "w9");
        // Once idle again, a later tick closes it.
        *f.world.agents.borrow_mut() = "[]".into();
        assert_eq!(close(&f), 1);
    }

    #[test]
    fn a_second_pane_must_be_idle_too() {
        let f = fixture();
        f.workspaces.borrow_mut()[1] = primary_json("w9", &f.repo, false).replace(r#""pane_count":1"#, r#""pane_count":2"#);
        *f.world.panes.borrow_mut() = format!("[{},{},{}]", f.world.coordinator_pane(&f.project), pane_json("w9", "w9:t1", "w9:p1", &f.repo), pane_json("w9", "w9:t2", "w9:p2", "/tmp"));
        f.busy.borrow_mut().push("w9:p2".into());
        assert_eq!(close(&f), 0);
    }

    #[test]
    fn a_user_made_space_is_never_touched() {
        let f = fixture();
        // Never recorded: the thread has no repo Space.
        thread::update(&f.project, "t-0001", |t| t.repo_workspace.clear()).unwrap();
        assert_eq!(close(&f), 0);
        // Recorded, but herdr now shows that id as something else (a reused id).
        thread::update(&f.project, "t-0001", |t| t.repo_workspace = "w9".into()).unwrap();
        f.workspaces.borrow_mut()[1] = r#"{"workspace_id":"w9","label":"mine","pane_count":1}"#.into();
        assert_eq!(close(&f), 0);
    }

    #[test]
    fn kept_while_any_thread_still_uses_it() {
        let f = fixture();
        let other = f.world.project("other", "a.sock");
        let r = f.repo.clone();
        f.world.thread(&other, f.world.home.path(), |t| {
            t.repo = r;
            t.repo_workspace = "w9".into();
        });
        assert_eq!(close(&f), 0);
        // A worktree Space under it that no thread recorded keeps it too.
        thread::update(&other, "t-0001", |t| t.status = Status::Resolved).unwrap();
        thread::update(&other, "t-0001", |t| t.repo_workspace.clear()).unwrap();
        f.workspaces.borrow_mut().push(child_json("w5", &f.repo));
        assert_eq!(close(&f), 0);
        f.workspaces.borrow_mut().pop();
        assert_eq!(close(&f), 1);
    }

    #[test]
    fn kept_while_focused_but_sweep_lists_it() {
        let f = fixture();
        f.workspaces.borrow_mut()[1] = primary_json("w9", &f.repo, true);
        assert_eq!(close(&f), 0);
        let ctx = f.world.ctx();
        let herdr = Herdr::new(f.world.env.herdr_bin(), f.project.coordinator().unwrap().socket, &f.world.runner);
        assert_eq!(empty(&ctx, &f.project, &herdr, true), [Space { id: "w9".into(), label: "repo".into() }]);
        let swept: Vec<String> = crate::sweep::find(&ctx, &f.project).iter().map(|o| o.describe()).collect();
        assert!(swept.iter().any(|d| d.starts_with("empty Space repo (w9)")), "{swept:?}");
        f.workspaces.borrow_mut()[1] = primary_json("w9", &f.repo, false);
        assert_eq!(close(&f), 1);
    }

    #[test]
    fn the_coordinator_space_is_never_closed() {
        let f = fixture();
        thread::update(&f.project, "t-0001", |t| t.repo_workspace = "w1".into()).unwrap();
        f.workspaces.borrow_mut()[0] = primary_json("w1", &f.repo, false);
        f.workspaces.borrow_mut().pop();
        *f.world.panes.borrow_mut() = format!("[{}]", f.world.coordinator_pane(&f.project));
        assert_eq!(close(&f), 0);
    }

    #[test]
    fn open_threads_record_the_space_when_first_seen() {
        let f = fixture();
        thread::update(&f.project, "t-0001", |t| {
            t.status = Status::Open;
            t.repo_workspace.clear();
        })
        .unwrap();
        let herdr = Herdr::new(f.world.env.herdr_bin(), f.project.coordinator().unwrap().socket, &f.world.runner);
        record(&f.project, &herdr);
        assert_eq!(thread::load(&f.project, "t-0001").unwrap().repo_workspace, "w9");
    }

    #[test]
    fn an_orphan_home_space_is_closed_once_no_thread_uses_it() {
        let f = fixture();
        f.workspaces.borrow_mut().push(home_json(&f));
        *f.world.panes.borrow_mut() = format!(
            "[{},{},{}]",
            f.world.coordinator_pane(&f.project),
            pane_json("w9", "w9:t1", "w9:p1", &f.repo),
            pane_json("w8", "w8:t2", "w8:p2", "/tmp")
        );
        // While the thread is open it keeps both the primary and its twin.
        thread::update(&f.project, "t-0001", |t| {
            t.status = Status::Open;
            t.workspace_id = "w8".into();
        })
        .unwrap();
        assert_eq!(close(&f), 0);
        thread::update(&f.project, "t-0001", |t| t.status = Status::Resolved).unwrap();
        assert_eq!(close(&f), 2);
        let calls = f.world.runner.calls.borrow();
        assert!(calls.iter().any(|c| c.args.ends_with(&["close".to_string(), "w8".to_string()])));
    }

    #[test]
    fn an_open_home_thread_keeps_the_space_it_lives_in() {
        let f = fixture();
        f.workspaces.borrow_mut().push(home_json(&f));
        *f.world.panes.borrow_mut() = format!(
            "[{},{},{}]",
            f.world.coordinator_pane(&f.project),
            pane_json("w9", "w9:t1", "w9:p1", &f.repo),
            pane_json("w8", "w8:t2", "w8:p2", "/tmp")
        );
        // A home thread lives in w8: the cleanup must not close the Space
        // while its tab works there.
        thread::update(&f.project, "t-0001", |t| {
            t.kind = Kind::Home;
            t.status = Status::Open;
            t.workspace_id = "w8".into();
        })
        .unwrap();
        assert_eq!(close(&f), 0);
        thread::update(&f.project, "t-0001", |t| t.status = Status::Resolved).unwrap();
        assert_eq!(close(&f), 1);
    }

    #[test]
    fn an_orphan_home_space_with_an_agent_is_kept() {
        let f = fixture();
        f.workspaces.borrow_mut().push(home_json(&f));
        *f.world.panes.borrow_mut() = format!(
            "[{},{},{}]",
            f.world.coordinator_pane(&f.project),
            pane_json("w9", "w9:t1", "w9:p1", &f.repo),
            pane_json("w8", "w8:t2", "w8:p2", "/tmp")
        );
        *f.world.agents.borrow_mut() = format!("[{}]", agent_json("w8", "w8:t2", "w8:p2", "/tmp", "some-agent", "idle"));
        assert_eq!(close(&f), 1);
        // Closing the primary forgot it on the thread.
        assert_eq!(thread::load(&f.project, "t-0001").unwrap().repo_workspace, "");
    }

    #[test]
    fn a_resolved_tab_threads_empty_own_space_is_closed_once() {
        let f = tab_fixture(Status::Resolved, 0, "Research");
        assert_eq!(close(&f), 1);
        let calls = f.world.runner.calls.borrow();
        let closed = calls.iter().find(|c| c.display().contains("workspace close")).unwrap();
        assert!(closed.args.ends_with(&["close".to_string(), "w8".to_string()]), "{}", closed.display());
        drop(calls);
        assert_eq!(items(&f), ["closed empty Space Research (w8)"]);
        // Forgotten: the record no longer claims a Space herdr does not show.
        assert_eq!(thread::load(&f.project, "t-0001").unwrap().own_workspace, "");
        assert_eq!(close(&f), 1);
    }

    #[test]
    fn a_legacy_tab_record_is_closed_by_its_workspace_id() {
        let f = tab_fixture(Status::Resolved, 0, "Research");
        thread::update(&f.project, "t-0001", |t| t.own_workspace.clear()).unwrap();
        assert_eq!(close(&f), 1);
        assert_eq!(thread::load(&f.project, "t-0001").unwrap().workspace_id, "");
        assert_eq!(close(&f), 1);
    }

    #[test]
    fn a_tab_threads_own_space_is_kept_while_the_thread_works_there() {
        let f = tab_fixture(Status::Open, 1, "Research");
        let folder = f.project.dir().join("threads/t-0001");
        *f.world.panes.borrow_mut() = format!("[{},{}]", f.world.coordinator_pane(&f.project), pane_json("w8", "w8:t1", "w8:p1", &folder.to_string_lossy()));
        assert_eq!(close(&f), 0);
        assert_eq!(thread::load(&f.project, "t-0001").unwrap().own_workspace, "w8");
        // Resolved: its last tab is gone from the records' eyes and the
        // Space is reclaimed, while the home Space is never touched.
        thread::update(&f.project, "t-0001", |t| {
            t.status = Status::Resolved;
            t.workspace_id.clear();
        })
        .unwrap();
        assert_eq!(close(&f), 1);
        let calls = f.world.runner.calls.borrow();
        assert!(calls.iter().any(|c| c.args.ends_with(&["close".to_string(), "w8".to_string()])));
        assert!(!calls.iter().any(|c| c.args.ends_with(&["close".to_string(), "w1".to_string()])), "the home Space is never closed");
    }

    #[test]
    fn a_reused_id_or_a_pane_outside_the_folder_keeps_a_space() {
        // Herdr reused w8 for something else: the label does not match.
        let f = tab_fixture(Status::Resolved, 0, "mine");
        assert_eq!(close(&f), 0);
        // The label matches, but a live pane in it works elsewhere.
        let f = tab_fixture(Status::Resolved, 1, "Research");
        *f.world.panes.borrow_mut() = format!("[{},{}]", f.world.coordinator_pane(&f.project), pane_json("w8", "w8:t1", "w8:p1", "/tmp"));
        assert_eq!(close(&f), 0);
        // A busy pane anywhere in it keeps it too.
        let folder = f.project.dir().join("threads/t-0001");
        *f.world.panes.borrow_mut() = format!("[{},{}]", f.world.coordinator_pane(&f.project), pane_json("w8", "w8:t1", "w8:p1", &folder.to_string_lossy()));
        f.busy.borrow_mut().push("w8:p1".into());
        assert_eq!(close(&f), 0);
        f.busy.borrow_mut().clear();
        assert_eq!(close(&f), 1);
    }

    /// Two lanes with two coordinator panes; `live` records the second lane's
    /// pane in `w8`, the orphan home Space, which only the lane keeps alive.
    fn two_lanes_in(project: &Project, lane: crate::lanes::Status) {
        crate::lanes::Registry {
            lanes: vec![
                crate::lanes::Lane { id: "c-1".into(), name: "fix".into(), label: "抢修".into(), pane_id: "w1:p1".into(), ..Default::default() },
                crate::lanes::Lane { id: "c-2".into(), name: "books".into(), label: "收藏".into(), pane_id: "w8:p2".into(), status: lane, ..Default::default() },
            ],
            default: "c-1".into(),
            next: 3,
        }
        .save(project)
        .unwrap();
        crate::coordinator::save_live(
            project,
            &[crate::coordinator::LivePane { pane_id: "w8:p2".into(), workspace_id: "w8".into(), lane: "c-2".into(), ..Default::default() }],
        )
        .unwrap();
    }

    fn with_home_and_a_lane(lane: crate::lanes::Status) -> Fixture {
        let f = fixture();
        f.workspaces.borrow_mut().push(home_json(&f));
        *f.world.panes.borrow_mut() = format!(
            "[{},{},{}]",
            f.world.coordinator_pane(&f.project),
            pane_json("w9", "w9:t1", "w9:p1", &f.repo),
            pane_json("w8", "w8:t2", "w8:p2", "/tmp")
        );
        two_lanes_in(&f.project, lane);
        f
    }

    #[test]
    fn an_open_lane_protects_its_home_space_and_a_closed_lane_does_not() {
        // Open: the lane's coordinator lives in w8, so the sweep keeps it and
        // closes only the repository's primary Space.
        let f = with_home_and_a_lane(crate::lanes::Status::Open);
        assert_eq!(close(&f), 1);
        {
            let calls = f.world.runner.calls.borrow();
            assert!(!calls.iter().any(|c| c.args.ends_with(&["close".to_string(), "w8".to_string()])), "an open lane keeps its Space");
        }
        // Closed: the same live pane no longer protects it (§6.4, §9).
        let f = with_home_and_a_lane(crate::lanes::Status::Closed);
        assert_eq!(close(&f), 2);
        let calls = f.world.runner.calls.borrow();
        assert!(calls.iter().any(|c| c.args.ends_with(&["close".to_string(), "w8".to_string()])), "a closed lane must not block the sweep");
    }

    #[test]
    fn sync_home_label_renames_when_project_md_changed() {
        let f = fixture();
        let md = f.project.dir().join("PROJECT.md");
        let text = std::fs::read_to_string(&md).unwrap();
        std::fs::write(&md, text.replace(r#"name = "Demo""#, r#"name = "Beta""#)).unwrap();
        let old = format!("Demo{}", crate::grouping::HOME_MARK);
        let new = format!("Beta{}", crate::grouping::HOME_MARK);
        let label = Rc::new(RefCell::new(old));
        let shown = label.clone();
        f.world.runner.on_fn(
            |c| c.display().contains("workspace get"),
            move |_| Ok(ok(&format!(r#"{{"result":{{"workspace":{{"label":"{}"}}}}}}"#, shown.borrow()))),
        );
        f.world.runner.on("workspace rename", ok(r#"{"result":{}}"#));
        let herdr = Herdr::new(f.world.env.herdr_bin(), f.project.coordinator().unwrap().socket, &f.world.runner);
        sync_home_label(&f.project, &herdr);
        assert_eq!(f.world.runner.count("workspace rename"), 1);
        let calls = f.world.runner.calls.borrow();
        let renamed = calls.iter().find(|c| c.display().contains("workspace rename")).unwrap();
        assert!(renamed.args.contains(&new));
        drop(calls);
        // Already right: no second rename.
        *label.borrow_mut() = new;
        sync_home_label(&f.project, &herdr);
        assert_eq!(f.world.runner.count("workspace rename"), 1);
    }
}
