//! delete: a project is in use, or it is gone from the root.

use anyhow::{Context, Result, bail};

use crate::paths::Ctx;
use crate::project::Project;
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
