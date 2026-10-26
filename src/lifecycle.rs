//! pause, resume, archive, unarchive and delete.

use anyhow::{Context, Result, bail};

use crate::paths::Ctx;
use crate::project::{Project, Status};
use crate::thread;
use crate::threads::{self, SessionView};
use crate::coordinator;

/// (what, pane id) of every recorded pane that is alive in the project's session.
pub(crate) fn alive_panes(project: &Project, view: &SessionView) -> Vec<(String, String, String)> {
    let mut alive = Vec::new();
    if let Some(record) = project.coordinator() {
        if crate::lanes::split_in_use(project) {
            // Every lane can have its own coordinator pane, so no lane may be
            // missed here (§9): `classify_all` names the lane of every live
            // pane, and an `Ambiguous` pane is still somebody (it is counted
            // for liveness, never given a lane — §5.2).
            let answers = crate::lanes::classify_all(project, &record, &view.agents);
            for (agent, answer) in view.agents.iter().zip(answers) {
                if answer.counts_as_coordinator() && !alive.iter().any(|(_, pane, _): &(String, String, String)| pane == &agent.pane_id) {
                    alive.push(("coordinator".to_string(), agent.pane_id.clone(), agent.agent_status.clone()));
                }
            }
            // A lane whose pane is listed but matched no rule still counts as
            // recorded, exactly the old record fallback — per lane.
            let registry = crate::lanes::Registry::for_project(project);
            for lane in &registry.lanes {
                if lane.pane_id.is_empty() || alive.iter().any(|(_, pane, _)| pane == &lane.pane_id) {
                    continue;
                }
                let lane_record = registry.coordinator_for(project, lane);
                if view.panes.iter().any(|p| coordinator::pane_matches(&lane_record, p)) {
                    alive.push(("coordinator".to_string(), lane.pane_id.clone(), String::new()));
                }
            }
        } else {
            let mut any = false;
            for agent in view.agents.iter().filter(|a| coordinator::is_coordinator(&record, a)) {
                any = true;
                alive.push(("coordinator".to_string(), agent.pane_id.clone(), agent.agent_status.clone()));
            }
            if !any && view.panes.iter().any(|p| coordinator::pane_matches(&record, p)) {
                alive.push(("coordinator".to_string(), record.pane_id.clone(), String::new()));
            }
        }
    }
    let now = jiff::Timestamp::now();
    for t in thread::list(project) {
        if t.status == thread::Status::Resolved || t.is_remote() {
            continue;
        }
        let live = thread::live_state(&t, &view.agents, &view.panes, now);
        if live.pane_exists {
            alive.push((t.id.clone(), t.pane_id.clone(), live.agent_state.unwrap_or_default()));
        }
    }
    alive
}

/// The Spaces a project's coordinators live in on archive, one per lane —
/// `(space, what to call it)`. Without a registry this is the one record's
/// Space, exactly as before. With one, each lane's live pane is found through
/// the resolver and its record is the fallback, so archive never leaves a
/// lane's coordinator behind (§9).
fn coordinator_workspaces(project: &Project, view: &SessionView) -> Vec<(String, String)> {
    let Some(record) = project.coordinator() else {
        return Vec::new();
    };
    let label = |lane: &crate::lanes::Lane| -> String {
        let name = if lane.label.is_empty() { &lane.name } else { &lane.label };
        format!("lane `{name}` ({})'s workspace", lane.id)
    };
    if !crate::lanes::split_in_use(project) {
        return coordinator::workspace_open(&record, &view.panes)
            .then(|| (record.workspace_id.clone(), "the project workspace".to_string()))
            .into_iter()
            .collect();
    }
    let registry = crate::lanes::Registry::for_project(project);
    let mut out: Vec<(String, String)> = Vec::new();
    let answers = crate::lanes::classify_all(project, &record, &view.agents);
    for (agent, answer) in view.agents.iter().zip(answers) {
        // Only a pane that resolved to a **lane** is a lane's coordinator. An
        // `Ambiguous` pane is still somebody for liveness, but it claims no
        // Space of this project's, so archive must not close one on its word.
        let Some(lane_id) = answer.lane_id() else {
            continue;
        };
        if agent.workspace_id.is_empty() || out.iter().any(|(space, _)| space == &agent.workspace_id) {
            continue;
        }
        let what = registry.get(lane_id).map(&label).unwrap_or_else(|| "a coordinator's workspace".to_string());
        out.push((agent.workspace_id.clone(), what));
    }
    for lane in &registry.lanes {
        let lane_record = registry.coordinator_for(project, lane);
        if lane_record.workspace_id.is_empty() || out.iter().any(|(space, _)| space == &lane_record.workspace_id) {
            continue;
        }
        if coordinator::workspace_open(&lane_record, &view.panes) {
            out.push((lane_record.workspace_id.clone(), label(lane)));
        }
    }
    out
}

/// Closes the open workspaces of local threads, then the project's own. With
/// lanes the project's own means every lane's (`coordinator_workspaces`).
/// Never `--group`: archive never closes a repository's primary workspace,
/// and Herdr's refusal (`workspace_group_close_required`) is reported.
fn close_workspaces(project: &Project, view: &SessionView) -> Vec<String> {
    let mut notes = Vec::new();
    let mut close = |workspace: &str, what: &str| match view.herdr.call(&["workspace", "close", workspace], crate::herdr::CALL_TIMEOUT) {
        Ok(_) => notes.push(format!("closed {what} (workspace {workspace})")),
        Err(error) if error.code == "workspace_group_close_required" => notes.push(format!("left {what} open: it is a repository's primary workspace")),
        Err(error) => notes.push(format!("could not close {what}: {error}")),
    };
    let mut done = Vec::new();
    let home = project.coordinator().map(|c| c.workspace_id).unwrap_or_default();
    for t in thread::list(project).iter().filter(|t| t.status != thread::Status::Resolved && !t.is_remote() && matches!(t.kind, thread::Kind::Worktree | thread::Kind::Tab)) {
        // A worktree thread's Space is on its worktree, a tab thread's on its
        // own `threads/<id>/` folder; one still living in the home (placed
        // before it got a Space of its own) is closed with the home below.
        // A home thread's tab sits in the home itself and closes with it too,
        // so it needs no entry here.
        let root = if t.kind == thread::Kind::Worktree { t.worktree_path.as_str() } else { t.cwd.as_str() };
        let workspace = view.panes.iter().find(|p| !root.is_empty() && p.workspace_id != home && std::path::Path::new(&p.cwd).starts_with(root)).map(|p| p.workspace_id.clone());
        if let Some(workspace) = workspace
            && !done.contains(&workspace)
        {
            close(&workspace, &format!("{}'s workspace", t.id));
            done.push(workspace);
        }
    }
    for (workspace, what) in coordinator_workspaces(project, view) {
        if workspace.is_empty() || done.contains(&workspace) {
            continue;
        }
        crate::sidebar::clear_workspace(&view.herdr, &workspace);
        close(&workspace, &what);
        done.push(workspace);
    }
    notes
}

pub fn set_status(ctx: &Ctx, slug: &str, status: Status) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    let current = project.status();
    match (current, status) {
        (Status::Archived, Status::Paused) => bail!("`{slug}` is archived; `unarchive` it first"),
        (Status::Archived, Status::Active) | (_, Status::Archived) | (_, Status::Paused) | (Status::Paused, Status::Active) | (Status::Active, Status::Active) => {}
    }
    project.set_status(status)?;
    println!("`{slug}` is now {status}");

    let view = threads::session_view(ctx, &project);
    match status {
        Status::Paused => {
            println!("The ticker skips it and `thread start` is refused. Running agents are not interrupted.");
            if let Some(view) = &view {
                for (what, pane, _) in alive_panes(&project, view).into_iter().filter(|(_, _, s)| s == "working") {
                    println!("  still working: {what} (pane {pane})");
                }
            }
        }
        Status::Archived => {
            println!("It is marked `archived` in `list`, skipped by the projects picker, overviews and the ticker, and `open` is refused until `unarchive`. Its folder and every unresolved thread's worktree stay.");
            if let Some(view) = &view {
                for (_, pane, _) in alive_panes(&project, view) {
                    crate::sidebar::clear_pane(&view.herdr, &pane);
                }
                for note in close_workspaces(&project, view) {
                    println!("  {note}");
                }
            }
        }
        Status::Active if current == Status::Archived => {
            // Unarchive reopens it: the workspace and a coordinator.
            let options = crate::coordinator::OpenOptions { session: crate::paths::SessionFlags::default(), rebind: false, profile: None, new: false, here: false, person: true, lane: None, lane_name: None };
            if let Err(error) = crate::coordinator::open(ctx, slug, &options) {
                println!("reopen it with `open {slug}` ({error:#})");
            }
        }
        Status::Active => {}
    }
    Ok(())
}

/// Moves the project folder to `<root>/.trash/<slug>-<timestamp>/`. Touches no
/// worktree, branch or pull request.
pub fn delete(ctx: &Ctx, slug: &str, force: bool) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    if !force {
        if let Some(view) = threads::session_view(ctx, &project) {
            let alive = alive_panes(&project, &view);
            if !alive.is_empty() {
                let list: Vec<String> = alive.iter().map(|(what, pane, _)| format!("{what} (pane {pane})")).collect();
                bail!("`{slug}` still has live panes: {}. Close them, or pass --force.", list.join(", "));
            }
        }
    }
    let threads = thread::list(&project);
    let canonical = project.canonical_dir();

    let trash = ctx.root.join(".trash");
    std::fs::create_dir_all(&trash)?;
    let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ").to_string();
    let target = trash.join(format!("{slug}-{stamp}"));
    {
        // Held while the folder moves, so no writer lands in between; writers
        // re-check PROJECT.md after taking the lock and drop their write.
        let _lock = project.lock()?;
        std::fs::rename(project.dir(), &target).with_context(|| format!("could not move {} to the trash", project.dir().display()))?;
    }
    println!("moved `{slug}` to {}", target.display());

    let left: Vec<&thread::Thread> = threads.iter().filter(|t| !t.worktree_path.is_empty() || !t.branch.is_empty()).collect();
    if !left.is_empty() {
        println!("Left alone (remove them yourself if you no longer want them):");
        for t in left {
            let place = if t.machine.is_empty() { String::new() } else { format!(" on {}", t.machine) };
            println!("  {}: worktree {}{place}, branch {} in {}", t.id, if t.worktree_path.is_empty() { "-" } else { &t.worktree_path }, if t.branch.is_empty() { "-" } else { &t.branch }, t.repo);
        }
    }
    // Only point at leftovers that actually exist: the per-project safety
    // table and this project's routine approvals both live in the user config
    // and would silently apply to a new project at the same path.
    let table = crate::safety::project_table_exists(&ctx.config_dir, &canonical);
    let path_key = canonical.to_string_lossy().into_owned();
    let approvals = crate::routine::approvals(&ctx.config_dir).iter().any(|a| a.project == path_key);
    if table || approvals {
        let subject = match (table, approvals) {
            (true, true) => format!("The `[safety.\"{}\"]` table and routine approvals for this path remain", canonical.display()),
            (true, false) => format!("The `[safety.\"{}\"]` table for this path remains", canonical.display()),
            (false, _) => "Routine approvals for this path remain".to_string(),
        };
        println!("{subject} in {} and would apply to a new project at the same path.", ctx.config_dir.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenarios::World;

    #[test]
    fn delete_refuses_while_a_pane_is_alive_and_force_moves_the_folder() {
        let world = World::new();
        let project = world.project("demo", "a.sock");
        world.thread(&project, world.home.path(), |t| t.branch = "hp/demo/t-0001-x".into());
        *world.panes.borrow_mut() = format!("[{}]", world.coordinator_pane(&project));
        let ctx = world.ctx();

        let error = delete(&ctx, "demo", false).unwrap_err().to_string();
        assert!(error.contains("coordinator (pane w1:p1)"), "{error}");
        assert!(project.dir().is_dir());

        delete(&ctx, "demo", true).unwrap();
        assert!(!project.dir().exists());
        let trashed: Vec<_> = std::fs::read_dir(world.root.join(".trash")).unwrap().flatten().collect();
        assert_eq!(trashed.len(), 1);
        assert!(trashed[0].file_name().to_string_lossy().starts_with("demo-"));
        assert!(trashed[0].path().join("PROJECT.md").is_file());
        assert!(trashed[0].path().join("threads/t-0001.toml").is_file());
        // Nothing but herdr list calls ran: no worktree, branch or PR was touched.
        assert!(world.runner.calls.borrow().iter().all(|c| c.display().contains(" list")));
        // `.trash` is not a project.
        assert!(crate::project::list_slugs(&world.root).is_empty());
    }

    #[test]
    fn delete_without_live_panes_needs_no_force() {
        let world = World::new();
        let project = world.project("demo", "a.sock");
        delete(&world.ctx(), "demo", false).unwrap();
        assert!(!project.dir().exists());
    }

    #[test]
    fn archive_reaches_every_lane_and_alive_panes_lists_them_all() {
        let world = World::new();
        let project = world.project("demo", "a.sock");
        let dir = project.canonical_dir().to_string_lossy().into_owned();
        crate::lanes::Registry {
            lanes: vec![
                crate::lanes::Lane { id: "c-1".into(), name: "fix".into(), label: "抢修".into(), pane_id: "w1:p1".into(), ..Default::default() },
                crate::lanes::Lane { id: "c-2".into(), name: "books".into(), label: "收藏".into(), pane_id: "w2:p1".into(), ..Default::default() },
            ],
            default: "c-1".into(),
            next: 3,
        }
        .save(&project)
        .unwrap();
        *world.agents.borrow_mut() = format!(
            "[{},{}]",
            crate::scenarios::agent_json_tokens("w1", "w1:p1", &dir, &[("hp_project", "demo"), ("hp_lane", "c-1")]),
            crate::scenarios::agent_json_tokens("w2", "w2:p1", &dir, &[("hp_project", "demo"), ("hp_lane", "c-2")]),
        );
        *world.panes.borrow_mut() = format!("[{},{}]", crate::scenarios::pane_json("w1", "w1:t1", "w1:p1", &dir), crate::scenarios::pane_json("w2", "w2:t1", "w2:p1", &dir));
        world.runner.on("workspace close", crate::runner::fake::ok(r#"{"result":{}}"#));
        let ctx = world.ctx();
        let view = crate::threads::session_view(&ctx, &project).unwrap();
        // Neither lane's coordinator may be missed by archive or by a delete's
        // live-pane refusal (§9).
        let alive = alive_panes(&project, &view);
        let panes: Vec<&str> = alive.iter().map(|(_, pane, _)| pane.as_str()).collect();
        assert_eq!(panes, ["w1:p1", "w2:p1"]);
        let notes = close_workspaces(&project, &view);
        assert_eq!(notes.len(), 2, "{notes:?}");
        let calls = world.runner.calls.borrow();
        for space in ["w1", "w2"] {
            assert!(calls.iter().any(|c| c.args.ends_with(&["close".to_string(), space.to_string()])), "workspace {space} not closed: {}", calls.iter().map(|c| c.display()).collect::<Vec<_>>().join(" | "));
        }
    }

    #[test]
    fn archive_clears_tokens_and_blocks_pause() {
        let world = World::new();
        let project = world.project("demo", "a.sock");
        *world.panes.borrow_mut() = format!("[{}]", world.coordinator_pane(&project));
        let ctx = world.ctx();
        set_status(&ctx, "demo", Status::Archived).unwrap();
        assert_eq!(project.status(), Status::Archived);
        let calls = world.runner.calls.borrow();
        let clear = calls.iter().find(|c| c.display().contains("--clear-token")).expect("tokens cleared");
        assert!(clear.display().contains("w1:p1"));
        drop(calls);
        assert!(set_status(&ctx, "demo", Status::Paused).is_err());
        set_status(&ctx, "demo", Status::Active).unwrap();
        assert_eq!(project.status(), Status::Active);
    }
}
