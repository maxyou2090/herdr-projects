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
        if t.status == thread::Status::Resolved {
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
    let mut close = |workspace: &str, what: &str| match view.herdr.workspace_close(workspace) {
        Ok(_) => notes.push(format!("closed {what} (workspace {workspace})")),
        Err(error) if error.code == "workspace_group_close_required" => notes.push(format!("left {what} open: it is a repository's primary workspace")),
        Err(error) => notes.push(format!("could not close {what}: {error}")),
    };
    let mut done = Vec::new();
    let home = project.coordinator().map(|c| c.workspace_id).unwrap_or_default();
    for t in thread::list(project).iter().filter(|t| {
        t.status != thread::Status::Resolved && matches!(t.kind, thread::Kind::Worktree | thread::Kind::Tab | thread::Kind::Checkout)
    }) {
        // A worktree thread's Space is on its worktree, a tab thread's on its
        // own `threads/<id>/` folder; one still living in the home (placed
        // before it got a Space of its own) is closed with the home below.
        // A home thread's tab sits in the home itself and closes with it too,
        // so it needs no entry here.
        // A checkout thread's Space is named by its record (never by its
        // folder: the repository's primary Space can hold panes in that same
        // main checkout, and archive never closes it), and only by the
        // `own_workspace` a record gets when it is placed in one: a legacy
        // checkout thread's `workspace_id` names the project's home instead.
        let workspace = if t.kind == thread::Kind::Checkout {
            let own = t.own_workspace.as_str();
            view.herdr
                .workspace_list()
                .unwrap_or_default()
                .iter()
                .find(|w| !own.is_empty() && own != home && w.workspace_id == own && w.worktree.is_none())
                .map(|w| w.workspace_id.clone())
        } else {
            let root = if t.kind == thread::Kind::Worktree { t.worktree_path.as_str() } else { t.cwd.as_str() };
            view.panes.iter().find(|p| !root.is_empty() && p.workspace_id != home && std::path::Path::new(&p.cwd).starts_with(root)).map(|p| p.workspace_id.clone())
        };
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
                    crate::labels::clear_pane(&view.herdr, &pane);
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
    if !force
        && let Some(view) = threads::session_view(ctx, &project)
    {
        let alive = alive_panes(&project, &view);
        if !alive.is_empty() {
            let list: Vec<String> = alive.iter().map(|(what, pane, _)| format!("{what} (pane {pane})")).collect();
            bail!("`{slug}` still has live panes: {}. Close them, or pass --force.", list.join(", "));
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
            println!("  {}: worktree {}, branch {} in {}", t.id, if t.worktree_path.is_empty() { "-" } else { &t.worktree_path }, if t.branch.is_empty() { "-" } else { &t.branch }, t.repo);
        }
    }
    // Only point at leftovers that actually exist: the per-project safety
    // table lives in the user config and would silently apply to a new project
    // at the same path.
    if crate::safety::project_table_exists(&ctx.config_dir, &canonical) {
        println!(
            "The `[safety.\"{}\"]` table for this path remains in {} and would apply to a new project at the same path.",
            canonical.display(),
            ctx.config_dir.display()
        );
    }
    Ok(())
}
