//! Adopting an already-running local agent pane into a project.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::core::{Agent, Core};
use crate::paths::Ctx;
use crate::project::{self, Project};
use crate::runner::Cmd;
use crate::thread::{self, Kind, Status, Thread};
use crate::{coordinator, threads, ticker};

const DEFAULT_TASK: &str = "Continue the work you were already doing in this pane. It now belongs to the project described above: follow its instructions, and when you finish or stop to wait for the user, write the report.";

/// The refusals `thread adopt` makes before anything is created. Returns the
/// agent herdr detects in the pane.
pub fn adoptable_agent(ctx: &Ctx, herdr: &Core, socket: &str, pane: &str) -> Result<Agent> {
    adoptable_agent_except(ctx, herdr, socket, pane, None)
}

/// The same checks, with one project exempted from the "already a coordinator"
/// refusal. `hp lane adopt` passes its own project: a pane in the project home
/// matches the old single-record test by definition, and that is exactly the
/// pane the command exists to claim. Threads and other projects' coordinators
/// are still refused.
pub fn adoptable_agent_except(ctx: &Ctx, herdr: &Core, socket: &str, pane: &str, own: Option<&str>) -> Result<Agent> {
    let agents = herdr.agent_list().map_err(|e| anyhow::anyhow!("the herdr session at {socket} is not reachable: {e}"))?;
    let agent = agents
        .into_iter()
        .find(|a| a.pane_id == pane)
        .with_context(|| format!("no agent is detected in pane {pane} of the session at {socket}; only a running local agent pane can be adopted"))?;
    // A pane is identified by (socket, id): the same id in another session is
    // another pane.
    for slug in project::list_slugs(&ctx.root) {
        let Ok(other) = Project::load(&ctx.root, &slug) else {
            continue;
        };
        let Some(record) = other.coordinator() else {
            continue;
        };
        if record.socket != socket {
            continue;
        }
        if own != Some(slug.as_str()) && coordinator::is_coordinator(&record, &agent) {
            bail!("pane {pane} is a coordinator of `{slug}` (its working directory is the project folder)");
        }
        if let Some(t) = thread::list(&other).iter().find(|t| t.status != Status::Resolved && t.pane_id == pane && thread::agent_matches(t, &agent)) {
            bail!("pane {pane} is already thread {} of `{slug}`", t.id);
        }
    }
    Ok(agent)
}

pub fn adopt(ctx: &Ctx, slug: &str, pane: &str, title: &str, task: Option<String>) -> Result<Thread> {
    let project = Project::load(&ctx.root, slug)?;
    if project.status() != project::Status::Active {
        bail!("`{slug}` is {}; adopting is refused until it is active again", project.status());
    }
    if title.trim().is_empty() {
        bail!("--title may not be empty");
    }
    let record = project.coordinator().with_context(|| format!("`{slug}` has never been opened; run `open {slug}` first"))?;
    ticker::start(ctx)?;
    let herdr = Core::new(ctx.env.hc_bin(), &record.socket, ctx.runner);
    let agent = adoptable_agent(ctx, &herdr, &record.socket, pane)?;

    // The pane's repository and branch, when it is in one.
    let git = |args: &[&str]| -> Option<String> {
        let out = ctx.runner.run(&Cmd::new("git", Duration::from_secs(5)).args(["-C", &agent.cwd]).args(args.iter().copied())).ok()?;
        out.success().then(|| out.stdout.trim().to_string()).filter(|s| !s.is_empty())
    };
    let repo = git(&["rev-parse", "--show-toplevel"]).unwrap_or_default();
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD").unwrap_or_default();
    let origin = git(&["remote", "get-url", "origin"]).unwrap_or_default();

    // Same ownership rule as `thread start`: the record is created with the
    // initiating coordinator's lane and nothing later rewrites it (§4.2). A
    // pre-lane project asks Herdr for nothing extra and writes no lane.
    let lane = if crate::lanes::split_in_use(&project) {
        let agents = herdr.agent_list().unwrap_or_default();
        crate::lanes::lane_of_initiator(ctx, &project, &agents)
    } else {
        String::new()
    };
    let created = thread::allocate(&project, |t| {
        t.title = title.trim().to_string();
        t.kind = Kind::Adopted;
        t.repo = repo;
        t.branch = branch;
        t.origin = origin;
        t.agent = agent.agent.clone();
        // Not started by the binary: whatever name herdr reports, possibly empty.
        t.agent_name = agent.name.clone();
        t.cwd = agent.cwd.clone();
        t.workspace_id = agent.workspace_id.clone();
        t.tab_id = agent.tab_id.clone();
        t.pane_id = agent.pane_id.clone();
        t.lane = lane;
    })?;
    let id = created.id.clone();
    let task = task.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| DEFAULT_TASK.to_string());

    let briefed = (|| -> Result<()> {
        {
            let _lock = project.lock()?;
            project::write_atomic(&thread::task_path(&project, &id), task.as_bytes())?;
        }
        // Two adopted panes in one directory still get separate thread directories.
        let dir = thread::thread_dir(&agent.cwd, slug, &id);
        let with_dir = Thread { thread_dir: dir.clone(), ..created.clone() };
        let brief = thread::brief_for(&project, &with_dir, &task, false)?;
        std::fs::create_dir_all(Path::new(&dir).join("library")).with_context(|| format!("could not create {dir}"))?;
        threads::exclude_from_git(ctx.runner, &agent.cwd)?;
        project::write_atomic(&Path::new(&dir).join("brief.md"), brief.as_bytes())?;
        thread::update(&project, &id, |t| t.thread_dir = dir)?;
        Ok(())
    })();
    if let Err(error) = briefed {
        let message = format!("{error:#}");
        let _ = thread::update(&project, &id, |t| {
            t.status = Status::Failed;
            t.error = message;
        });
        return Err(error);
    }

    let open = thread::update(&project, &id, |t| {
        t.status = Status::Open;
        t.prompt_pending = true;
        t.last_state = agent.agent_status.clone();
        t.last_state_change = project::now();
    })?;
    // Prompt now when the agent is ready for one and its box is empty (a
    // draft is never merged into the brief); otherwise the ticker's delivery
    // path sends the line later (also when the agent ends in `done`).
    if agent.ready() {
        crate::brief::deliver(&project, &herdr, &open, &agent, crate::brief::Sender::Manual)?;
    }
    let adopted = thread::load(&project, &id)?;
    threads::report_thread_tokens(&herdr, &adopted, slug, thread::Group::Working);
    Ok(adopted)
}
