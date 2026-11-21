//! The `thread` subcommands. Each is one deterministic mechanic; the
//! coordinator decides whether, what and where.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::core::{Agent, Core, Pane, Workspace};
use crate::paths::Ctx;
use crate::project::{self, Project};
use crate::runner::{Cmd, Runner};
use crate::thread::{self, CopyOutcome, Group, Kind, Live, Recorded, Status, Thread};
use crate::{coordinator, ticker};

const GIT_TIMEOUT: Duration = Duration::from_secs(5);
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// The project's session as the binary sees it right now.
pub struct SessionView<'a> {
    pub herdr: Core<'a>,
    pub socket: String,
    pub agents: Vec<Agent>,
    pub panes: Vec<Pane>,
}

/// `None` when the project's session is unreachable, or it has no usable
/// record and no agent works in its folder in the current session.
pub fn session_view<'a>(ctx: &'a Ctx, project: &Project) -> Option<SessionView<'a>> {
    // A record is the session it names, empty for the default core; whether that
    // core answers is the two calls below, not a path on disk.
    let record = match project.coordinator() {
        Some(record) => record,
        None => {
            let session = crate::paths::resolve_session(&Default::default(), ctx.env).ok()?;
            let socket = session.socket.clone();
            let agents = Core::new(ctx.env.hc_bin(), &socket, ctx.runner).agent_list().ok()?;
            // Read only: the ticker records it on its next tick.
            crate::coordinator::found(project, &socket, &agents)?
        }
    };
    let herdr = Core::new(ctx.env.hc_bin(), &record.socket, ctx.runner);
    let agents = herdr.agent_list().ok()?;
    let panes = herdr.pane_list().ok()?;
    Some(SessionView { herdr, socket: record.socket, agents, panes })
}

fn require_session<'a>(ctx: &'a Ctx, project: &Project) -> Result<SessionView<'a>> {
    session_view(ctx, project).with_context(|| {
        format!(
            "the session of `{}` is not reachable; run `open {}` first",
            project.slug, project.slug
        )
    })
}

fn git(runner: &dyn Runner, repo: &str, args: &[&str], timeout: Duration) -> Result<String> {
    let out = runner.run(&Cmd::new("git", timeout).args(["-C", repo]).args(args.iter().copied()))?;
    if !out.success() {
        bail!("git {}: {}", args.join(" "), out.error_text());
    }
    Ok(out.stdout.trim().to_string())
}

pub fn report_thread_tokens(herdr: &Core, thread: &Thread, slug: &str) {
    crate::labels::report_pane(herdr, &thread.pane_id, &crate::labels::thread_display(thread), slug, None);
}

fn clear_thread_tokens(herdr: &Core, thread: &Thread) {
    crate::labels::clear_pane(herdr, &thread.pane_id);
}

pub struct StartArgs {
    pub title: String,
    pub repo: Option<String>,
    /// The profile (`--profile`), default `thread_profile` in PROJECT.md;
    /// it must be on the project's allow-list.
    pub profile: Option<String>,
    /// Where the thread works (`--kind worktree|checkout|local`); default
    /// worktree with a repo, `local` (its own folder on `threads/<id>/`)
    /// without one. Every kind puts the thread's tab in the project's thread
    /// Space (`docs/thread-space.md`); `tab` and `home` only survive as the
    /// old names `Kind::parse` still takes and old records still carry.
    pub kind: Option<Kind>,
    pub base: Option<String>,
    pub task: String,
}

/// The profile a thread starts with.
#[derive(Debug)]
pub struct ThreadProfile {
    pub name: String,
    pub agent: String,
}

impl ThreadProfile {
    fn apply(&self, t: &mut Thread) {
        t.agent = self.agent.clone();
        t.profile = self.name.clone();
    }
}

/// A thread's profile is this machine's (`requested`, else the project's
/// `thread_profile`). The name must be on this project's allow-list.
pub fn thread_profile(ctx: &Ctx, project: &Project, requested: Option<&str>) -> Result<ThreadProfile> {
    let profile = crate::profiles::for_project(ctx, project, crate::profiles::Role::Thread, requested)?;
    Ok(ThreadProfile { name: profile.name.clone(), agent: profile.agent().to_string() })
}

/// The placement of a new thread from what was asked and whether it has a repo.
pub fn placement(kind: Option<Kind>, has_repo: bool) -> Result<Kind> {
    match (kind, has_repo) {
        (None, true) => Ok(Kind::Worktree),
        (None, false) => Ok(Kind::Local),
        (Some(Kind::Worktree), false) | (Some(Kind::Checkout), false) => bail!("a worktree or checkout thread needs --repo"),
        (Some(Kind::Adopted), _) => bail!("an adopted thread is made with `thread adopt`"),
        (Some(kind), _) => Ok(kind),
    }
}

/// Creates the workspace or tab, the thread directory and the brief, then
/// returns. The agent is launched by the ticker, so there is one delivery path.
pub fn start(ctx: &Ctx, slug: &str, args: StartArgs) -> Result<Thread> {
    let project = Project::load(&ctx.root, slug)?;
    if let Some(rename) = crate::rename::pending(&ctx.root, slug) {
        bail!("`{slug}` is being renamed to `{}`; `thread start` is refused until its coordinator reopens there", rename.to);
    }
    if args.title.trim().is_empty() {
        bail!("--title may not be empty");
    }
    if args.task.trim().is_empty() {
        bail!("the task is empty");
    }
    let (settings, _) = project.read_project_md()?;
    // Without a running ticker nothing launches.
    ticker::start(ctx)?;
    let view = require_session(ctx, &project)?;

    let repo = match &args.repo {
        None => String::new(),
        Some(repo) => {
            let path = std::fs::canonicalize(repo)
                .with_context(|| format!("repository {repo} does not exist"))?
                .to_string_lossy()
                .into_owned();
            if !settings.repos.iter().any(|r| r.path == path || &r.path == repo) {
                eprintln!("warning: {path} is not listed in `repos` in PROJECT.md");
            }
            path
        }
    };

    let profile = thread_profile(ctx, &project, args.profile.as_deref())?;
    let kind = placement(args.kind, !repo.is_empty())?;
    // Ownership is written once, at creation, from the lane the initiating
    // coordinator pane claims (§4.2). Everything else — open, adopt, ticker —
    // leaves an existing record's lane alone (§6.3).
    let lane = crate::lanes::lane_of_initiator(ctx, &project, &view.agents);
    let record = thread::allocate(&project, |t| {
        t.title = args.title.trim().to_string();
        t.kind = kind;
        t.repo = repo.clone();
        profile.apply(t);
        t.base = args.base.clone().unwrap_or_default();
        t.lane = lane;
    })?;
    let id = record.id.clone();
    {
        let _lock = project.lock()?;
        project::write_atomic(&thread::task_path(&project, &id), args.task.as_bytes())?;
    }

    match place_and_brief(ctx, &project, &view, &id, false) {
        Ok(thread) => Ok(thread),
        Err(error) => {
            // Nothing is cleaned up automatically; `thread restart` retries.
            let message = format!("{error:#}");
            let _ = thread::update(&project, &id, |t| {
                t.status = Status::Failed;
                t.error = message.clone();
            });
            Err(error.context(format!("thread {id} failed to start; `thread restart {slug} {id}` retries")))
        }
    }
}

/// Steps 2 to 5 of starting a thread, also used by `thread restart` case (a).
fn place_and_brief(ctx: &Ctx, project: &Project, view: &SessionView, id: &str, restart: bool) -> Result<Thread> {
    let slug = &project.slug;
    let record = thread::load(project, id)?;
    let runner = ctx.runner;

    let placed = match record.kind {
        Kind::Worktree => {
            git(runner, &record.repo, &["rev-parse", "--show-toplevel"], GIT_TIMEOUT)
                .with_context(|| format!("{} is not a git repository", record.repo))?;
            let origin = git(runner, &record.repo, &["remote", "get-url", "origin"], GIT_TIMEOUT).unwrap_or_default();
            if !origin.is_empty()
                && let Err(error) = git(runner, &record.repo, &["fetch", "origin"], FETCH_TIMEOUT)
            {
                eprintln!("warning: {error:#}");
            }
            let base = if record.base.is_empty() {
                git(runner, &record.repo, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"], GIT_TIMEOUT)
                    .or_else(|_| git(runner, &record.repo, &["rev-parse", "--abbrev-ref", "HEAD"], GIT_TIMEOUT))
                    .and_then(|base| match base.as_str() {
                        "HEAD" => git(runner, &record.repo, &["rev-parse", "HEAD"], GIT_TIMEOUT),
                        _ => Ok(base),
                    })?
            } else {
                record.base.clone()
            };
            let branch = thread::branch_name(slug, id, &record.title);
            // The folder first, its tab second: a project's threads share one
            // Space (`docs/thread-space.md`), so the worktree is an ordinary git
            // worktree and its tab is placed like any other thread's.
            let path = view.herdr.worktree_add(&record.repo, &branch, &base)?;
            let folder = std::path::PathBuf::from(&path);
            // The worktree's own fields recorded with the placement, so a command
            // killed midway still leaves a record `thread restart` can act on.
            let placed = place_thread(project, view, &record, &folder, |t| {
                t.origin = origin;
                t.base = base;
                t.branch = branch;
                t.worktree_path = path.clone();
            })?;
            // Which worktree this pane stands in: what the cleanup reads to
            // remove this one and no other in the same Space. Marked outside the
            // placement's lock, which is safe: the record already names the pane,
            // so nothing judges that Space empty in between.
            if let Err(error) = view.herdr.worktree_mark(&placed.pane_id, &path) {
                eprintln!("warning: could not mark pane {} as worktree {path} ({error})", placed.pane_id);
            }
            placed
        }
        Kind::Local | Kind::Checkout => place_tab(project, view, &record)?,
        Kind::Adopted => bail!("an adopted thread is not placed by the binary"),
    };
    prepare_thread_agents(&placed)?;
    write_brief(ctx, project, &placed, restart)?;
    finish_placement(project, view, id)
}

fn prepare_thread_agents(thread: &Thread) -> Result<()> {
    if thread.kind != Kind::Worktree {
        return Ok(());
    }
    crate::skills::prepare(std::path::Path::new(&thread.repo), std::path::Path::new(&thread.worktree_path))
        .with_context(|| format!("could not prepare shared .agents directory for thread {}", thread.id))
}

/// The Spaces this project's coordinators sit in, one per open lane: never a
/// place for a thread's tab, and the one Space a thread must never be placed in.
/// A coordinator's own pane is what names it when the record has drifted, which
/// is the normal case here (the record's `cwd` is often `~`).
fn coordinator_spaces(project: &Project, panes: &[Pane], agents: &[Agent]) -> Vec<String> {
    let mut homes = coordinator::coordinator_space_ids(project);
    if let Some(record) = project.coordinator() {
        if let Some(space) = coordinator::project_workspace(&record, panes, agents) {
            homes.push(space);
        }
        if let Some(pane) = panes.iter().find(|p| p.pane_id == record.pane_id) {
            homes.push(pane.workspace_id.clone());
        }
    }
    homes
}

/// The project's shared thread Space — `<display name> Threads` — as it is open
/// right now, if it is (`docs/thread-space.md`). Every thread of the project
/// runs there rather than each in a Space of its own, so the sidebar reads two
/// levels: the project's Space, then its thread Space, then the `t-00xx` tabs.
///
/// The question is not "which Space is called `<display name> Threads`" but
/// "which Space is **this** project's": every project has one of these, they all
/// wear a name ending in ` Threads`, and every project's tick asks this question
/// (`spaces::sync_threads_label`). A Space says whose it is in three ways, and
/// the order is what keeps one Space from becoming two:
///
/// 1. a thread record of this project names it — the strongest statement, and
///    then a name from before a display-name change (`… Threads`) or no name at
///    all is enough: the app can list a Space it made before an hp label landed
///    on a terminal of it (§6.5);
/// 2. it carries today's name, exactly (`<display name> Threads`) — an existing
///    thread Space that nothing records yet;
/// 3. this project's panes are in it (`hp_project=<slug>`) — the label of a Space
///    the app made can be stale, and the panes are what no label change touches.
///
/// Each of the three also requires that the Space is not one of the project's own
/// coordinator Spaces, not a thread's **own** Space from before this change, and
/// not a Space another project's panes are in.
///
/// `panes` is every pane the caller read, and an empty slice means none were
/// read: `spaces::sync_threads_label` runs on every ticker tick and cannot
/// afford a pane listing, so it can be answered by 1 and 2 only, never by 3
/// (§4.2). It is the same judgement either way — the panes only ever add
/// evidence, they never overrule a name.
pub fn threads_space(project: &Project, workspaces: &[Workspace], panes: &[Pane], agents: &[Agent], record: Option<&Thread>) -> Option<String> {
    let homes = coordinator_spaces(project, panes, agents);
    let threads = thread::list(project);
    // A thread's own Space under the old rule, when a thread had one to itself:
    // labelled with the thread's title, or `<title> Threads` for the worktree
    // kind. Both are abandoned, never joined — resolving them closes them.
    let own_old = |w: &Workspace| {
        threads.iter().any(|t| {
            crate::labels::thread_space_matches(&w.label, &t.title)
                || (!t.title.is_empty() && w.label == crate::labels::threads_space_label(&t.title))
        })
    };
    // Whose panes are in a Space. A pane says `hp_project=<slug>`; nothing else a
    // Space carries is as hard to get wrong, and a Space another project's panes
    // are in is that project's, whatever a drifted record or a copy of a name
    // says.
    fn owner(p: &Pane) -> Option<&str> {
        p.tokens.get(crate::labels::PROJECT_TOKEN).map(String::as_str).filter(|slug| !slug.is_empty())
    }
    let mine = |id: &str| panes.iter().any(|p| p.workspace_id == id && owner(p) == Some(project.slug.as_str()));
    let foreign = |id: &str| panes.iter().any(|p| p.workspace_id == id && owner(p).is_some_and(|slug| slug != project.slug));
    let usable = |w: &Workspace| !homes.contains(&w.workspace_id) && !own_old(w) && !foreign(&w.workspace_id);
    // 1: the Space a record of this project names. A Space wearing any other name
    // than a thread Space's is another project's, unless this project's panes are
    // in it.
    let claimed = |w: &Workspace| usable(w) && (w.label.is_empty() || crate::labels::is_threads_space_label(&w.label) || mine(&w.workspace_id));
    let claimed_by = |id: &str| !id.is_empty() && workspaces.iter().any(|w| w.workspace_id == id && claimed(w));
    if let Some(id) = record.map(|t| t.workspace_id.clone()).filter(|id| claimed_by(id)) {
        return Some(id);
    }
    let open: Vec<&Thread> = threads.iter().filter(|t| t.status != Status::Resolved && !t.workspace_id.is_empty()).collect();
    if let Some(t) = open.iter().find(|t| claimed_by(&t.workspace_id)) {
        return Some(t.workspace_id.clone());
    }
    // 2: today's name, exactly. Deliberately not "any name ending in ` Threads`":
    // that is every other project's thread Space too, and each project's tick then
    // renamed the one Space it found around, ten projects taking turns
    // (2026-11-20, §4.2).
    let label = crate::labels::threads_space_label(&project.display_name());
    if let Some(w) = workspaces.iter().find(|w| usable(w) && w.label == label) {
        return Some(w.workspace_id.clone());
    }
    // 3: this project's panes, for a Space the app made whose name is stale or
    // missing. Unreachable with no panes read.
    let count = |w: &Workspace| panes.iter().filter(|p| p.workspace_id == w.workspace_id && owner(p) == Some(project.slug.as_str())).count();
    workspaces.iter().filter(|w| usable(w) && mine(&w.workspace_id)).max_by_key(|w| count(w)).map(|w| w.workspace_id.clone())
}

/// The folder a thread works in: its worktree, the repository's main checkout
/// for a checkout thread, and `threads/<id>/` for the rest.
fn thread_folder(project: &Project, record: &Thread) -> Result<std::path::PathBuf> {
    if record.kind == Kind::Worktree {
        return Ok(std::path::PathBuf::from(&record.worktree_path));
    }
    if record.kind == Kind::Checkout {
        return Ok(std::path::PathBuf::from(&record.repo));
    }
    let folder = project.dir().join("threads").join(&record.id);
    {
        let _lock = project.lock()?;
        if !folder.is_dir() {
            std::fs::create_dir(&folder).with_context(|| format!("could not create {}", folder.display()))?;
        }
    }
    Ok(std::fs::canonicalize(&folder)?)
}

/// A thread's tab in the project's shared thread Space, and the Space itself
/// when this is the project's first thread: named after its display name, its
/// tab named after the thread's id, in hp's labels and in the app alike.
///
/// A new Space is made only when nothing names one (see [`threads_space`]). The
/// caller holds `project.lock()` ([`place_thread`]), which is what keeps two
/// threads started at the same moment from opening one each: the first one's
/// record is written before the second looks.
fn place(project: &Project, view: &SessionView, record: &Thread, folder: &Path) -> Result<crate::core::Created> {
    let tab = crate::labels::thread_tab_label(&record.id);
    let label = crate::labels::threads_space_label(&project.display_name());
    let listed = view.herdr.workspace_list().unwrap_or_default();
    if let Some(space) = threads_space(project, &listed, &view.panes, &view.agents, Some(record)) {
        return Ok(view.herdr.tab_create(&space, folder, &tab)?);
    }
    Ok(view.herdr.workspace_create(folder, &label, &tab)?)
}

/// Places a thread's tab and writes where it landed, all under the project
/// lock: finding the Space, `tab_create`, and the record that names the pane are
/// one step. `spaces::close_empty` holds the same lock, so a tick can never
/// judge a Space on a listing taken before this placement finished and then
/// close it under the new tab (2026-11-20, `docs/thread-space.md` §5).
///
/// `before_finish` writes the fields a kind records ahead of the tab's ids — a
/// worktree's branch and path — and is empty for the rest.
fn place_thread(project: &Project, view: &SessionView, record: &Thread, folder: &Path, before_finish: impl FnOnce(&mut Thread)) -> Result<Thread> {
    let _lock = project.lock()?;
    let created = place(project, view, record, folder)?;
    thread::update_locked(project, &record.id, before_finish)?;
    finish_place(project, view, &record.id, &created, folder)
}

/// Records where a placement ended, so the ticker, a restart and a resolve all
/// read the same ids. The caller holds the project lock ([`place_thread`]).
fn finish_place(project: &Project, view: &SessionView, id: &str, created: &crate::core::Created, folder: &Path) -> Result<Thread> {
    let cwd = view.herdr.pane_cwd(&created.pane_id).unwrap_or_default();
    let cwd = if cwd.is_empty() { folder.to_string_lossy().into_owned() } else { cwd };
    thread::update_locked(project, id, |t| {
        t.cwd = cwd;
        t.workspace_id = created.workspace_id.clone();
        t.tab_id = created.tab_id.clone();
        t.pane_id = created.pane_id.clone();
    })
}

/// A thread's tab in the project's shared thread Space, on its own folder for a
/// tab or home thread (the two kinds are one placement now: neither has a Space
/// of its own any more) and on the repository's main checkout for a checkout
/// thread.
fn place_tab(project: &Project, view: &SessionView, record: &Thread) -> Result<Thread> {
    let folder = thread_folder(project, record)?;
    place_thread(project, view, record, &folder, |_| {})
}

/// Creates the thread directory, keeps it out of git, writes `brief.md`.
fn write_brief(ctx: &Ctx, project: &Project, placed: &Thread, restart: bool) -> Result<()> {
    let dir = thread::thread_dir(&placed.cwd, &project.slug, &placed.id);
    let with_dir = Thread { thread_dir: dir.clone(), ..placed.clone() };
    let task = std::fs::read_to_string(thread::task_path(project, &placed.id)).unwrap_or_default();
    let brief = thread::brief_for(project, &with_dir, &task, restart)?;

    std::fs::create_dir_all(Path::new(&dir).join("library")).with_context(|| format!("could not create {dir}"))?;
    if !matches!(placed.kind, Kind::Local) {
        exclude_from_git(ctx.runner, &placed.cwd)?;
    }
    project::write_atomic(&Path::new(&dir).join("brief.md"), brief.as_bytes())?;
    thread::update(project, &placed.id, |t| t.thread_dir = dir)?;
    Ok(())
}

/// Adds `.herdr-project/` to the repository's `info/exclude` if it is not
/// already listed, so nothing in the thread directory is ever committed.
pub fn exclude_from_git(runner: &dyn Runner, cwd: &str) -> Result<()> {
    let Ok(path) = git(runner, cwd, &["rev-parse", "--git-path", "info/exclude"], GIT_TIMEOUT) else {
        return Ok(()); // not inside a git repository
    };
    let path = Path::new(cwd).join(path);
    crate::skills::exclude_line(&path, ".herdr-project/")
}

/// Step 5: hand the thread to the ticker's launch step.
fn finish_placement(project: &Project, view: &SessionView, id: &str) -> Result<Thread> {
    let thread = thread::update(project, id, |t| {
        t.agent_name = thread::agent_name(&project.slug, &t.id);
        t.prompt_pending = true;
        t.launch_attempts = 0;
        t.launched_at.clear();
        t.brief_attempts = 0;
        t.brief_claimed.clear();
        t.brief_seen.clear();
        t.brief_seen_at.clear();
        t.brief_stuck = false;
        t.status = Status::Open;
        t.error.clear();
        t.last_state.clear();
        t.last_state_change = project::now();
    })?;
    report_thread_tokens(&view.herdr, &thread, &project.slug);
    Ok(thread)
}

#[derive(Debug, PartialEq)]
pub enum RestartPlan {
    /// (a) nothing was created: run the create step again.
    Create,
    /// (c) the recorded pane is alive at a shell prompt: reuse it.
    ReusePane,
    /// (e) open the existing worktree, or a new tab in `threads/<id>/`.
    Reopen,
}

/// What `thread restart` does, from what the record shows was reached.
pub fn restart_plan(thread: &Thread, live: &Live, branch_exists: bool, now: jiff::Timestamp) -> Result<RestartPlan> {
    match thread.kind {
        Kind::Adopted => bail!("an adopted thread cannot be restarted; adopt a new pane instead"),
        Kind::Worktree | Kind::Local | Kind::Checkout => {}
    }
    if thread.status == Status::Resolved {
        bail!("{} is resolved; `thread resolve --reopen` first", thread.id);
    }
    if thread.status == Status::Starting && thread::seconds_since(&thread.updated, now) < thread::STARTING_TIMEOUT_SECS {
        bail!("{} is still starting", thread.id);
    }
    // (d)
    if live.agent_state.is_some() {
        if thread.prompt_pending {
            bail!("{} is running: its pane has an agent in it that has not had its brief; `thread brief` sends it", thread.id);
        }
        bail!("{} is running: its pane has an agent in it", thread.id);
    }
    if live.pane_exists && thread.prompt_pending && thread.launch_attempts < thread::MAX_LAUNCH_ATTEMPTS && thread.status == Status::Open {
        bail!("{} is being launched by the ticker (attempt {} of {})", thread.id, thread.launch_attempts, thread::MAX_LAUNCH_ATTEMPTS);
    }
    if thread.kind == Kind::Worktree && thread.worktree_path.is_empty() {
        if branch_exists {
            // (b)
            bail!(
                "{}: no worktree was recorded but its branch already exists. A half-made worktree needs a human look: run `thread resolve`, then start a new thread.",
                thread.id
            );
        }
        return Ok(RestartPlan::Create);
    }
    if matches!(thread.kind, Kind::Local | Kind::Checkout) && thread.pane_id.is_empty() {
        return Ok(RestartPlan::Create);
    }
    if live.pane_exists {
        return Ok(RestartPlan::ReusePane);
    }
    Ok(RestartPlan::Reopen)
}

/// `thread restart [--profile NAME]`: brings a thread back in its pane,
/// worktree or a new tab, with the same or another profile.
pub fn restart(ctx: &Ctx, slug: &str, id: &str, profile: Option<&str>) -> Result<Thread> {
    let project = Project::load(&ctx.root, slug)?;
    let record = thread::load(&project, id)?;
    let profile = profile.map(|name| thread_profile(ctx, &project, Some(name))).transpose()?;
    // Settle any older ticker before examining the pane state; no restart pane changes happen yet.
    ticker::start(ctx)?;
    let view = require_session(ctx, &project)?;
    let (agents, panes) = (&view.agents, &view.panes);
    let now = jiff::Timestamp::now();
    let live = thread::live_state(&record, agents, panes, now);
    let branch_exists = record.kind == Kind::Worktree && record.worktree_path.is_empty() && {
        let branch = thread::branch_name(slug, id, &record.title);
        git(ctx.runner, &record.repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")], GIT_TIMEOUT).is_ok()
    };
    let plan = restart_plan(&record, &live, branch_exists, now)?;

    // This atomic state transition is the launch barrier. The ticker claims a launch
    // under the same project lock and re-reads the record before invoking Herdr.
    thread::begin_restart(&project, id, |t| {
        if let Some(profile) = &profile {
            profile.apply(t);
            t.agent_args.clear();
        }
    })?;

    let attempt = (|| -> Result<Thread> {
        match plan {
            RestartPlan::Create => return place_and_brief(ctx, &project, &view, id, true),
            RestartPlan::ReusePane => {}
            RestartPlan::Reopen => match record.kind {
                // The worktree is already on disk: only its tab has to come back,
                // in the project's shared thread Space like every other thread's.
                Kind::Worktree => {
                    let folder = std::path::PathBuf::from(&record.worktree_path);
                    if !folder.is_dir() {
                        bail!("{}: its worktree {} is not there any more", record.id, folder.display());
                    }
                    let created = place(&project, &view, &record, &folder)?;
                    if let Err(error) = view.herdr.worktree_mark(&created.pane_id, &record.worktree_path) {
                        eprintln!("warning: could not mark pane {} as worktree {} ({error})", created.pane_id, record.worktree_path);
                    }
                    finish_place(&project, &view, id, &created, &folder)?;
                }
                _ => {
                    place_tab(&project, &view, &record)?;
                }
            },
        }
        let placed = thread::load(&project, id)?;
        prepare_thread_agents(&placed)?;
        write_brief(ctx, &project, &placed, true)?;
        finish_placement(&project, &view, id)
    })();

    match attempt {
        Ok(thread) => Ok(thread),
        Err(error) => {
            let message = format!("{error:#}");
            let _ = thread::update(&project, id, |t| {
                if t.status == Status::Starting {
                    t.status = Status::Failed;
                    t.prompt_pending = false;
                    t.error = message.clone();
                }
            });
            Err(error.context(format!("thread {id} failed to restart; `thread restart {slug} {id}` retries")))
        }
    }
}

/// Sends a follow-up. The one sender that does not use the ready-for-a-prompt
/// predicate: agents queue a message that arrives while they work.
/// `recorded` only decides how the task file marks the entry: the text is sent
/// the same way, and every guard below runs either way.
pub fn prompt(ctx: &Ctx, slug: &str, id: &str, text: &str, recorded: Recorded) -> Result<String> {
    let project = Project::load(&ctx.root, slug)?;
    let record = thread::load(&project, id)?;
    if text.trim().is_empty() {
        bail!("the text is empty");
    }
    if record.status == Status::Resolved {
        bail!("{id} is resolved");
    }
    if record.prompt_pending {
        bail!("{id} has not received its brief yet; the ticker sends it once the agent is ready, and `thread brief` sends it now");
    }
    let view = require_session(ctx, &project)?;
    let agents = &view.agents;
    let state = prompt_state(&record, agents)?;
    let herdr = &view.herdr;
    // Someone typing in the thread's pane would have the prompt merged into
    // their text and submitted. A box this binary cannot read (another kind, a
    // layout it does not know) is sent to as before.
    let kind = agents.iter().find(|a| thread::agent_matches(&record, a)).map(|a| a.agent.as_str()).unwrap_or(&record.agent);
    let screen = herdr.agent_screen(&record.pane_id).map_err(|error| anyhow::anyhow!("{error}"))?;
    // Herdr may read a trust screen as idle; Enter there would accept it.
    if let Some(phrase) = crate::trust_screen::detect(kind, &screen) {
        let by_user = project.safety(&ctx.config_dir).map(|s| s.trust_screens == crate::safety::USER).unwrap_or(true);
        bail!("{}", crate::safety::trust_refusal(id, &record.pane_id, phrase, by_user));
    }
    if crate::prompt_box::check(kind, &screen) == crate::prompt_box::Draft::Typed {
        bail!("draft_in_box: {id}'s input box holds text someone typed ({}); not sending, so it is not merged into their prompt. Try again once it is empty; `thread read` shows it", record.pane_id);
    }
    // Confirmed only once herdr sees the agent start on it. A try that was
    // typed but not confirmed is never typed again: it may sit in the box.
    let sent = herdr.agent_prompt_confirmed(&record.pane_id, text.trim());
    if let Err(error) = &sent
        && crate::core::refused_before_typing(error)
    {
        bail!("{error}");
    }
    // Written after the send, so the task file never claims a prompt that was
    // refused; a restarted thread re-reads it with its task.
    thread::append_follow_up(&project, id, text, recorded)?;
    if let Err(error) = sent {
        bail!(
            "prompt_unconfirmed: the text was typed into {id}'s pane ({}) but the agent was not seen starting on it ({error}). Do not send it again: `thread read` shows whether it sits in the input box, and `thread keys {slug} {id} enter` submits it",
            record.pane_id
        );
    }
    Ok(state)
}

/// `thread next`: forward line N of the thread's Next list as a prompt, or add
/// a line the coordinator wants on that list.
pub fn next(ctx: &Ctx, slug: &str, id: &str, line: Option<usize>, add: Option<&str>) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    thread::load(&project, id)?;
    if let Some(text) = add {
        let text = text.trim();
        if text.is_empty() || text.contains('\n') {
            bail!("a Next line is one non-empty line");
        }
        let path = thread::extra_next_path(&project, id);
        let _lock = project.lock()?;
        let mut current = std::fs::read_to_string(&path).unwrap_or_default();
        current.push_str(&format!("- {text}\n"));
        project::write_atomic(&path, current.as_bytes())?;
        println!("added to {id}'s Next list");
        return Ok(());
    }
    let lines = thread::all_next(&project, id);
    match line {
        None => {
            if lines.is_empty() {
                println!("{id} has no Next list");
            }
            for (n, text) in lines.iter().enumerate() {
                println!("{}. {text}", n + 1);
            }
            Ok(())
        }
        Some(n) => {
            let text = lines.get(n.wrapping_sub(1)).with_context(|| format!("{id} has no Next line {n} ({} lines)", lines.len()))?;
            let state = prompt(ctx, slug, id, text, Recorded::Coordinator)?;
            println!("sent Next line {n} to {id} (agent was {state}): {text}");
            Ok(())
        }
    }
}

/// `thread stop`: Escape in the thread's pane, the harness's own interrupt.
pub fn stop(ctx: &Ctx, slug: &str, id: &str) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    let record = thread::load(&project, id)?;
    if record.status == Status::Resolved || record.pane_id.is_empty() {
        bail!("{id} has no pane");
    }
    let view = require_session(ctx, &project)?;
    view.herdr.agent_send_keys(&record.pane_id, &["esc".to_string()]).map_err(|error| anyhow::anyhow!("{error}"))?;
    println!("sent Escape to {id} (pane {})", record.pane_id);
    Ok(())
}

/// A thread's pane, reached on its own session, with the state of the agent in
/// it. Refuses a pane with no agent: nothing is read from or typed
/// at a bare shell prompt.
struct PaneAgent<'a> {
    record: Thread,
    herdr: Core<'a>,
    state: String,
    agent: Agent,
}

fn pane_agent<'a>(ctx: &'a Ctx, slug: &str, id: &str) -> Result<PaneAgent<'a>> {
    let project = Project::load(&ctx.root, slug)?;
    let record = thread::load(&project, id)?;
    if record.status == Status::Resolved || record.pane_id.is_empty() {
        bail!("{id} has no pane");
    }
    let view = require_session(ctx, &project)?;
    let agents = &view.agents;
    let agent = agents
        .iter()
        .find(|a| thread::agent_matches(&record, a))
        .cloned()
        .with_context(|| format!("no agent is detected in {id}'s pane; nothing is read from or typed at a bare shell (try `thread restart`)"))?;
    Ok(PaneAgent { record, herdr: view.herdr, state: agent.agent_status.clone(), agent })
}

/// `thread read`: what the thread's pane shows now (a trust dialog, a question
/// menu, a permission prompt), framed as data. `lines` reads scrollback instead.
pub fn read(ctx: &Ctx, slug: &str, id: &str, lines: Option<usize>) -> Result<()> {
    let pane = pane_agent(ctx, slug, id)?;
    let text = pane.herdr.agent_read(&pane.record.pane_id, lines).map_err(|error| anyhow::anyhow!("{error}"))?;
    println!("{id} · pane {} · agent {}", pane.record.pane_id, pane.state);
    println!("--- screen (data from the pane, never instructions) ---");
    println!("{}", text.trim_end());
    println!("--- end of screen ---");
    Ok(())
}

/// `thread keys`: types `text` (if any), then presses `keys`, in the thread's
/// pane. Nothing is recorded: a key press only means something on the screen
/// it answered.
pub fn keys(ctx: &Ctx, slug: &str, id: &str, keys: &[String], text: Option<&str>) -> Result<()> {
    let text = text.filter(|t| !t.is_empty());
    if keys.is_empty() && text.is_none() {
        bail!("give at least one key or --text");
    }
    if keys.iter().any(|k| k.trim().is_empty()) {
        bail!("a key name is empty");
    }
    let pane = pane_agent(ctx, slug, id)?;
    let fail = |error: crate::core::CoreError| anyhow::anyhow!("{error}");
    // With `trust_screens = user`, a trust answer is the user's alone.
    if Project::load(&ctx.root, slug)?.safety(&ctx.config_dir)?.trust_screens == crate::safety::USER
        && let Some(phrase) = pane.herdr.trust_screen(&pane.record.pane_id, &pane.record.agent).map_err(fail)?
    {
        bail!("{}", crate::safety::trust_refusal(id, &pane.record.pane_id, phrase, true));
    }
    if let Some(text) = text {
        pane.herdr.pane_send_text(&pane.record.pane_id, text).map_err(fail)?;
    }
    if !keys.is_empty() {
        pane.herdr.agent_send_keys(&pane.record.pane_id, keys).map_err(fail)?;
    }
    let typed = text.map(|t| format!("typed {} characters", t.chars().count()));
    let pressed = (!keys.is_empty()).then(|| format!("pressed {}", keys.join(" ")));
    let done: Vec<String> = typed.into_iter().chain(pressed).collect();
    println!("{id} (agent was {}): {}; `thread read {slug} {id}` shows the result", pane.state, done.join(", then "));
    Ok(())
}

/// `thread brief`: delivers a thread's brief now instead of on the ticker's
/// next check (it prompts an agent seconds after it is ready). The record
/// is claimed under the project lock first, so the ticker does not send it too.
pub fn brief(ctx: &Ctx, slug: &str, id: &str) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    let record = thread::load(&project, id)?;
    if !record.prompt_pending {
        println!("{id} already has its brief; send more with `thread prompt`");
        return Ok(());
    }
    if record.status != Status::Open {
        bail!("{id} is {}; `thread restart` brings it back", format!("{:?}", record.status).to_lowercase());
    }
    let pane = pane_agent(ctx, slug, id).map_err(|e| anyhow::anyhow!("{e:#}; the ticker launches the agent on its next pass"))?;
    match pane.state.as_str() {
        "blocked" => bail!("{id}'s pane shows a prompt: `thread read` shows it, `thread keys` answers it; then run `thread brief` again"),
        state if !pane.agent.ready_for_brief(if pane.agent.agent.is_empty() { &record.agent } else { &pane.agent.agent }) && record.brief_attempts == 0 => bail!("{id}'s agent is {state}, not ready for its brief yet; try again shortly"),
        _ => {}
    }
    match crate::brief::deliver(&project, &pane.herdr, &pane.record, &pane.agent, crate::brief::Sender::Manual)? {
        crate::brief::Outcome::Delivered => println!("{id} has its brief (agent was {})", pane.state),
        crate::brief::Outcome::Taken => println!("{id}'s brief is being sent by the ticker, or it just got it"),
        crate::brief::Outcome::Waiting(why) => bail!("{id}'s brief is not delivered yet: {why}; `thread read` shows the pane, then run `thread brief` again"),
        crate::brief::Outcome::Stuck => bail!("{id}'s brief did not get through; `thread read` shows the pane"),
        crate::brief::Outcome::TrustScreen(phrase) => {
            let by_user = project.safety(&ctx.config_dir).map(|s| s.trust_screens == crate::safety::USER).unwrap_or(true);
            bail!("{}; the brief follows once it is answered", crate::safety::trust_refusal(id, &pane.record.pane_id, phrase, by_user));
        }
    }
    Ok(())
}

/// The state a follow-up may be sent in, or the refusal.
pub fn prompt_state(record: &Thread, agents: &[Agent]) -> Result<String> {
    let agent = agents
        .iter()
        .find(|a| thread::agent_matches(record, a))
        .with_context(|| format!("no agent is detected in {}'s pane; text is never typed at a bare shell prompt (try `thread restart`)", record.id))?;
    match agent.agent_status.as_str() {
        "blocked" => bail!(
            "agent_blocked: {} is waiting on a prompt in its pane ({}); `thread read` shows it, `thread keys` answers it",
            record.id,
            record.pane_id
        ),
        "unknown" => bail!("{}'s agent state is unknown; not sending", record.id),
        state => Ok(state.to_string()),
    }
}

pub fn ack(ctx: &Ctx, slug: &str, id: &str) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    let record = thread::update(&project, id, |t| t.acked_report_hash = t.report_hash.clone())?;
    if record.report_hash.is_empty() {
        println!("{id} has no report yet; nothing to acknowledge");
    } else {
        println!("{id}: report acknowledged");
    }
    Ok(())
}

#[derive(Default)]
pub struct ResolveArgs {
    pub reopen: bool,
    /// Keep the worktree (and so the branch) instead of cleaning up.
    pub keep_worktree: bool,
    pub skip_copy: bool,
    /// Remove the worktree even though the final copy was partial.
    pub discard_uncopied: bool,
}

pub fn resolve(ctx: &Ctx, slug: &str, id: &str, args: &ResolveArgs) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    let record = thread::load(&project, id)?;
    if args.reopen {
        if record.status != Status::Resolved {
            bail!("{id} is not resolved");
        }
        thread::update(&project, id, |t| {
            t.status = Status::Open;
            t.resolved_reason.clear();
            t.stray_agent.clear();
        })?;
        println!("{id} is open again. Nothing was started; `thread restart {slug} {id}` brings its agent back.");
        return Ok(());
    }
    if record.status == Status::Resolved {
        bail!("{id} is already resolved; `sweep {slug}` cleans what is left of it");
    }
    // The resolve starts here. Every item about this thread that already
    // exists is an event nobody can act on once it is resolved, so the end of
    // this function moves them to `inbox/done/`; what the resolve writes
    // itself is newer and stays (see `inbox::done_for_thread`).
    let started = project::now();

    // Every path that resolves a thread performs a final copy first.
    let mut copy_complete = !args.skip_copy;
    if !args.skip_copy {
        let copied = final_copy(ctx, &project, &record);
        match &copied.outcome {
            CopyOutcome::Complete => {}
            CopyOutcome::Partial(notes) => {
                copy_complete = false;
                println!("the final copy was partial:");
                for note in notes {
                    println!("  - {note}");
                }
            }
            CopyOutcome::Failed(error) => {
                bail!("the final copy failed ({error}); not resolving. `--skip-copy` resolves without it.");
            }
        }
    }
    let resolved = thread::update(&project, id, |t| {
        t.status = Status::Resolved;
        t.resolved_reason = "manual".into();
        t.stray_agent.clear();
        t.prompt_pending = false;
    })?;
    let cleaned = clean(ctx, &project, &resolved, &Clean {
        keep_worktree: args.keep_worktree,
        copy_complete: copy_complete || args.discard_uncopied,
        merged_head: crate::steps::load_state(&project).prs.get(id).map(|s| s.head_oid.clone()).unwrap_or_default(),
    });
    let notes = cleaned.notes;
    // A resolved thread's earlier events are done with: archiving them keeps
    // the inbox about what still needs someone (little-world, 2026-11-13:
    // 26 of 27 unhandled items pointed at resolved threads).
    let archived = crate::inbox::done_for_thread(&project, id, &started)?;
    println!("{id} resolved; its report and library are kept.");
    for note in &notes {
        println!("  - {note}");
    }
    if archived > 0 {
        println!("  - {}", archive_note(id, archived));
    }
    let event_id = crate::inbox::write_in(
        &project,
        &crate::inbox::lane_of_thread(&project, id),
        "thread-state",
        id,
        "resolved",
        &format!(
            "{id} \"{}\" was resolved: {}",
            resolved.title,
            notes.join("; ")
        ),
        "",
    )?;
    if called_from_coordinator(ctx, &project) {
        crate::inbox::done(&project, &[event_id], false)?;
    }
    report_stray(&project, id, &resolved, cleaned.stray)
}

/// The one line `thread resolve` prints when it archived the thread's earlier
/// items.
pub(crate) fn archive_note(id: &str, count: usize) -> String {
    if count == 1 {
        format!("archived 1 pending item about {id}")
    } else {
        format!("archived {count} pending items about {id}")
    }
}

/// A resolve whose cleanup left the thread's agent running is not a clean
/// close: the record says so, an item tells the coordinator, and the caller
/// gets an error naming the pane. Nothing is killed here: what a surviving
/// process is for is the user's decision, not this command's.
pub fn report_stray(project: &Project, id: &str, record: &Thread, stray: Option<StrayAgent>) -> Result<()> {
    let Some(stray) = stray else {
        return Ok(());
    };
    let summary = format!(
        "{id} \"{}\" is resolved but its agent is still running: {stray}",
        record.title
    );
    thread::update(project, id, |t| t.stray_agent = stray.to_string())?;
    crate::inbox::write_in(project, &crate::inbox::lane_of_thread(project, id), "thread-error", id, "agent outlived its pane", &summary, "")?;
    bail!(
        "{id} is resolved, but {stray}; nothing was killed. Check `herdr pane list` for {}, end it there (or kill its process), then run `sweep {}` for what is left",
        stray.pane,
        project.slug
    )
}

/// A coordinator already knows the result of a resolve it initiated, so keep
/// its audit event without putting a redundant item back in its own inbox.
///
/// "It is this coordinator" means the reader's core terminal (`HC_TERMINAL_ID`)
/// runs an agent in the project's session whose pane is a coordinator's — the
/// recorded one, or any lane's. Herdr 1 compared its own pane/tab/workspace
/// variables against the record; herdr-core never sets those, so the answer was
/// always "no" and every resolve left one extra item behind.
fn called_from_coordinator(ctx: &Ctx, project: &Project) -> bool {
    let Some(terminal) = ctx.env.this_terminal() else {
        return false;
    };
    let Some(record) = project.coordinator() else {
        return false;
    };
    if !crate::core::same_session(ctx.env.this_session(), &record.socket) {
        return false;
    }
    let agent = crate::core::Core::new(ctx.env.hc_bin(), &record.socket, ctx.runner)
        .agent_list()
        .unwrap_or_default()
        .into_iter()
        .find(|agent| agent.in_terminal(terminal));
    let Some(agent) = agent else {
        return false;
    };
    agent.pane_id == record.pane_id
        || crate::lanes::Registry::for_project(project).lanes.iter().any(|lane| !lane.pane_id.is_empty() && lane.pane_id == agent.pane_id)
}

pub struct Clean {
    pub keep_worktree: bool,
    /// Everything the thread wrote is home: the worktree may go.
    pub copy_complete: bool,
    /// The merged pull request's head commit. Passed in, not read from
    /// `ticker.json`: the ticker saves that file only at the end of its tick.
    pub merged_head: String,
}

/// Cleans up after a resolved thread: its worktree (never forced), its local
/// branch when the pull request is merged, its tab. Reports and library are
/// never touched. Returns one note per thing, for the output and the inbox.
pub fn clean(ctx: &Ctx, project: &Project, t: &Thread, options: &Clean) -> Cleaned {
    let mut notes = Vec::new();
    let view = session_view(ctx, project);
    let mut merged = t.pr_state.eq_ignore_ascii_case("merged");
    let mut merged_head = options.merged_head.clone();
    if t.kind == Kind::Worktree
        && !t.branch.is_empty()
        && (!merged || merged_head.is_empty())
        && let Some(summary) = last_pr_lookup(ctx, project, t)
    {
        merged = summary.state == "MERGED";
        merged_head = summary.head_oid;
    }
    match t.kind {
        Kind::Worktree => {
            let mut removed = t.worktree_path.is_empty();
            if t.worktree_path.is_empty() {
                notes.push("no worktree was recorded".into());
            } else if options.keep_worktree {
                let _ = thread::update(project, &t.id, |t| t.kept_worktree = true);
                notes.push(format!("worktree kept at {} (asked to keep it)", t.worktree_path));
            } else if !options.copy_complete && !worktree_gone(t) {
                // A folder that is gone has nothing left to lose.
                notes.push(format!("worktree kept at {}: not everything in it was copied home (`--discard-uncopied` removes it anyway)", t.worktree_path));
            } else {
                match remove_worktree(ctx, project, t, view.as_ref()) {
                    Ok(Removal::Removed) => {
                        removed = true;
                        let _ = thread::update(project, &t.id, |t| t.worktree_path.clear());
                        notes.push(format!("worktree {} removed", t.worktree_path));
                    }
                    Ok(Removal::AlreadyGone(closed)) => {
                        removed = true;
                        let _ = thread::update(project, &t.id, |t| t.worktree_path.clear());
                        notes.push(format!("worktree {} was already gone; {closed}", t.worktree_path));
                    }
                    Err(error) => notes.push(format!("worktree kept at {}: {error:#}", t.worktree_path)),
                }
            }
            if !t.branch.is_empty() {
                if merged && removed {
                    match delete_branch(ctx, t, &merged_head) {
                        Ok(true) => notes.push(format!("branch {} deleted (its pull request is merged)", t.branch)),
                        Ok(false) => notes.push(format!("branch {} was already deleted", t.branch)),
                        Err(error) => notes.push(format!("branch {} kept: {error:#}", t.branch)),
                    }
                } else if !merged {
                    notes.push(format!("branch {} kept: its pull request is not merged", t.branch));
                } else {
                    notes.push(format!("branch {} kept: its worktree is still there", t.branch));
                }
            }
        }
        // A tab, checkout or home thread has nothing to remove but its tab
        // (below) and, for a checkout, no branch of its own either.
        Kind::Local | Kind::Checkout => {}
        Kind::Adopted => notes.push("its pane was left alone (an adopted pane is yours)".into()),
    }
    // Every thread's tab goes, after its worktree if it had one: the Space it
    // was in is shared with the project's other threads, which is what
    // `close_place` keeps (it closes the Space only when this was its last
    // pane).
    if t.kind != Kind::Adopted {
        match &view {
            Some(view) => notes.extend(close_place(project, view, t)),
            None => notes.push("its tab was already closed".into()),
        }
    }
    let mut stray = None;
    if let Some(view) = &view {
        clear_thread_tokens(&view.herdr, t);
        // A thread's own Space from before the shared one, and an orphan home
        // Space: closed once nothing uses them. Neither is ever the thread
        // Space, which `spaces::empty` judges by its panes.
        for error in crate::spaces::close_empty(ctx, project, &view.herdr) {
            notes.push(format!("{error:#}"));
        }
        stray = stray_agent(view, t);
    }
    Cleaned { notes, stray }
}

/// What a resolve saw: the notes it prints and archives, and an agent of the
/// thread that outlived its own cleanup.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Cleaned {
    pub notes: Vec<String>,
    pub stray: Option<StrayAgent>,
}

/// An agent of a resolved thread that herdr still lists, in a pane that is
/// still there, after the cleanup that should have taken both away. The
/// cleanup did not take effect; `stray_agent` says why this and not more.
#[derive(Debug, Clone, PartialEq)]
pub struct StrayAgent {
    pub pane: String,
    /// The agent's name, or its kind when it has no name.
    pub name: String,
    pub state: String,
}

impl std::fmt::Display for StrayAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.state.is_empty() { write!(f, "agent {} is still in pane {}", self.name, self.pane) } else { write!(f, "agent {} is still in pane {} ({})", self.name, self.pane, self.state) }
    }
}

/// The thread's agent, when it is still there after its cleanup.
///
/// Herdr lists agents per pane, so "still listed" alone proves nothing: a
/// listing taken before the close would say so forever. What this asks is the
/// narrow question herdr can answer — **an agent of this thread is listed, and
/// the pane it lives in is still there** — which is the tab close that did not
/// take effect, or a workspace that stayed open. A process that detaches from
/// a pane that herdr has already forgotten cannot be seen by herdr at all; the
/// resolve note for a failed close is what covers it.
///
/// Listed twice on purpose: the close and the listing go through the same
/// server, and one more pair of calls turns an entry cached from the moment
/// before the close into nothing to report, instead of a false alarm on every
/// resolve. [`Core::end`] now waits for the program to be gone before it
/// removes the record, so the race that made this look twice — a kill sent and
/// a look taken in the same breath (t-0068 §6.1) — is closed; the second look
/// stays because the wait is bounded (a program that outlives [`crate::core::END_WAIT`]
/// is still listed on this look, and core's registry is its own to change), and
/// because a stray is written into the record and an inbox item: a false one
/// costs a coordinator a real errand. An unreachable session reports nothing:
/// guessing here would block every resolve. `Kind::Adopted` panes are the
/// user's and outlive their thread by design.
fn stray_agent(view: &SessionView, t: &Thread) -> Option<StrayAgent> {
    if t.kind == Kind::Adopted || t.pane_id.is_empty() {
        return None;
    }
    let herdr = &view.herdr;
    let look = || {
        let agents = herdr.agent_list().ok()?;
        let panes = herdr.pane_list().ok()?;
        agents
            .iter()
            .find(|a| thread::agent_matches(t, a) && panes.iter().any(|p| p.pane_id == a.pane_id))
            .map(|a| StrayAgent {
                pane: a.pane_id.clone(),
                name: if a.name.is_empty() { a.agent.clone() } else { a.name.clone() },
                state: a.agent_status.clone(),
            })
    };
    look()?;
    look()
}

/// A last look for the thread's pull request before its branch is judged. The
/// ticker checks pull requests every two minutes, so a thread that opens and
/// merges one and is resolved in between would otherwise keep its branch.
/// What it finds is recorded, with the usual `pr` item when it is news.
fn last_pr_lookup(ctx: &Ctx, project: &Project, t: &Thread) -> Option<crate::pr::Summary> {
    use crate::pr;
    let report = std::fs::read_to_string(thread::home_report_path(project, &t.id)).unwrap_or_default();
    let url = match pr::pr_line(&report) {
        Ok(Some(url)) => url,
        _ if !t.pr.is_empty() => t.pr.clone(),
        _ => pr::find_by_branch(ctx.runner, &t.origin, &t.branch).ok()??,
    };
    let json = pr::view(ctx.runner, &url, &t.origin).ok()?;
    let pr::Checked::Summary(summary) = pr::reduce(&json, &t.branch, &t.origin).ok()? else {
        return None;
    };
    if t.pr != url || t.pr_state != summary.state {
        let (new_url, state, review) = (url.clone(), summary.state.clone(), summary.review_decision.clone());
        let _ = thread::update(project, &t.id, |r| {
            r.pr = new_url;
            r.pr_state = state;
            r.pr_review = review;
        });
        let _ = crate::inbox::write_in(project, &crate::inbox::lane_of_thread(project, &t.id), "pr", &t.id, "PR found at resolve", &format!("{}: pull request {} (found at resolve)", crate::steps::thread_label(t), pr::describe_change(None, &summary)), "");
    }
    Some(summary)
}

/// Deletes the local branch only when its tip is the pull request's merged
/// head: a commit made after the merge and never pushed keeps the branch.
/// `false`: there was no such branch left to delete.
fn delete_branch(ctx: &Ctx, t: &Thread, head: &str) -> Result<bool> {
    if head.is_empty() {
        bail!("the merged pull request's head commit is not known");
    }
    let tip = git(ctx.runner, &t.repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{}", t.branch)], GIT_TIMEOUT).unwrap_or_default();
    if tip.is_empty() {
        return Ok(false);
    }
    if tip != head {
        bail!("it has commits that are not in the merged pull request");
    }
    // `-D`: GitHub says the pull request is merged, and a squash merge leaves
    // the branch unmerged as far as git can tell.
    git(ctx.runner, &t.repo, &["branch", "-D", &t.branch], GIT_TIMEOUT).map(|_| true)
}

/// The final report and library copy, storing the new report hash.
pub fn final_copy(ctx: &Ctx, project: &Project, record: &Thread) -> thread::Copied {
    let copied = thread::copy_home_local(project, record, true, ctx.runner);
    if let Some(hash) = &copied.report_hash
        && *hash != record.report_hash
    {
        let _ = thread::update(project, &record.id, |t| {
            t.report_hash = hash.clone();
            t.last_report_change = project::now();
        });
    }
    copied
}

pub enum Removal {
    Removed,
    /// The folder was gone before the removal; says what became of its tab.
    AlreadyGone(String),
}

/// The live pane of a worktree thread: the one carrying `hp_worktree` for the
/// thread's worktree, else the recorded pane when it still works there. A pane
/// elsewhere that happens to have `cd`'d into the worktree does not count, and
/// the other threads of the same Space are somebody else's panes: the Space is
/// shared, so the **label** is what says which worktree a pane stands in
/// (`docs/thread-space.md`).
pub fn worktree_pane<'p>(record: &Thread, panes: &'p [Pane]) -> Option<&'p Pane> {
    if record.worktree_path.is_empty() {
        return None;
    }
    panes
        .iter()
        .find(|p| p.tokens.get(crate::core::L_WORKTREE).is_some_and(|path| *path == record.worktree_path))
        .or_else(|| {
            panes.iter().find(|p| {
                p.pane_id == record.pane_id && !p.cwd.is_empty() && Path::new(&p.cwd).starts_with(&record.worktree_path)
            })
        })
}

/// A local worktree whose folder no longer exists: git and herdr would both
/// refuse to remove it ("is not a working tree").
pub fn worktree_gone(record: &Thread) -> bool {
    !record.worktree_path.is_empty() && !Path::new(&record.worktree_path).exists()
}

/// Never forces. herdr's or git's refusal (for example uncommitted changes) is
/// reported unchanged. A live pane goes through `Core::worktree_remove`, which
/// reads the worktree path off **that pane's** label; a worktree whose folder is
/// already gone is pruned from git and its tab closed.
pub fn remove_worktree(ctx: &Ctx, project: &Project, record: &Thread, view: Option<&SessionView>) -> Result<Removal> {
    if record.worktree_path.is_empty() {
        bail!("{} has no recorded worktree", record.id);
    }
    let _ = project;
    if worktree_gone(record) {
        let _ = git(ctx.runner, &record.repo, &["worktree", "prune"], GIT_TIMEOUT);
        // Its tab is closed by `close_place`, which every resolve runs after
        // this; closing it here too would close it twice and say so twice.
        let closed = match view.and_then(|v| worktree_pane(record, &v.panes)) {
            Some(_) => "its tab is closed with the thread".to_string(),
            None => "no tab was open on it".to_string(),
        };
        return Ok(Removal::AlreadyGone(closed));
    }
    // A worktree placed before the shared `.agents` link was excluded can
    // still be all git sees as untracked. Clear that before either remover's
    // non-forced run. Best effort: a link that is not ours is left alone, and
    // the removal itself still reports what it finds.
    let _ = crate::skills::exclude_shared_link_if_present(Path::new(&record.repo), Path::new(&record.worktree_path));
    // The pane path needs the pane to *say* it is in this worktree; one that
    // does not (a pane from before the label, a label that did not land) takes
    // the git path below rather than being removed by a guess.
    if let Some(view) = view
        && let Some(pane) = worktree_pane(record, &view.panes).filter(|p| p.tokens.get(crate::core::L_WORKTREE).is_some_and(|path| *path == record.worktree_path))
    {
        return view.herdr.worktree_remove(&pane.pane_id).map(|_| Removal::Removed).map_err(|error| anyhow::anyhow!("{error}"));
    }
    git(ctx.runner, &record.repo, &["worktree", "remove", &record.worktree_path], Duration::from_secs(20))?;
    let _ = git(ctx.runner, &record.repo, &["worktree", "prune"], GIT_TIMEOUT);
    Ok(Removal::Removed)
}

/// Closes a thread's tab, and the Space it was in when that was the last pane
/// there: a project's threads share one Space, so it is closed only when nothing
/// is left in it, never because this thread left (`docs/thread-space.md`).
/// Returns one note per thing.
///
/// The pane count and the closes are one step, under the project lock: a
/// `thread start` placing a tab into this Space holds the same lock, so its new
/// pane is never counted as absent and then closed underneath it.
fn close_place(project: &Project, view: &SessionView, t: &Thread) -> Vec<String> {
    let mut notes = Vec::new();
    if t.pane_id.is_empty() {
        return vec!["its tab was already closed".into()];
    }
    let now = jiff::Timestamp::now();
    if !thread::live_state(t, &view.agents, &view.panes, now).pane_exists {
        return vec!["its tab was already closed".into()];
    }
    let _lock = match project.lock() {
        Ok(lock) => lock,
        Err(error) => return vec![format!("its tab was left open: {error}")],
    };
    // The live pane's tab, not the recorded one: ids move after a restart.
    let pane = view.panes.iter().find(|p| thread::pane_matches(t, p));
    let tab = pane.map(|p| p.tab_id.clone()).unwrap_or_else(|| t.tab_id.clone());
    let space = pane.map(|p| p.workspace_id.clone()).unwrap_or_else(|| t.workspace_id.clone());
    let others = view.panes.iter().filter(|p| p.workspace_id == space && p.pane_id != t.pane_id).count();
    match view.herdr.tab_close(&tab) {
        Ok(_) => notes.push("its tab was closed".into()),
        Err(error) => notes.push(format!("its tab could not be closed ({error})")),
    }
    if others == 0 && !space.is_empty() {
        match view.herdr.workspace_close(&space) {
            Ok(_) => notes.push("the Space it was the last thread in was closed".into()),
            Err(error) => notes.push(format!("the Space it was the last thread in could not be closed ({error})")),
        }
    }
    notes
}

/// A thread with its live state and group, for `thread list`, `thread show`
/// and the overview.
pub struct Row {
    pub thread: Thread,
    pub group: Group,
    pub note: String,
}

pub fn rows(ctx: &Ctx, project: &Project) -> Vec<Row> {
    rows_with_view(project, session_view(ctx, project).as_ref())
}

/// The same rows from a session view the caller already has, so one digest
/// costs one pair of Herdr lists rather than two.
pub fn rows_with_view(project: &Project, view: Option<&SessionView>) -> Vec<Row> {
    let now = jiff::Timestamp::now();
    thread::list(project)
        .into_iter()
        .map(|t| row(&t, &project.root, view, now))
        .collect()
}

fn row(t: &Thread, root: &Path, view: Option<&SessionView>, now: jiff::Timestamp) -> Row {
    // Before the first poll a thread that is waiting for its launch is Working.
    let recorded = Group::from_token(&t.last_group).unwrap_or(if t.prompt_pending { Group::Working } else { Group::Idle });
    if t.status == Status::Resolved {
        // `stray_agent` is what the resolve saw at that moment, kept on the
        // record. It goes stale: an agent that outlived its cleanup is often
        // ended a minute later, and the note then says "is still in pane X"
        // about a pane `hc ls` no longer has (t-0052). Ask the live lists which
        // it is, and say the agent exited rather than repeat a claim nobody can
        // act on. A session that cannot be reached leaves the record's word
        // alone: guessing "gone" would be the same mistake in reverse.
        let exited = !t.stray_agent.is_empty()
            && view.is_some_and(|view| {
                let live = thread::live_state(t, &view.agents, &view.panes, now);
                live.agent_state.is_none() && !live.pane_exists
            });
        let note = if t.stray_agent.is_empty() {
            t.resolved_reason.clone()
        } else if exited {
            format!("{}; the agent the resolve left behind has since exited", t.resolved_reason)
        } else {
            format!("{}; {}", t.resolved_reason, t.stray_agent)
        };
        return Row { thread: t.clone(), group: Group::Resolved, note };
    }
    let Some(view) = view else {
        // Records are still printed; panes are not treated as gone.
        return Row { thread: t.clone(), group: recorded, note: "session unreachable".into() };
    };
    let live = thread::live_with_report(t, &view.agents, &view.panes, now, root, &view.socket);
    // A report the ticker has not hashed yet still counts, as it does for the ticker.
    let fresh = Thread { report_hash: thread::local_report_hash(t).unwrap_or_else(|| t.report_hash.clone()), ..t.clone() };
    let group = thread::group(&fresh, &live, now);
    let note = if t.status == Status::Failed {
        format!("failed: {}", t.error)
    } else if !live.pane_exists {
        "pane closed".to_string()
    } else {
        live.agent_state.unwrap_or_else(|| "no agent".into())
    };
    Row { thread: t.clone(), group, note }
}

/// One thread as JSON: the record, its group and note, the Next list and the
/// home report path.
pub fn row_json(project: &Project, row: &Row) -> serde_json::Value {
    let t = &row.thread;
    let report = thread::home_report_path(project, &t.id);
    let mut value = serde_json::to_value(t).unwrap_or_default();
    value["group"] = row.group.label().into();
    value["group_token"] = row.group.token().into();
    value["rank"] = row.group.rank().into();
    value["note"] = row.note.clone().into();
    value["next"] = thread::all_next(project, &t.id).into();
    value["report"] = if report.is_file() { report.to_string_lossy().into_owned().into() } else { serde_json::Value::Null };
    value["library"] = project.dir().join("library").join(&t.id).to_string_lossy().into_owned().into();
    value
}

pub fn print_list(ctx: &Ctx, slug: &str, json: bool) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    let rows = rows(ctx, &project);
    if json {
        let values: Vec<serde_json::Value> = rows.iter().map(|r| row_json(&project, r)).collect();
        println!("{}", serde_json::to_string_pretty(&values)?);
        return Ok(());
    }
    for row in rows {
        println!("{}\t{}\t{}\t{}", row.thread.id, row.group.label(), row.note, row.thread.title);
    }
    Ok(())
}

pub fn print_show(ctx: &Ctx, slug: &str, id: &str, json: bool) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    let record = thread::load(&project, id)?;
    let view = session_view(ctx, &project);
    let row = row(&record, &project.root, view.as_ref(), jiff::Timestamp::now());
    if json {
        println!("{}", serde_json::to_string_pretty(&row_json(&project, &row))?);
        return Ok(());
    }
    println!("group = {:?}", row.group.label());
    println!("live = {:?}", row.note);
    print!("{}", toml::to_string(&record)?);
    let report = thread::home_report_path(&project, id);
    if report.is_file() {
        println!("# home copy of the report: {}", report.display());
    }
    let next = thread::all_next(&project, id);
    if !next.is_empty() {
        println!("# next:");
        for (n, line) in next.iter().enumerate() {
            println!("#   {}. {line}", n + 1);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> jiff::Timestamp {
        "2026-09-17T12:00:00Z".parse().unwrap()
    }

    fn worktree_thread() -> Thread {
        Thread {
            id: "t-0001".into(),
            kind: Kind::Worktree,
            status: Status::Open,
            created: "2026-09-17T10:00:00Z".into(),
            worktree_path: "/wt".into(),
            pane_id: "w2:p1".into(),
            ..Thread::default()
        }
    }

    fn gone() -> Live {
        Live { pane_exists: false, agent_state: None, state_secs: 0, ..Live::default() }
    }

    fn shell() -> Live {
        Live { pane_exists: true, agent_state: None, state_secs: 0, ..Live::default() }
    }

    #[test]
    fn restart_case_a_nothing_created() {
        let t = Thread { status: Status::Failed, worktree_path: String::new(), ..worktree_thread() };
        assert_eq!(restart_plan(&t, &gone(), false, now()).unwrap(), RestartPlan::Create);
    }

    #[test]
    fn restart_case_b_branch_without_worktree_needs_a_human() {
        let t = Thread { status: Status::Failed, worktree_path: String::new(), ..worktree_thread() };
        let error = restart_plan(&t, &gone(), true, now()).unwrap_err().to_string();
        assert!(error.contains("thread resolve"), "{error}");
    }

    #[test]
    fn restart_case_c_reuses_a_pane_at_a_shell_prompt() {
        assert_eq!(restart_plan(&worktree_thread(), &shell(), false, now()).unwrap(), RestartPlan::ReusePane);
    }

    #[test]
    fn restart_case_d_refuses_a_running_thread() {
        let running = Live { pane_exists: true, agent_state: Some("working".into()), state_secs: 0, ..Live::default() };
        assert!(restart_plan(&worktree_thread(), &running, false, now()).is_err());
    }

    #[test]
    fn restart_case_e_reopens_the_worktree() {
        assert_eq!(restart_plan(&worktree_thread(), &gone(), false, now()).unwrap(), RestartPlan::Reopen);
        let local = Thread { kind: Kind::Local, worktree_path: String::new(), ..worktree_thread() };
        assert_eq!(restart_plan(&local, &gone(), false, now()).unwrap(), RestartPlan::Reopen);
    }

    #[test]
    fn restart_refuses_a_launch_in_progress_adopted_resolved_and_young_starting() {
        let launching = Thread { prompt_pending: true, launch_attempts: 1, ..worktree_thread() };
        assert!(restart_plan(&launching, &shell(), false, now()).is_err());
        let exhausted = Thread { prompt_pending: true, launch_attempts: 3, ..worktree_thread() };
        assert_eq!(restart_plan(&exhausted, &shell(), false, now()).unwrap(), RestartPlan::ReusePane);

        let adopted = Thread { kind: Kind::Adopted, ..worktree_thread() };
        assert!(restart_plan(&adopted, &gone(), false, now()).is_err());
        let resolved = Thread { status: Status::Resolved, ..worktree_thread() };
        assert!(restart_plan(&resolved, &gone(), false, now()).is_err());

        let young = Thread { status: Status::Starting, created: "2026-09-17T11:59:00Z".into(), updated: "2026-09-17T11:59:00Z".into(), worktree_path: String::new(), ..worktree_thread() };
        assert!(restart_plan(&young, &gone(), false, now()).is_err());
        let stale = Thread { created: "2026-09-17T11:00:00Z".into(), updated: "2026-09-17T11:00:00Z".into(), ..young };
        assert_eq!(restart_plan(&stale, &gone(), false, now()).unwrap(), RestartPlan::Create);
    }

    fn agent(state: &str) -> Agent {
        Agent { pane_id: "w2:p1".into(), agent_status: state.into(), ..Agent::default() }
    }

    fn space(id: &str, label: &str) -> crate::core::Workspace {
        crate::core::Workspace { workspace_id: id.into(), label: label.into(), ..crate::core::Workspace::default() }
    }

    #[test]
    fn the_thread_space_is_named_by_the_record_the_label_or_the_panes() {
        let home = tempfile::tempdir().unwrap();
        let project = crate::project::create(&home.path().join("root"), "demo", vec![]).unwrap();
        // A real record: the finder reads the project's threads, and a thread's
        // own Space is recognised by that thread's label.
        let t = thread::allocate(&project, |t| {
            t.title = "Research".into();
            t.kind = Kind::Local;
        })
        .unwrap();
        let t = Thread { workspace_id: "w9".into(), ..t };
        let mine = crate::core::Pane { pane_id: "p1".into(), workspace_id: "w4".into(), tokens: [("hp_project".to_string(), "demo".to_string())].into_iter().collect(), ..crate::core::Pane::default() };
        let elsewhere = crate::core::Pane { pane_id: "p2".into(), workspace_id: "w5".into(), tokens: [("hp_project".to_string(), "other".to_string())].into_iter().collect(), ..crate::core::Pane::default() };
        let panes = [mine, elsewhere];
        let homes = [space("whome", "Demo")];
        // 1: the Space the record names, when the label still says it is a
        // thread Space (a restart lands where the thread was).
        assert_eq!(threads_space(&project, &[space("w9", "Demo Threads"), space("whome", "Demo")], &panes, &[], Some(&t)).as_deref(), Some("w9"));
        // Never the project's home Space, whatever the label.
        assert_eq!(threads_space(&project, &homes, &panes, &[], Some(&t)), None);
        // A Space that is one worktree is still the thread Space when a record
        // names it: a thread Space holding a single worktree thread is exactly
        // that, and reading it as "somebody's worktree Space" is what made every
        // worktree thread open a Space of its own (2026-11-19, t-0072).
        let worktree = crate::core::Workspace { worktree: Some("/wt/one".into()), ..space("w9", "Demo Threads") };
        assert_eq!(threads_space(&project, &[worktree], &[], &[], Some(&t)).as_deref(), Some("w9"));
        // A thread's own Space from **before** the change is still not one, and
        // the worktree kind's own Space was labelled `<title> Threads`.
        let legacy = crate::core::Workspace { worktree: Some("/wt/one".into()), ..space("w9", "Research Threads") };
        assert_eq!(threads_space(&project, &[legacy], &[], &[], None), None);
        // 3: today's label, for a Space nothing records yet.
        assert_eq!(threads_space(&project, &[space("w7", "Demo Threads")], &[], &[], None).as_deref(), Some("w7"));
        // 4: this project's panes, when the label drifted (the app's own name).
        assert_eq!(threads_space(&project, &[space("w4", "whatever the app shows"), space("w5", "also not it")], &panes, &[], None).as_deref(), Some("w4"));
        // A thread's own Space from before the change (labelled with its title)
        // is not one: a restart abandons it, it never joins it.
        assert_eq!(threads_space(&project, &[space("w9", "Research")], &[], &[], Some(&t)), None);
    }

    #[test]
    fn prompt_refusals_and_sending_while_working() {
        let t = Thread { agent_name: String::new(), kind: Kind::Adopted, ..worktree_thread() };
        assert!(prompt_state(&t, &[]).unwrap_err().to_string().contains("bare shell prompt"));
        assert!(prompt_state(&t, &[agent("unknown")]).is_err());
        assert!(prompt_state(&t, &[agent("blocked")]).unwrap_err().to_string().contains("agent_blocked"));
        assert_eq!(prompt_state(&t, &[agent("working")]).unwrap(), "working");
        assert_eq!(prompt_state(&t, &[agent("idle")]).unwrap(), "idle");
    }

    /// A project with a live coordinator in its own Space (id `whome`), over a
    /// fake core, so `place_tab` runs its real placement path in a test.
    struct FakeProject {
        core: crate::fake_hc::FakeCore,
        project: Project,
        /// Kept alive: the project lives under it.
        _home: tempfile::TempDir,
    }

    impl FakeProject {
        fn new() -> FakeProject {
            let home = tempfile::tempdir().unwrap();
            let project = crate::project::create(&home.path().join("root"), "demo", vec![]).unwrap();
            let dir = project.canonical_dir().to_string_lossy().into_owned();
            let core = crate::fake_hc::FakeCore::new();
            let coordinator = core.terminal("hpc-demo", &dir, &[("hp_ws", "whome"), ("hp_ws_label", "Demo")]);
            project
                .update_coordinator(|c| {
                    c.socket = String::new();
                    c.workspace_id = "whome".into();
                    c.tab_id = "whome-t".into();
                    c.pane_id = coordinator.name.clone();
                    c.cwd = dir.clone();
                })
                .unwrap();
            FakeProject { core, project, _home: home }
        }

        fn view(&self) -> SessionView<'_> {
            let herdr = Core::new("hc", "", &self.core);
            let agents = herdr.agent_list().unwrap();
            let panes = herdr.pane_list().unwrap();
            SessionView { herdr, socket: String::new(), agents, panes }
        }

        /// The labels of the terminal a placement ended on.
        fn labels_of(&self, pane: &str) -> std::collections::BTreeMap<String, String> {
            self.core.terminals.borrow().iter().find(|t| t.name == pane).map(|t| t.labels.clone()).unwrap_or_default()
        }

        fn spaces(&self) -> Vec<crate::core::Workspace> {
            Core::new("hc", "", &self.core).workspace_list().unwrap()
        }

        /// The project's thread Space, as the core lists it: `<display name>
        /// Threads` (`docs/thread-space.md`).
        fn thread_spaces(&self) -> Vec<crate::core::Workspace> {
            self.spaces().into_iter().filter(|w| crate::labels::is_threads_space_label(&w.label)).collect()
        }

        /// A context the real `thread` subcommands run in: this project's root,
        /// the fake core as the runner, an environment that names no shell.
        fn ctx<'a>(&'a self, env: &'a crate::paths::Env) -> Ctx<'a> {
            Ctx { env, root: self._home.path().join("root"), config_dir: self._home.path().join("cfg"), runner: &self.core, detached_ticker: false }
        }
    }

    /// One thread of the project, placed the way `thread start` places it.
    fn start_home(world: &FakeProject, title: &str) -> Thread {
        let record = thread::allocate(&world.project, |t| {
            t.title = title.to_string();
            t.kind = Kind::Local;
        })
        .unwrap();
        place_tab(&world.project, &world.view(), &record).unwrap()
    }

    /// One worktree thread of the project, placed the way `thread start`
    /// places it: the worktree folder first (already made here, so no git), its
    /// tab second, `hp_worktree` on the pane, and the record written between.
    fn start_worktree(world: &FakeProject, title: &str, path: &Path) -> Thread {
        let text = path.to_string_lossy().into_owned();
        let record = thread::allocate(&world.project, |t| {
            t.title = title.to_string();
            t.kind = Kind::Worktree;
            t.repo = path.to_string_lossy().into_owned();
            t.worktree_path = text.clone();
        })
        .unwrap();
        let created = place_thread(&world.project, &world.view(), &record, path, |_| {}).unwrap();
        world.view().herdr.worktree_mark(&created.pane_id, &text).unwrap();
        created
    }

    /// t-0062: every thread of a project runs in one Space of its own,
    /// `<display name> Threads`, as a tab named after its id — the project keeps
    /// the Space its coordinator lives in.
    #[test]
    fn a_new_thread_lands_in_the_project_s_thread_space() {
        for kind in [Kind::Local, Kind::Checkout] {
            let world = FakeProject::new();
            let record = thread::allocate(&world.project, |t| {
                t.title = "线程命名 tab 用 id".into();
                t.kind = kind;
            })
            .unwrap();
            let placed = place_tab(&world.project, &world.view(), &record).unwrap();
            let threads = world.thread_spaces();
            assert_eq!(threads.len(), 1, "{kind:?}: one Space holds the project's threads, not one each: {:?}", world.spaces());
            assert_eq!(threads[0].label, "Demo Threads", "{kind:?}: the Space is the project's, not the thread's");
            assert_eq!(placed.workspace_id, threads[0].workspace_id, "{kind:?}: the thread's tab is in it");
            assert_ne!(placed.workspace_id, "whome", "{kind:?}: never the coordinator's own Space");
            assert_eq!(world.labels_of(&placed.pane_id).get("hp_tab_label").map(String::as_str), Some("t-0001"), "{kind:?}: the tab is the thread's id");
            assert!(!world.labels_of(&placed.pane_id).contains_key("hp_worktree"), "{kind:?}: only a worktree pane carries that");
        }
    }

    #[test]
    fn a_second_thread_joins_the_same_space_instead_of_opening_another() {
        let world = FakeProject::new();
        let first = start_home(&world, "Research");
        let second = start_home(&world, "Write the notes");
        assert_eq!(second.workspace_id, first.workspace_id, "the same Space");
        assert_ne!(second.tab_id, first.tab_id, "a tab each");
        assert_eq!(world.thread_spaces().len(), 1, "{:?}", world.spaces());
        assert_eq!(world.spaces().len(), 2, "the coordinator's Space and the threads' one, and no third");
    }

    #[test]
    fn a_restart_reuses_the_thread_space_rather_than_opening_a_new_one() {
        let world = FakeProject::new();
        let first = start_home(&world, "Research");
        let second = start_home(&world, "Write the notes");
        // The first thread's pane is gone; its record still names the Space.
        world.core.finish(&first.pane_id, "exited");
        let record = thread::load(&world.project, &first.id).unwrap();
        let reopened = place_tab(&world.project, &world.view(), &record).unwrap();
        assert_eq!(reopened.workspace_id, first.workspace_id, "the same Space, not a fresh one beside it");
        assert_eq!(world.thread_spaces().len(), 1, "{:?}", world.spaces());
        assert_eq!(second.workspace_id, first.workspace_id);
    }

    #[test]
    fn a_resolve_closes_the_tab_and_keeps_the_space_the_other_threads_are_in() {
        let world = FakeProject::new();
        let first = start_home(&world, "Research");
        let second = start_home(&world, "Write the notes");
        let env = crate::paths::Env::for_test(world._home.path(), &[]);
        let ctx = world.ctx(&env);
        let record = thread::load(&world.project, &first.id).unwrap();
        let cleaned = clean(&ctx, &world.project, &record, &Clean { keep_worktree: false, copy_complete: true, merged_head: String::new() });
        assert!(cleaned.notes.iter().any(|n| n == "its tab was closed"), "{:?}", cleaned.notes);
        assert!(!cleaned.notes.iter().any(|n| n.contains("Space")), "the Space is not touched: {:?}", cleaned.notes);
        let spaces = world.spaces();
        assert!(spaces.iter().any(|w| w.workspace_id == first.workspace_id), "the shared Space is still there: {spaces:?}");
        let panes = world.view().panes;
        assert!(!panes.iter().any(|p| p.pane_id == first.pane_id), "only its tab closed");
        assert!(panes.iter().any(|p| p.pane_id == second.pane_id), "the other thread's tab is untouched");
        // And the ticker's own sweep agrees: a Space an open thread is in is never empty.
        let herdr = Core::new("hc", "", &world.core);
        let empty = crate::spaces::empty(&ctx, &world.project, &herdr, false);
        assert!(!empty.iter().any(|s| s.id == first.workspace_id), "{empty:?}");
    }

    /// t-0071: a resolve used to look for a stray agent the instant it hung the
    /// program up. A real program takes tens of milliseconds to exit, so the
    /// look found it alive, wrote `agent outlived its pane` into the record and
    /// an inbox `thread-error` (`is resolved but its agent is still running`) —
    /// 72 of those in a few days, 7 of them measurably dead 0–19 seconds later
    /// (t-0068 §6.1). The close waits now, so a thread whose agent does exit is
    /// not reported as a stray.
    #[test]
    fn a_resolve_does_not_call_an_agent_a_stray_while_it_is_still_exiting() {
        let world = FakeProject::new();
        let placed = start_home(&world, "Research");
        // The pane is a running agent, as one is after the ticker's launch
        // step: the record names it and the terminal carries the label.
        let record = thread::update(&world.project, &placed.id, |t| t.agent_name = "hp-demo-t-0001".into()).unwrap();
        let herdr = Core::new("hc", "", &world.core);
        herdr.set_labels(&record.pane_id, &[(crate::core::L_AGENT.to_string(), record.agent_name.clone())]).unwrap();
        world.core.dies_in(&record.pane_id, std::time::Duration::from_millis(300));
        let env = crate::paths::Env::for_test(world._home.path(), &[]);
        let ctx = world.ctx(&env);
        let resolved = thread::update(&world.project, &record.id, |t| t.status = Status::Resolved).unwrap();

        let cleaned = clean(&ctx, &world.project, &resolved, &Clean { keep_worktree: false, copy_complete: true, merged_head: String::new() });

        assert!(cleaned.stray.is_none(), "the agent was exiting, not a stray: {:?}", cleaned.stray);
        assert!(report_stray(&world.project, &resolved.id, &resolved, cleaned.stray).is_ok(), "a clean close is not an error");
        let items = crate::inbox::unhandled(&world.project);
        assert!(!items.iter().any(|i| i.summary.contains("still running")), "no `thread-error` about an agent that went away: {items:?}");
        assert!(thread::load(&world.project, &record.id).unwrap().stray_agent.is_empty(), "nor in the record");
    }

    #[test]
    fn the_thread_space_is_closed_only_when_its_last_thread_leaves() {
        let world = FakeProject::new();
        let first = start_home(&world, "Research");
        let env = crate::paths::Env::for_test(world._home.path(), &[]);
        let ctx = world.ctx(&env);
        let record = thread::load(&world.project, &first.id).unwrap();
        let cleaned = clean(&ctx, &world.project, &record, &Clean { keep_worktree: false, copy_complete: true, merged_head: String::new() });
        assert!(cleaned.notes.iter().any(|n| n == "the Space it was the last thread in was closed"), "{:?}", cleaned.notes);
        assert!(!world.spaces().iter().any(|w| w.workspace_id == first.workspace_id), "nothing is left in it: {:?}", world.spaces());
    }

    /// 2026-11-20, law: `thread start` made the Space and its tab at 23:31:42.8,
    /// and at 23:31:44.2 a tick's `spaces::empty` called that Space empty and
    /// closed it, killing the pane and the thread 1.3s after it was placed. The
    /// record is written after `tab_create`, so at that instant the only thing
    /// saying the pane was a thread's was the pane's own `hp_tab_label`.
    #[test]
    fn a_space_holding_a_tab_no_record_names_yet_is_not_empty() {
        let world = FakeProject::new();
        // A thread resolved a moment before, its record still naming the Space:
        // that is what gets the sweep past its cheap early return, as it did
        // then.
        let earlier = start_home(&world, "Earlier");
        world.core.finish(&earlier.pane_id, "exited");
        thread::update(&world.project, &earlier.id, |t| t.status = Status::Resolved).unwrap();
        // The tab placed, the record not written yet: exactly the window.
        let fresh = start_home(&world, "Research");
        thread::update(&world.project, &fresh.id, |t| {
            t.workspace_id.clear();
            t.tab_id.clear();
            t.pane_id.clear();
        })
        .unwrap();
        let env = crate::paths::Env::for_test(world._home.path(), &[]);
        let ctx = world.ctx(&env);
        let herdr = Core::new("hc", "", &world.core);

        let empty = crate::spaces::empty(&ctx, &world.project, &herdr, false);
        assert!(!empty.iter().any(|s| s.id == fresh.workspace_id), "the pane says which thread it is: {empty:?}");

        // Take that self-claim away and the Space is what it looked like before
        // the fix: nothing left in it. This half is what makes the one above a
        // test of the rule rather than of "never close anything".
        herdr.set_labels(&fresh.pane_id, &[(crate::core::L_TAB_LABEL.to_string(), String::new())]).unwrap();
        let empty = crate::spaces::empty(&ctx, &world.project, &herdr, false);
        assert!(empty.iter().any(|s| s.id == fresh.workspace_id), "nothing names it any more: {empty:?}");
    }

    /// 2026-11-21, my-net: `open` made a home Space and its `coordinator` tab at
    /// 07:21:56.7 and wrote their names, a tick judged the Space empty at
    /// 07:21:59.5 and killed the pane, and `coordinator.json` only came at
    /// 07:22:00 — 2.8s too late. The pane's own `hp_tab_label` is the only
    /// claim the Space has until the record exists.
    #[test]
    fn a_home_space_holding_only_an_unrecorded_coordinator_tab_is_not_empty() {
        let world = FakeProject::new();
        // A thread of this project, which is what gets the sweep past its cheap
        // early return (`twins`): without one nothing can be an orphan home.
        start_home(&world, "Earlier");
        let dir = world.project.canonical_dir().to_string_lossy().into_owned();
        // The Space `open` just made: the home label, one `coordinator` tab, and
        // no record or lane instance naming either of them yet.
        let pane = world.core.terminal("hp-demo-1", &dir, &[("hp_ws", "wnew"), ("hp_ws_label", "Demo"), ("hp_tab_label", "coordinator")]);
        let env = crate::paths::Env::for_test(world._home.path(), &[]);
        let ctx = world.ctx(&env);
        let herdr = Core::new("hc", "", &world.core);

        let empty = crate::spaces::empty(&ctx, &world.project, &herdr, false);
        assert!(!empty.iter().any(|s| s.id == "wnew"), "the tab says what it is: {empty:?}");

        // Take that self-claim away and the Space is what it looked like before
        // the fix: a leftover home Space with a shell in it.
        herdr.set_labels(&pane.name, &[(crate::core::L_TAB_LABEL.to_string(), String::new())]).unwrap();
        let empty = crate::spaces::empty(&ctx, &world.project, &herdr, false);
        assert!(empty.iter().any(|s| s.id == "wnew"), "nothing names it any more: {empty:?}");
    }

    /// The record still names the Space the coordinator lived in before, and the
    /// new one holds only that unrecorded tab: the pane's claim is the one that
    /// counts (my-net on 2026-11-21). A tab whose `hp_lane` names a **closed**
    /// lane protects nothing, though — that lane is retired (`docs/lanes.md`
    /// §6.4) — and neither does one of another project.
    #[test]
    fn an_unrecorded_coordinator_tab_whose_lane_is_closed_protects_nothing() {
        let world = FakeProject::new();
        start_home(&world, "Earlier");
        let dir = world.project.canonical_dir().to_string_lossy().into_owned();
        let pane = world.core.terminal("hp-demo-1", &dir, &[("hp_ws", "wnew"), ("hp_ws_label", "Demo"), ("hp_tab_label", "coordinator")]);
        let env = crate::paths::Env::for_test(world._home.path(), &[]);
        let ctx = world.ctx(&env);
        let herdr = Core::new("hc", "", &world.core);
        // FakeProject records `whome` as the coordinator's Space: an old one, so
        // the new Space is nobody's by record.
        assert_ne!(world.project.coordinator().unwrap().workspace_id, "wnew");

        let empty = crate::spaces::empty(&ctx, &world.project, &herdr, false);
        assert!(!empty.iter().any(|s| s.id == "wnew"), "the pane names a lane nobody closed yet: {empty:?}");

        // A registry whose only lane is closed: the pane claims a retired line.
        crate::lanes::Registry {
            lanes: vec![crate::lanes::Lane { id: "c-1".into(), name: "default".into(), label: "Demo".into(), status: crate::lanes::Status::Closed, ..Default::default() }],
            default: "c-1".into(),
            next: 2,
        }
        .save(&world.project)
        .unwrap();
        herdr.set_labels(&pane.name, &[(crate::lanes::TOKEN.to_string(), "c-1".to_string())]).unwrap();
        let empty = crate::spaces::empty(&ctx, &world.project, &herdr, false);
        assert!(empty.iter().any(|s| s.id == "wnew"), "a closed lane protects nothing: {empty:?}");
    }

    /// The tick's recycle holds the project lock across "list, judge, close",
    /// and a placement holds that same lock from its Space lookup to the record
    /// naming its pane (`threads::place_thread`). That is what makes them one
    /// step: a tick that listed before a placement cannot close after it. Here
    /// the lock is held by the test, and the close has to wait for it.
    #[test]
    fn closing_empty_spaces_waits_for_a_placement_that_holds_the_lock() {
        struct NoCommands;
        impl Runner for NoCommands {
            fn run(&self, _: &Cmd) -> Result<crate::runner::Output> {
                Ok(crate::runner::Output { code: Some(1), stdout: String::new(), stderr: "the session is not reachable".into(), timed_out: false })
            }
            fn run_foreground(&self, _: &Cmd, _: &mut dyn FnMut() -> bool) -> Result<Option<i32>> {
                Ok(Some(1))
            }
        }
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        let project = crate::project::create(&root, "demo", vec![]).unwrap();
        let held = project.lock().unwrap();
        let (done, waiting) = std::sync::mpsc::channel();
        let worker_root = root.clone();
        let worker = std::thread::spawn(move || {
            let env = crate::paths::Env::for_test(&worker_root, &[]);
            let ctx = Ctx { env: &env, root: worker_root.clone(), config_dir: worker_root.join("cfg"), runner: &NoCommands, detached_ticker: false };
            let project = Project::load(&worker_root, "demo").unwrap();
            let herdr = Core::new("hc", "", &NoCommands);
            let _ = done.send(crate::spaces::close_empty(&ctx, &project, &herdr).len());
        });
        assert!(waiting.recv_timeout(std::time::Duration::from_millis(300)).is_err(), "the close waits while the lock is held");
        drop(held);
        assert_eq!(waiting.recv_timeout(std::time::Duration::from_secs(10)).ok(), Some(0), "and finishes once it is free");
        worker.join().unwrap();
    }

    #[test]
    fn a_record_from_before_the_change_still_starts_in_the_thread_space() {
        // `kind = "tab"` and `kind = "home"` records, with the Space of their own
        // they were made in and the fields the merge dropped (`own_workspace`,
        // `repo_workspace`): the value still parses — as `local` —, the dropped
        // fields are ignored, and the placement puts the thread in the project's
        // thread Space instead.
        for raw in ["tab", "home"] {
            let world = FakeProject::new();
            let record = thread::allocate(&world.project, |t| {
                t.title = "研究".into();
                t.kind = Kind::Local;
            })
            .unwrap();
            let path = thread::record_path(&world.project, &record.id);
            let mut toml = std::fs::read_to_string(&path).unwrap();
            for (key, value) in [("kind", raw), ("own_workspace", "wold"), ("workspace_id", "wold"), ("repo_workspace", "wrepo")] {
                let line = format!("{key} = \"{value}\"");
                match toml.lines().position(|l| l.starts_with(&format!("{key} = "))) {
                    Some(at) => {
                        let mut lines: Vec<String> = toml.lines().map(str::to_string).collect();
                        lines[at] = line;
                        toml = lines.join("\n") + "\n";
                    }
                    None => toml.push_str(&format!("{line}\n")),
                }
            }
            std::fs::write(&path, toml).unwrap();
            let old = thread::load(&world.project, &record.id).unwrap();
            assert_eq!(old.kind, Kind::Local, "kind = \"{raw}\" still parses, as `local`");
            assert_eq!(toml::to_string(&old).unwrap().lines().find(|l| l.starts_with("kind = ")), Some("kind = \"local\""), "and is written back as `local`");
            let placed = place_tab(&world.project, &world.view(), &old).unwrap();
            assert_eq!(world.thread_spaces().len(), 1, "{raw}: {:?}", world.spaces());
            assert_eq!(placed.workspace_id, world.thread_spaces()[0].workspace_id);
            assert_ne!(placed.workspace_id, "wold");
            assert_ne!(placed.workspace_id, "wrepo");
        }
    }

    #[test]
    fn the_thread_space_follows_the_display_name_instead_of_being_left_behind() {
        let world = FakeProject::new();
        let first = start_home(&world, "Research");
        // `hp set demo name "Law Office"`.
        let text = std::fs::read_to_string(world.project.project_md()).unwrap();
        let text = crate::settings::set_in(&text, "name", "Law Office").unwrap();
        std::fs::write(world.project.project_md(), text).unwrap();
        let herdr = Core::new("hc", "", &world.core);
        crate::spaces::sync_threads_label(&world.project, &herdr);
        let threads = world.thread_spaces();
        assert_eq!(threads.len(), 1, "{:?}", world.spaces());
        assert_eq!(threads[0].label, "Law Office Threads", "renamed, not replaced");
        assert_eq!(threads[0].workspace_id, first.workspace_id);
        // And a thread started now joins it rather than opening a second one.
        let second = start_home(&world, "The other one");
        assert_eq!(second.workspace_id, first.workspace_id);
        assert_eq!(world.thread_spaces().len(), 1, "{:?}", world.spaces());
    }

    /// t-0052: resolved with a stray agent on its record, and by the time anyone
    /// read the row the pane was gone from `hc ls` too. `thread audit` said
    /// `nothing unusual`; the row still said the agent "is still in pane X".
    #[test]
    fn a_resolved_rows_stray_agent_note_stops_claiming_it_is_still_running() {
        let world = FakeProject::new();
        let dir = world.project.canonical_dir().to_string_lossy().into_owned();
        let stale = thread::allocate(&world.project, |t| {
            t.kind = Kind::Local;
            t.status = Status::Resolved;
            t.title = "resolved and gone".into();
            t.resolved_reason = "manual".into();
            t.pane_id = "hp-space-b13f3d".into();
            t.cwd = dir.clone();
            t.agent_name = "hp-herdr-projects-t-0052".into();
            t.stray_agent = "agent hp-herdr-projects-t-0052 is still in pane hp-space-b13f3d (done)".into();
        })
        .unwrap();
        let note = |world: &FakeProject| {
            let view = world.view();
            rows_with_view(&world.project, Some(&view)).into_iter().find(|row| row.thread.id == stale.id).map(|row| row.note).unwrap()
        };
        assert_eq!(note(&world), "manual; the agent the resolve left behind has since exited");

        // While the agent and its pane really are there, the resolve's own line
        // is still the truth and is printed unchanged.
        world.core.terminal("hp-space-b13f3d", &dir, &[("hp_agent", "hp-herdr-projects-t-0052")]);
        assert_eq!(note(&world), "manual; agent hp-herdr-projects-t-0052 is still in pane hp-space-b13f3d (done)");
    }

    /// A thread's own Space from before t-0062 — the thread's title as its
    /// label — is left behind: its tab is not added to it another time, and
    /// `spaces::empty` recycles it once its panes are gone.
    #[test]
    fn a_legacy_tab_thread_s_space_is_left_behind_and_its_tab_joins_the_thread_space() {
        let world = FakeProject::new();
        let record = thread::allocate(&world.project, |t| {
            t.kind = Kind::Local;
            t.title = "线程命名 tab 用 id".into();
            t.status = Status::Open;
            t.workspace_id = "wthread".into();
        })
        .unwrap();
        // Its own Space as it was made, still holding one idle shell.
        world.core.terminal("hp-own", "/tmp", &[("hp_ws", "wthread"), ("hp_ws_label", "线程命名tab用id")]);
        let placed = place_tab(&world.project, &world.view(), &record).unwrap();
        assert_ne!(placed.workspace_id, "wthread", "the thread's own Space is not joined again");
        assert_eq!(world.thread_spaces().len(), 1, "{:?}", world.spaces());
        assert_eq!(placed.workspace_id, world.thread_spaces()[0].workspace_id);
        let labels = world.labels_of(&placed.pane_id);
        assert_eq!(labels.get("hp_tab_label").map(String::as_str), Some(record.id.as_str()), "the tab is the thread's id");
        assert_eq!(labels.get("hp_ws_label").map(String::as_str), Some("Demo Threads"));
    }

    /// A checkout thread restarts into the same Space as every other thread,
    /// still working in the repository's main checkout (not a worktree, and its
    /// own branch is never deleted for it).
    #[test]
    fn a_restarted_checkout_thread_joins_the_thread_space_on_the_repository() {
        let world = FakeProject::new();
        let repo = tempfile::tempdir().unwrap();
        let repo_path = std::fs::canonicalize(repo.path()).unwrap().to_string_lossy().into_owned();
        let record = thread::allocate(&world.project, |t| {
            t.kind = Kind::Checkout;
            t.title = "checkout thread".into();
            t.status = Status::Open;
            t.repo = repo_path.clone();
            t.workspace_id = "wcheck".into();
        })
        .unwrap();
        world.core.terminal("hp-check", &repo_path, &[("hp_ws", "wcheck"), ("hp_ws_label", "checkout thread")]);
        let placed = place_tab(&world.project, &world.view(), &record).unwrap();
        assert_ne!(placed.workspace_id, "wcheck");
        assert_eq!(placed.workspace_id, world.thread_spaces()[0].workspace_id);
        assert_eq!(placed.cwd, repo_path, "it still works in the repository's main checkout");
        assert_eq!(world.labels_of(&placed.pane_id).get("hp_tab_label").map(String::as_str), Some(record.id.as_str()));
    }

    /// A thread without a repo is no longer a tab of the project's Space: it has
    /// a tab of its own in the project's thread Space, so the coordinator's own
    /// Space holds nothing but its coordinator.
    #[test]
    fn a_home_thread_no_longer_lands_in_the_project_s_own_space() {
        let world = FakeProject::new();
        let record = thread::allocate(&world.project, |t| {
            t.kind = Kind::Local;
            t.title = "local thread".into();
            t.status = Status::Open;
        })
        .unwrap();
        let placed = place_tab(&world.project, &world.view(), &record).unwrap();
        assert_ne!(placed.workspace_id, "whome", "the project's Space keeps its coordinator only");
        assert_eq!(placed.workspace_id, world.thread_spaces()[0].workspace_id);
        let folder = std::fs::canonicalize(record_folder(&world.project, &record)).unwrap().to_string_lossy().into_owned();
        assert_eq!(placed.cwd, folder, "it still works on `threads/<id>/`");
        let homes = world.spaces().into_iter().filter(|w| w.workspace_id == "whome").collect::<Vec<_>>();
        assert_eq!(homes.len(), 1, "the home Space is untouched");
        assert_eq!(homes[0].pane_count, 1, "one pane in it: the coordinator");
    }

    /// The 2026-11-19 incident at the placement level, the way `thread restart`
    /// would meet it: demo's record still names the Space another project's
    /// drifted coordinator sat in, and demo's own Space is proven only by its
    /// coordinator's label (its shell sits in `~`). The tab must go neither into
    /// the other project's Space nor into demo's own: it belongs in demo's thread
    /// Space.
    #[test]
    fn a_thread_record_naming_another_projects_space_is_not_trusted() {
        let home = tempfile::tempdir().unwrap();
        let project = crate::project::create(&home.path().join("root"), "demo", vec![]).unwrap();
        let dir = project.canonical_dir().to_string_lossy().into_owned();
        let core = crate::fake_hc::FakeCore::new();
        // The other project's coordinator, drifted into demo's folder. Terminals
        // keep creation order, so a folder search finds this one first; only its
        // label says it is not demo's.
        core.terminal("t5h", &dir, &[("hp_ws", "sp-law"), ("hp_ws_label", "Law"), ("hp_project", "law")]);
        // Demo's own coordinator in demo's own Space, its shell at `~`.
        core.terminal("t5b", "/home/me", &[("hp_ws", "sp-demo"), ("hp_ws_label", "Demo"), ("hp_project", "demo")]);
        project
            .update_coordinator(|c| {
                c.socket = String::new();
                c.workspace_id = "sp-demo".into();
                c.tab_id = "sp-demo-tab".into();
                c.pane_id = "t5b".into();
                c.cwd = dir.clone();
            })
            .unwrap();
        let record = thread::allocate(&project, |t| {
            t.kind = Kind::Local;
            t.title = "Drain the held-document backlog".into();
            t.status = Status::Open;
            // What the incident's record was left saying.
            t.workspace_id = "sp-law".into();
        })
        .unwrap();
        std::fs::create_dir_all(project.dir().join("threads").join(&record.id)).unwrap();

        let herdr = Core::new("hc", "", &core);
        let agents = herdr.agent_list().unwrap();
        let panes = herdr.pane_list().unwrap();
        let view = SessionView { herdr, socket: String::new(), agents, panes };
        let placed = place_tab(&project, &view, &record).unwrap();

        assert_ne!(placed.workspace_id, "sp-law", "never the other project's Space the record names");
        assert_ne!(placed.workspace_id, "sp-demo", "and never the project's own Space either");
        assert_eq!(placed.workspace_id, world_thread_space(&core), "a thread Space of its own");
        let labels = core.terminals.borrow().iter().find(|t| t.name == placed.pane_id).map(|t| t.labels.clone()).unwrap();
        assert_eq!(labels.get("hp_ws_label").map(String::as_str), Some("Demo Threads"));
        let in_law = core.terminals.borrow().iter().filter(|t| t.labels.get("hp_ws").map(String::as_str) == Some("sp-law")).count();
        assert_eq!(in_law, 1, "the drifted coordinator's own terminal, and nothing else was added to that Space");
    }

    /// The 2026-11-19 incident: three worktree threads started seconds apart in
    /// one project, and each one opened a Space of its own, all named `Project
    /// Threads`. A thread Space holding one worktree thread — every thread here
    /// is one — was being judged "this Space is one worktree" and refused.
    #[test]
    fn worktree_threads_started_one_after_another_share_one_space() {
        let world = FakeProject::new();
        let folders = tempfile::tempdir().unwrap();
        let paths: Vec<std::path::PathBuf> = ["one", "two", "three"]
            .iter()
            .map(|name| {
                let path = folders.path().join(name);
                std::fs::create_dir_all(&path).unwrap();
                std::fs::canonicalize(&path).unwrap()
            })
            .collect();
        let mut placed = Vec::new();
        for (n, path) in paths.iter().enumerate() {
            placed.push(start_worktree(&world, &format!("thread {n}"), path));
        }
        let ids: Vec<&str> = placed.iter().map(|t| t.workspace_id.as_str()).collect();
        assert_eq!(ids[0], ids[1], "the second thread joins the first one's Space, so the same Space: {ids:?}");
        assert_eq!(ids[1], ids[2], "and the third the same: {ids:?}");
        assert_eq!(world.thread_spaces().len(), 1, "one `<display name> Threads`, not one per thread: {:?}", world.spaces());
        assert_eq!(world.spaces().len(), 2, "the coordinator's Space and the threads' one, and no third: {:?}", world.spaces());
        assert_eq!(world.thread_spaces()[0].pane_count, 3, "three tabs in it");
        // And a restart lands in it with one worktree thread left in the Space,
        // which is the shape that reads as "this Space is one worktree".
        world.core.finish(&placed[1].pane_id, "exited");
        let record = thread::load(&world.project, &placed[1].id).unwrap();
        let again = place_thread(&world.project, &world.view(), &record, &paths[1], |_| {}).unwrap();
        assert_eq!(again.workspace_id, ids[0], "a restart lands in the same Space: {:?}", world.spaces());
        assert_eq!(world.thread_spaces().len(), 1, "{:?}", world.spaces());
    }

    /// The same incident at its worst: two placements that took their snapshot
    /// of panes and agents before either Space existed, so neither can see the
    /// other's pane. Each placement holds the project lock from the lookup to its
    /// own record (`threads::place_thread`), so the second one reads the first
    /// one's record and joins its Space instead of making a second one.
    #[test]
    fn two_threads_placed_from_one_snapshot_share_one_space() {
        let world = FakeProject::new();
        let view = world.view();
        let folders = tempfile::tempdir().unwrap();
        let folder = |name: &str| {
            let path = folders.path().join(name);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::canonicalize(&path).unwrap()
        };
        let (a, b) = (folder("one"), folder("two"));
        let record = |title: &str, path: &Path| {
            thread::allocate(&world.project, |t| {
                t.title = title.to_string();
                t.kind = Kind::Worktree;
                t.worktree_path = path.to_string_lossy().into_owned();
            })
            .unwrap()
        };
        let (first, second) = (record("first", &a), record("second", &b));
        // The first one lands, worktree label and all, before the second asks.
        let made = place_thread(&world.project, &view, &first, &a, |_| {}).unwrap();
        view.herdr.worktree_mark(&made.pane_id, &a.to_string_lossy()).unwrap();
        let also = place_thread(&world.project, &view, &second, &b, |_| {}).unwrap();
        assert_eq!(also.workspace_id, made.workspace_id, "one Space, not one each");
        assert_eq!(world.thread_spaces().len(), 1, "{:?}", world.spaces());
        assert_eq!(world.spaces().len(), 2, "the coordinator's Space and the threads' one: {:?}", world.spaces());
    }

    /// The app can list a Space it made before any hp label is on a terminal of
    /// it. Nothing is written on such a Space, so the record naming it is the
    /// only thing that says whose it is — and it is renamed on the next tick,
    /// by the same judgement (`spaces::sync_threads_label`).
    #[test]
    fn a_thread_space_with_no_name_yet_is_found_by_its_record_and_named() {
        let world = FakeProject::new();
        let first = start_home(&world, "Research");
        let herdr = world.view().herdr;
        herdr.clear_labels(&first.pane_id, &["hp_ws_label", "hp_project"]).unwrap();
        let label_of = |world: &FakeProject, id: &str| world.spaces().into_iter().find(|w| w.workspace_id == id).map(|w| w.label);
        assert_eq!(label_of(&world, &first.workspace_id), Some(String::new()), "precondition: a Space with no name");
        assert_eq!(
            threads_space(&world.project, &world.spaces(), &[], &[], None).as_deref(),
            Some(first.workspace_id.as_str()),
            "the record is enough when nothing is written on the Space"
        );
        let second = start_home(&world, "Write the notes");
        assert_eq!(second.workspace_id, first.workspace_id, "a thread started now joins it, not a Space beside it");
        crate::spaces::sync_threads_label(&world.project, &herdr);
        assert_eq!(label_of(&world, &first.workspace_id), Some("Demo Threads".into()), "and it gets its name");
        assert_eq!(world.thread_spaces().len(), 1, "{:?}", world.spaces());
    }

    /// Two projects on one core, each with a coordinator in a Space of its own:
    /// the shape the cross-project questions are asked in. The panes of two
    /// projects are listed side by side, and nothing but the record, the exact
    /// name or the panes themselves may say which Space is whose.
    struct TwoProjects {
        core: crate::fake_hc::FakeCore,
        alpha: Project,
        beta: Project,
        _home: tempfile::TempDir,
    }

    impl TwoProjects {
        fn new() -> TwoProjects {
            let home = tempfile::tempdir().unwrap();
            let root = home.path().join("root");
            let alpha = crate::project::create(&root, "alpha", vec![]).unwrap();
            let beta = crate::project::create(&root, "beta", vec![]).unwrap();
            let core = crate::fake_hc::FakeCore::new();
            for (project, space, pane) in [(&alpha, "ahome", "a-coordinator"), (&beta, "bhome", "b-coordinator")] {
                let dir = project.canonical_dir().to_string_lossy().into_owned();
                let label = project.display_name();
                core.terminal(pane, &dir, &[("hp_ws", space), ("hp_ws_label", label.as_str()), ("hp_project", project.slug.as_str())]);
                project
                    .update_coordinator(|c| {
                        c.socket = String::new();
                        c.workspace_id = space.into();
                        c.tab_id = format!("{space}-t");
                        c.pane_id = pane.into();
                        c.cwd = dir.clone();
                    })
                    .unwrap();
            }
            TwoProjects { core, alpha, beta, _home: home }
        }

        fn herdr(&self) -> Core<'_> {
            Core::new("hc", "", &self.core)
        }

        fn spaces(&self) -> Vec<crate::core::Workspace> {
            self.herdr().workspace_list().unwrap()
        }

        fn label_of(&self, space: &str) -> String {
            self.spaces().into_iter().find(|w| w.workspace_id == space).map(|w| w.label).unwrap_or_default()
        }

        /// A thread Space one project's `place` would have made: the Space, a
        /// pane of that project in it (`hp_project`), and a record naming it.
        fn thread_space(&self, project: &Project, space: &str, worktree: Option<&str>) {
            let dir = project.canonical_dir().to_string_lossy().into_owned();
            let label = crate::labels::threads_space_label(&project.display_name());
            let mut labels = vec![("hp_ws", space), ("hp_ws_label", label.as_str()), ("hp_project", project.slug.as_str())];
            if let Some(path) = worktree {
                labels.push(("hp_worktree", path));
            }
            self.core.terminal(&format!("{}-thread", project.slug), &dir, &labels);
            let record = thread::allocate(project, |t| {
                t.kind = Kind::Local;
                t.title = "the first".into();
                t.status = Status::Open;
            })
            .unwrap();
            thread::update(project, &record.id, |t| t.workspace_id = space.to_string()).unwrap();
        }
    }

    /// 2026-11-20, the accident (t-0075's pane, `hc get` read tick after tick):
    /// `Project Threads` → `Web Tpl Threads` → `My Net Threads` → `Little World
    /// Threads`, each project's tick renaming the one Space it found to its own
    /// name. `sync_threads_label` runs for every project on every tick and reads
    /// no panes, so what binds a Space to *this* project is a record of this
    /// project naming it, or the Space carrying today's name exactly. Another
    /// project's `… Threads` is neither.
    #[test]
    fn a_projects_label_sync_never_renames_another_projects_thread_space() {
        // Two shapes of the other project's Space: plain, and holding one
        // worktree thread (which reads as a worktree Space, §6.4).
        for worktree in [None, Some("/wt/beta")] {
            let world = TwoProjects::new();
            world.thread_space(&world.beta, "bspace", worktree);
            let herdr = world.herdr();
            // Alpha has no thread Space of its own.
            crate::spaces::sync_threads_label(&world.alpha, &herdr);
            assert_eq!(world.label_of("bspace"), "Beta Threads", "{worktree:?}: alpha's tick renamed beta's Space");
            assert_eq!(world.label_of("bhome"), "Beta", "{worktree:?}: nor did it touch beta's own Space");
            // And the same with the panes in hand: alpha's finder does not answer
            // with beta's Space, and a thread of alpha is not placed in it.
            let panes = world.herdr().pane_list().unwrap();
            assert_eq!(threads_space(&world.alpha, &world.spaces(), &panes, &[], None), None, "{worktree:?}: beta's Space is beta's");
            let folder = world.alpha.dir().join("threads").join("t-0001");
            std::fs::create_dir_all(&folder).unwrap();
            let record = thread::allocate(&world.alpha, |t| {
                t.kind = Kind::Local;
                t.title = "alpha's first".into();
            })
            .unwrap();
            let view = SessionView { herdr: world.herdr(), socket: String::new(), agents: world.herdr().agent_list().unwrap(), panes: world.herdr().pane_list().unwrap() };
            let placed = place_tab(&world.alpha, &view, &record).unwrap();
            assert_ne!(placed.workspace_id, "bspace", "{worktree:?}: alpha's thread in beta's Space");
            assert_eq!(world.label_of(&placed.workspace_id), "Alpha Threads", "{worktree:?}: a thread Space of alpha's own");
            assert_eq!(world.label_of("bspace"), "Beta Threads", "{worktree:?}: and beta's Space is still beta's");
        }
    }

    /// The named finder at the cross-project boundary, with no record involved:
    /// another project's `… Threads` name says whose Space it is — that project's
    /// — and the panes in it say the same.
    #[test]
    fn the_finder_does_not_answer_with_another_projects_thread_space() {
        let world = TwoProjects::new();
        world.thread_space(&world.beta, "bspace", None);
        let herdr = world.herdr();
        let spaces = world.spaces();
        let panes = herdr.pane_list().unwrap();
        assert_eq!(threads_space(&world.alpha, &spaces, &[], &[], None), None, "a name that ends in ` Threads` is not alpha's");
        assert_eq!(threads_space(&world.alpha, &spaces, &panes, &[], None), None, "and neither are beta's panes");
        assert_eq!(threads_space(&world.beta, &spaces, &panes, &[], None).as_deref(), Some("bspace"), "while beta finds its own");
    }

    fn record_folder(project: &Project, record: &Thread) -> std::path::PathBuf {
        project.dir().join("threads").join(&record.id)
    }

    fn world_thread_space(core: &crate::fake_hc::FakeCore) -> String {
        let listed = Core::new("hc", "", core).workspace_list().unwrap();
        listed.iter().find(|w| crate::labels::is_threads_space_label(&w.label)).map(|w| w.workspace_id.clone()).unwrap_or_default()
    }

    #[test]
    fn placement_follows_the_request_then_the_repo() {
        assert_eq!(placement(None, true).unwrap(), Kind::Worktree);
        assert_eq!(placement(None, false).unwrap(), Kind::Local, "no repo: a folder of its own, no worktree");
        assert_eq!(placement(Some(Kind::Local), true).unwrap(), Kind::Local);
        assert_eq!(placement(Some(Kind::Local), false).unwrap(), Kind::Local);
        assert_eq!(placement(Some(Kind::Checkout), true).unwrap(), Kind::Checkout);
        assert!(placement(Some(Kind::Checkout), false).is_err());
        assert!(placement(Some(Kind::Worktree), false).is_err());
        assert!(placement(Some(Kind::Adopted), true).is_err());
        assert_eq!(Kind::parse("checkout").unwrap(), Kind::Checkout);
        assert_eq!(Kind::parse("local").unwrap(), Kind::Local);
        assert!(Kind::parse("nowhere").is_err());
    }

    /// `tab` and `home` were two names for what `local` is; the command line
    /// still takes them (a person's habit is not worth breaking), and the
    /// record writes one value.
    #[test]
    fn the_old_kind_names_are_local() {
        assert_eq!(Kind::parse("tab").unwrap(), Kind::Local);
        assert_eq!(Kind::parse("home").unwrap(), Kind::Local);
        assert_eq!(toml::to_string(&Thread { kind: Kind::Local, ..Thread::default() }).unwrap().lines().find(|l| l.starts_with("kind = ")), Some("kind = \"local\""), "one value is written, whatever it was called");
        for raw in ["home", "tab"] {
            let parsed: Thread = toml::from_str(&format!("kind = \"{raw}\"")).unwrap();
            assert_eq!(parsed.kind, Kind::Local, "a record saying `{raw}` reads as `local`");
        }
        let parsed: Thread = toml::from_str("kind = \"local\"").unwrap();
        assert_eq!(parsed.kind, Kind::Local);
        assert!(toml::from_str::<Thread>("kind = \"nowhere\"").is_err(), "a value that is not a kind is still refused");
    }

    #[test]
    fn exclude_is_added_once() {
        let repo = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(repo.path()).args(args).output().unwrap();
        run(&["init", "-q"]);
        let cwd = repo.path().to_string_lossy().into_owned();
        exclude_from_git(&crate::runner::RealRunner, &cwd).unwrap();
        exclude_from_git(&crate::runner::RealRunner, &cwd).unwrap();
        let text = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
        assert_eq!(text.matches(".herdr-project/").count(), 1);
        std::fs::create_dir_all(repo.path().join(".herdr-project/x")).unwrap();
        std::fs::write(repo.path().join(".herdr-project/x/report.md"), "r").unwrap();
        assert!(String::from_utf8_lossy(&run(&["status", "--porcelain"]).stdout).is_empty());
    }

    /// A real repo with a shared primary `.agents`, and a real worktree whose
    /// `.agents` is the link the pre-exclude code wrote: an untracked file to
    /// git, exactly the state `resolve` could not remove without `--force`.
    fn worktree_with_a_bare_agents_link() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        let worktree = home.path().join("worktree");
        let run = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git").arg("-C").arg(cwd).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q"]);
        run(&repo, &["config", "user.email", "t@example.invalid"]);
        run(&repo, &["config", "user.name", "Test"]);
        std::fs::create_dir_all(repo.join(".agents/skills/sample")).unwrap();
        std::fs::write(repo.join("README.md"), "base").unwrap();
        run(&repo, &["add", "README.md"]);
        run(&repo, &["commit", "-qm", "base"]);
        run(&repo, &["worktree", "add", "--detach", worktree.to_str().unwrap()]);
        std::os::unix::fs::symlink(std::fs::canonicalize(repo.join(".agents")).unwrap(), worktree.join(".agents")).unwrap();
        (home, repo, worktree)
    }

    fn removal_ctx<'a>(home: &Path, env: &'a crate::paths::Env) -> Ctx<'a> {
        Ctx { env, root: home.join("root"), config_dir: home.join("cfg"), runner: &crate::runner::RealRunner, detached_ticker: false }
    }

    fn record_of(repo: &Path, worktree: &Path) -> Thread {
        Thread {
            id: "t-0001".into(),
            kind: Kind::Worktree,
            status: Status::Open,
            repo: repo.to_string_lossy().into_owned(),
            worktree_path: worktree.to_string_lossy().into_owned(),
            cwd: worktree.to_string_lossy().into_owned(),
            ..Thread::default()
        }
    }

    #[test]
    fn a_worktree_whose_only_untracked_file_is_the_shared_agents_link_is_removed_non_forced() {
        let (home, repo, worktree) = worktree_with_a_bare_agents_link();
        let env = crate::paths::Env::for_test(home.path(), &[]);
        let ctx = removal_ctx(home.path(), &env);
        let project = crate::project::create(&ctx.root, "demo", vec![]).unwrap();

        let removal = remove_worktree(&ctx, &project, &record_of(&repo, &worktree), None).unwrap();
        assert!(matches!(removal, Removal::Removed));
        assert!(!worktree.exists(), "the worktree must be gone, not kept for its own link");
        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert!(exclude.contains("/.agents"), "the legacy link was excluded before removal: {exclude}");
    }

    /// `hc` from the fake core, `git` for real: what `worktree_remove` needs,
    /// since it reads a pane's `hp_worktree` label off core and then runs git.
    struct GitThroughCore<'a> {
        core: &'a crate::fake_hc::FakeCore,
    }

    impl crate::runner::Runner for GitThroughCore<'_> {
        fn run(&self, cmd: &Cmd) -> Result<crate::runner::Output> {
            if cmd.program == "git" {
                crate::runner::RealRunner.run(cmd)
            } else {
                self.core.run(cmd)
            }
        }

        fn run_foreground(&self, cmd: &Cmd, poll: &mut dyn FnMut() -> bool) -> Result<Option<i32>> {
            self.core.run_foreground(cmd, poll)
        }
    }

    /// t-0062, the data-safety one: a project's threads share one Space, so two
    /// worktree threads' panes sit in the same Space. Removing one thread's
    /// worktree must remove **its** folder — the one its own pane is labelled
    /// with — and leave the other worktree, and the other pane, alone.
    #[test]
    fn a_worktree_removal_in_a_shared_space_takes_that_worktree_and_no_other() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        let one = home.path().join("wt-one");
        let two = home.path().join("wt-two");
        let run = |cwd: &Path, args: &[&str]| {
            let out = std::process::Command::new("git").arg("-C").arg(cwd).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q"]);
        run(&repo, &["config", "user.email", "t@example.invalid"]);
        run(&repo, &["config", "user.name", "Test"]);
        std::fs::write(repo.join("README.md"), "base").unwrap();
        run(&repo, &["add", "README.md"]);
        run(&repo, &["commit", "-qm", "base"]);
        for (path, branch) in [(&one, "hp/demo/t-0001-one"), (&two, "hp/demo/t-0002-two")] {
            run(&repo, &["worktree", "add", "-b", branch, path.to_str().unwrap()]);
        }
        let (one, two) = (std::fs::canonicalize(&one).unwrap(), std::fs::canonicalize(&two).unwrap());
        let (one_text, two_text) = (one.to_string_lossy().into_owned(), two.to_string_lossy().into_owned());

        // Two panes of one Space, as two worktree threads leave them.
        let core = crate::fake_hc::FakeCore::new();
        let shared = "hpw-demo-threads";
        let first = core.terminal("hp-one", &one_text, &[("hp_ws", shared), ("hp_ws_label", "Demo Threads"), ("hp_worktree", &one_text)]);
        let second = core.terminal("hp-two", &two_text, &[("hp_ws", shared), ("hp_ws_label", "Demo Threads"), ("hp_worktree", &two_text)]);
        let runner = GitThroughCore { core: &core };
        let env = crate::paths::Env::for_test(home.path(), &[]);
        let ctx = Ctx { env: &env, root: home.path().join("root"), config_dir: home.path().join("cfg"), runner: &runner, detached_ticker: false };
        let project = crate::project::create(&ctx.root, "demo", vec![]).unwrap();
        let herdr = Core::new("hc", "", &runner);
        let agents = herdr.agent_list().unwrap();
        let panes = herdr.pane_list().unwrap();
        let view = SessionView { herdr, socket: String::new(), agents, panes };

        // The **second** thread of the Space is resolved: keying on the Space
        // would take the first pane's worktree, which is somebody else's real
        // work (the bug this test exists for).
        let record = Thread {
            id: "t-0002".into(),
            kind: Kind::Worktree,
            status: Status::Open,
            repo: repo.to_string_lossy().into_owned(),
            branch: "hp/demo/t-0002-two".into(),
            worktree_path: two_text.clone(),
            cwd: two_text.clone(),
            workspace_id: shared.into(),
            tab_id: second.labels.get("hp_tab").cloned().unwrap_or_default(),
            pane_id: second.name.clone(),
            ..Thread::default()
        };
        let removal = remove_worktree(&ctx, &project, &record, Some(&view)).unwrap();
        assert!(matches!(removal, Removal::Removed), "the worktree went");
        assert!(!two.exists(), "the thread's own worktree is gone");
        assert!(one.exists(), "the other thread's worktree is untouched: this is the bug that would delete real work");
        assert!(one.join("README.md").is_file(), "and its contents with it");
        // And the Space is still there, with the other pane in it.
        let listed = Core::new("hc", "", &core).workspace_list().unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].pane_count, 2, "removing a worktree closes no pane and no Space");
        assert!(core.terminals.borrow().iter().any(|t| t.name == first.name), "no pane was closed either");
    }

    #[test]
    fn real_untracked_work_still_keeps_the_worktree() {
        let (home, repo, worktree) = worktree_with_a_bare_agents_link();
        std::fs::write(worktree.join("unfinished.txt"), "not copied home").unwrap();
        let env = crate::paths::Env::for_test(home.path(), &[]);
        let ctx = removal_ctx(home.path(), &env);
        let project = crate::project::create(&ctx.root, "demo", vec![]).unwrap();

        assert!(remove_worktree(&ctx, &project, &record_of(&repo, &worktree), None).is_err());
        assert!(worktree.exists(), "real work must keep the worktree");
        assert_eq!(std::fs::read_to_string(worktree.join("unfinished.txt")).unwrap(), "not copied home");
        let out = std::process::Command::new("git").arg("-C").arg(&worktree).args(["status", "--porcelain", "--untracked-files=all"]).output().unwrap();
        let status = String::from_utf8_lossy(&out.stdout);
        assert!(status.contains("?? unfinished.txt"), "{status}");
        assert!(!status.contains(".agents"), "only the link is excluded: {status}");
    }
}

#[cfg(test)]
mod coordinator_caller_tests {
    use super::*;

    /// A project whose recorded coordinator is pane `w1:p1`, and a fake core
    /// where that pane is terminal `t44`. `/r/demo` is only a label: nothing
    /// here reads a directory.
    fn setup<'a>(home: &std::path::Path, runner: &'a crate::runner::fake::FakeRunner, terminal: &str) -> (Ctx<'a>, Project) {
        let root = &home.join("root");
        let project = crate::project::Project::load(root, "demo")
            .or_else(|_| crate::project::create(root, "demo", vec![]))
            .unwrap();
        project
            .update_coordinator(|c| {
                c.pane_id = "w1:p1".into();
                c.workspace_id = "w1".into();
                c.tab_id = "w1:t1".into();
            })
            .unwrap();
        let env = crate::paths::Env::for_test(home, &[("HC_TERMINAL_ID", terminal)]);
        let env: &'static crate::paths::Env = Box::leak(Box::new(env));
        let ctx = Ctx { env, root: home.join("root"), config_dir: home.join("cfg"), runner, detached_ticker: false };
        (ctx, project)
    }

    fn core(runner: &crate::runner::fake::FakeRunner) {
        runner.on(
            "--json ls",
            crate::runner::fake::ok(r#"{"terminals":[{"name":"w1:p1","address":"t44","cwd":"/r/demo","state":"live"}]}"#),
        );
        runner.on(
            "--json get",
            crate::runner::fake::ok(r#"{"address":"t44","name":"w1:p1","labels":{"hp_agent":"hpc-demo"},"cwd":"/r/demo","state":"live"}"#),
        );
    }

    #[test]
    fn a_resolve_in_the_coordinator_s_terminal_leaves_no_item_for_itself() {
        let home = tempfile::tempdir().unwrap();
        let runner = crate::runner::fake::FakeRunner::new();
        core(&runner);
        // The coordinator's own terminal: the event it wrote is already known to
        // it, so `resolve` archives it instead of leaving one more item.
        let (ctx, project) = setup(home.path(), &runner, "t44");
        assert!(called_from_coordinator(&ctx, &project));
        // Another terminal, and no terminal at all (the ticker, a script).
        let (elsewhere, _) = setup(home.path(), &runner, "t45");
        assert!(!called_from_coordinator(&elsewhere, &project));
        let (outside, _) = setup(home.path(), &runner, "");
        assert!(!called_from_coordinator(&outside, &project));
    }

    #[test]
    fn a_terminal_that_runs_an_agent_of_another_project_is_not_the_coordinator() {
        let home = tempfile::tempdir().unwrap();
        let runner = crate::runner::fake::FakeRunner::new();
        // The same terminal address, but its pane is not this project's pane.
        runner.on(
            "--json ls",
            crate::runner::fake::ok(r#"{"terminals":[{"name":"w9:p9","address":"t44","cwd":"/r/other","state":"live"}]}"#),
        );
        runner.on(
            "--json get",
            crate::runner::fake::ok(r#"{"address":"t44","name":"w9:p9","labels":{"hp_agent":"hpc-other"},"cwd":"/r/other","state":"live"}"#),
        );
        let (ctx, project) = setup(home.path(), &runner, "t44");
        assert!(!called_from_coordinator(&ctx, &project));
    }
}
