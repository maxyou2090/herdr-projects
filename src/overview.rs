//! The text overview, and how commands without a slug find their project.

use std::fmt::Write as _;
use std::io::{BufRead, IsTerminal, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::core::{Core, Pane};
use crate::paths::Ctx;
use crate::project::{self, Project};
use crate::thread::{self, Group};
use crate::threads::{self, Row};

/// Where this command runs, in core's terms: the terminal it sits in
/// (`HC_TERMINAL_ID`) and that terminal's pane among the live panes of this
/// session (`HC_SESSION`, empty for the default core). `None` outside a core
/// terminal — the ticker, the app, a test — or when the session lists no such
/// terminal any more, which is the same answer Herdr 1's variables gave by not
/// being set at all.
fn here(ctx: &Ctx) -> Option<(Pane, Vec<Pane>)> {
    let terminal = ctx.env.this_terminal()?;
    let herdr = Core::new(ctx.env.hc_bin(), ctx.env.this_session(), ctx.runner);
    let panes = herdr.pane_list().ok()?;
    let mine = panes.iter().find(|pane| pane.terminal_id == terminal)?.clone();
    Some((mine, panes))
}

/// The project the pane a command runs in belongs to, without asking.
///
/// This is the one place the port's identity question is answered
/// (t-0040): herdr-core sets neither `HERDR_PANE_ID` nor `HERDR_WORKSPACE_ID`
/// nor `HERDR_SOCKET_PATH`, so every lookup that read them answered `None` and
/// `overview` silently printed the first project by name. Two answers, most
/// precise first:
///
/// 1. the `hp_project` label this binary writes on the panes it starts and
///    reports ([`crate::labels::report_pane`]); it survives a working directory
///    that drifted to `~`, which is exactly where a coordinator usually sits;
/// 2. the Space the pane is in — in core a Space is the set of terminals
///    carrying the same `hp_ws` label — resolved by where its panes work and,
///    failing that, by the ids the projects recorded.
///
/// `None` when neither answers, and never "the first project": an unknown
/// pane must leave `overview` on its picker and `thread audit` on all projects.
/// A pane that is neither a project's nor a thread's (a shell opened in `~`, a
/// terminal of another session) is such a case.
pub fn resolve_slug_quiet(ctx: &Ctx) -> Option<String> {
    let (mine, panes) = here(ctx)?;
    labelled(ctx, &mine).or_else(|| project_for_space(ctx, &panes, &mine))
}

/// The project a pane's `hp_project` label names, when that project is still
/// there. A label naming a deleted project is no answer.
fn labelled(ctx: &Ctx, pane: &Pane) -> Option<String> {
    let slug = pane.tokens.get(crate::labels::PROJECT_TOKEN)?.clone();
    Project::load(&ctx.root, &slug).ok().map(|p| p.slug)
}

/// The project a Space belongs to, only among projects recorded in the same
/// session (space ids repeat across sessions). First by where the Space's
/// panes work (the reader's own pane first): in the project folder or in an
/// open thread's worktree. Recorded ids go stale when a coordinator is
/// reopened or started elsewhere, so they come second: the coordinator's
/// Space, a live coordinator's, or an open local thread's.
fn project_for_space(ctx: &Ctx, panes: &[Pane], mine: &Pane) -> Option<String> {
    let workspace_id = mine.workspace_id.as_str();
    if workspace_id.is_empty() {
        return None;
    }
    let session = crate::core::session_key(ctx.env.this_session());
    let projects: Vec<(Project, crate::project::Coordinator)> = project::list_slugs(&ctx.root)
        .into_iter()
        .filter_map(|slug| Project::load(&ctx.root, &slug).ok())
        .filter_map(|p| p.coordinator().filter(|r| crate::core::session_key(&r.socket) == session).map(|r| (p, r)))
        .collect();
    if projects.is_empty() {
        return None;
    }
    let mut members: Vec<&Pane> = panes.iter().filter(|pane| pane.workspace_id == workspace_id).collect();
    members.sort_by_key(|pane| pane.terminal_id != mine.terminal_id);
    let open_threads = |project: &Project| thread::list(project).into_iter().filter(|t| t.status != thread::Status::Resolved).collect::<Vec<_>>();
    for pane in members {
        for dir in [&pane.foreground_cwd, &pane.cwd].into_iter().filter(|d| !d.is_empty()) {
            let dir = Path::new(dir);
            let found = projects.iter().find(|(project, record)| {
                let home = if record.cwd.is_empty() { project.canonical_dir() } else { PathBuf::from(&record.cwd) };
                dir.starts_with(&home) || open_threads(project).iter().any(|t| t.kind == thread::Kind::Worktree && !t.worktree_path.is_empty() && dir.starts_with(canonical(&t.worktree_path)))
            });
            if let Some((project, _)) = found {
                return Some(project.slug.clone());
            }
        }
    }
    projects
        .iter()
        .find(|(project, record)| {
            record.workspace_id == workspace_id
                || crate::coordinator::live(project).iter().any(|c| c.workspace_id == workspace_id)
                || open_threads(project).iter().any(|t| t.workspace_id == workspace_id)
        })
        .map(|(project, _)| project.slug.clone())
}

/// The path with symlinks resolved when it exists (herdr reports physical
/// working directories).
fn canonical(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Resolved {
    Slug(String),
    /// Nothing resolved and there is no terminal to ask on.
    All,
}

/// An explicit slug, else the project of the terminal this command runs in,
/// else a numbered picker when on a terminal, else every project.
pub fn resolve_slug(ctx: &Ctx, slug: Option<&str>) -> Result<Resolved> {
    if let Some(slug) = slug {
        project::validate_slug(slug)?;
        return Ok(Resolved::Slug(slug.to_string()));
    }
    if let Some(slug) = resolve_slug_quiet(ctx) {
        return Ok(Resolved::Slug(slug));
    }
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return pick(ctx).map(Resolved::Slug);
    }
    Ok(Resolved::All)
}

/// Every project in the root, in listing order.
fn visible_slugs(ctx: &Ctx) -> Vec<String> {
    project::list_slugs(&ctx.root)
}

/// Is this project **in use**? A project has no state of its own: it is either
/// in use or deleted, and "not needed for now" is a closed Space and no
/// coordinator. So the question this view asks is the only one left: is
/// somebody working in it, or is a thread of it still open or starting? Every
/// other project is left out of `overview` and `hp overview --all` is how you
/// see it, exactly as `ls -a` shows what a plain `ls` does not.
fn in_use(ctx: &Ctx, project: &Project) -> bool {
    let open = thread::list(project).iter().any(|t| matches!(t.status, thread::Status::Open | thread::Status::Starting));
    open || coordinator_running(ctx, project)
}

/// A live coordinator: the ticker's own list of coordinator panes
/// (`.state/coordinators.json`, one per lane, written every 15 s), else the
/// pane the project's record names — asked about in the session that record
/// names, so a coordinator in another core session still counts, and asked
/// only when the ticker's file says nothing (a stopped ticker, a root the
/// ticker never saw). A terminal that is alive but reports no status is a
/// shell left behind, not a coordinator (t-0055), so it does not count.
fn coordinator_running(ctx: &Ctx, project: &Project) -> bool {
    if !crate::coordinator::live(project).is_empty() {
        return true;
    }
    let Some(record) = project.coordinator().filter(|r| !r.pane_id.is_empty()) else {
        return false;
    };
    let herdr = Core::new(ctx.env.hc_bin(), &record.socket, ctx.runner);
    herdr.terminal_fate(&record.pane_id).is_ok_and(|terminal| terminal.live() && terminal.reported)
}

/// The numbered project picker.
pub fn pick(ctx: &Ctx) -> Result<String> {
    let slugs = visible_slugs(ctx);
    match slugs.len() {
        0 => bail!("there are no projects in {}", ctx.root.display()),
        1 => return Ok(slugs[0].clone()),
        _ => {}
    }
    for (index, slug) in slugs.iter().enumerate() {
        println!("  {}. {slug}", index + 1);
    }
    print!("Project number: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let choice: usize = line.trim().parse().unwrap_or(0);
    match slugs.get(choice.wrapping_sub(1)) {
        Some(slug) => Ok(slug.clone()),
        None => bail!("no project number {}", line.trim()),
    }
}

/// Threads grouped by state, in the one display order.
pub fn render(project: &Project, rows: &[Row]) -> String {
    // A closed lane leaves every view (§9): its rows — resolved ones included,
    // since a close requires every thread resolved — are not printed. Without
    // a registry `thread_visible` is always true, so nothing changes there.
    let rows: Vec<&Row> = rows.iter().filter(|r| crate::lanes::thread_visible(project, &r.thread.lane)).collect();
    let mut out = String::new();
    let _ = writeln!(out, "{}", project.slug);
    if rows.is_empty() {
        let _ = writeln!(out, "\n  no threads yet");
    }
    for group in Group::DISPLAY_ORDER {
        let members: Vec<&Row> = rows.iter().copied().filter(|r| r.group == group).collect();
        if members.is_empty() {
            continue;
        }
        let _ = writeln!(out, "\n{} ({})", group.label(), members.len());
        for row in members {
            let t = &row.thread;
            let place = if !t.branch.is_empty() {
                t.branch.clone()
            } else if matches!(t.kind, thread::Kind::Local) {
                "local".to_string()
            } else {
                "-".to_string()
            };
            let _ = writeln!(out, "  {}  {}  [{}]  {}", t.id, t.title, row.note, place);
            if group == Group::WaitingOnYou && !t.pane_id.is_empty() && row.note != "pane closed" {
                let _ = writeln!(out, "          needs you in pane {}", t.pane_id);
            }
        }
    }
    out
}

pub fn run(ctx: &Ctx, slug: Option<&str>, all: bool, wait: bool) -> Result<()> {
    let slugs = match resolve_slug(ctx, slug)? {
        // A project named outright is printed whether or not anything runs in
        // it: asking for it is the answer.
        Resolved::Slug(slug) => vec![slug],
        Resolved::All if all => visible_slugs(ctx),
        Resolved::All => visible_slugs(ctx).into_iter().filter(|slug| Project::load(&ctx.root, slug).is_ok_and(|p| in_use(ctx, &p))).collect(),
    };
    if slugs.is_empty() {
        match all {
            true => println!("there are no projects in {}", ctx.root.display()),
            false => println!("no project is in use (nothing runs in any of them; --all lists every project)"),
        }
    }
    for (index, slug) in slugs.iter().enumerate() {
        let project = Project::load(&ctx.root, slug)?;
        if index > 0 {
            println!();
        }
        print!("{}", render(&project, &threads::rows(ctx, &project)));
    }
    // `--wait` is for a person reading it; an agent calling this never waits.
    if wait && std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
        print!("\nPress Enter to close ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_hc::FakeCore;
    use crate::core::L_WS;
    use crate::paths::Env;

    /// A projects root plus a fake core that answers `hc`: the two things
    /// identity resolution reads. `HC_SESSION` stays unset, which is the
    /// default core.
    struct World {
        home: tempfile::TempDir,
        core: FakeCore,
    }

    impl World {
        fn new() -> World {
            World { home: tempfile::tempdir().unwrap(), core: FakeCore::new() }
        }

        fn project(&self, name: &str) -> Project {
            project::create(&self.home.path().join("root"), name, vec![]).unwrap()
        }

        fn dir(&self, project: &Project) -> String {
            project.canonical_dir().to_string_lossy().into_owned()
        }

        /// A `Ctx` as the given core terminal would see it; `terminal` empty
        /// stands for a process core started for no terminal at all.
        fn ctx(&self, terminal: &str) -> Ctx<'_> {
            let env = Env::for_test(self.home.path(), &[("HC_TERMINAL_ID", terminal)]);
            Ctx {
                env: Box::leak(Box::new(env)),
                root: self.home.path().join("root"),
                config_dir: self.home.path().join("cfg"),
                runner: &self.core,
                detached_ticker: false,
            }
        }
    }

    #[test]
    fn a_project_line_has_no_goal_tail() {
        let world = World::new();
        let with_old_goal = world.project("alpha");
        let plain = world.project("beta");
        // An older PROJECT.md still carries the line; the line is not a view.
        std::fs::write(with_old_goal.project_md(), "+++\nname = \"alpha\"\ngoal = \"monthly invoices\"\n+++\n").unwrap();
        let text = render(&with_old_goal, &[]);
        assert_eq!(text.lines().next(), Some("alpha"), "{text}");
        assert!(!text.contains("monthly invoices"), "{text}");
        let text = render(&plain, &[]);
        assert_eq!(text.lines().next(), Some("beta"), "{text}");
    }

    #[test]
    fn a_command_in_a_projects_terminal_reads_that_project() {
        let world = World::new();
        let alpha = world.project("alpha");
        let beta = world.project("beta");
        // Alpha is first by name, so reading `beta` back proves the terminal
        // decided it and not the list order — the bug t-0040 fixed.
        let alpha_pane = world.core.terminal("hp-alpha", &world.dir(&alpha), &[(crate::labels::PROJECT_TOKEN, "alpha"), (L_WS, "ws-alpha")]);
        let beta_pane = world.core.terminal("hp-beta", &world.dir(&beta), &[(crate::labels::PROJECT_TOKEN, "beta"), (L_WS, "ws-beta")]);
        let ctx = world.ctx(&beta_pane.address);
        assert_eq!(resolve_slug_quiet(&ctx).as_deref(), Some("beta"));
        assert_eq!(resolve_slug(&ctx, None).unwrap(), Resolved::Slug("beta".into()));
        let ctx = world.ctx(&alpha_pane.address);
        assert_eq!(resolve_slug_quiet(&ctx).as_deref(), Some("alpha"));
        assert_eq!(resolve_slug(&ctx, None).unwrap(), Resolved::Slug("alpha".into()));
    }

    #[test]
    fn a_pane_with_no_label_is_known_by_its_space() {
        let world = World::new();
        let alpha = world.project("alpha");
        world.project("beta");
        let dir = world.dir(&alpha);
        // The project's own record: a coordinator opened in the default
        // session, in this Space.
        alpha
            .update_coordinator(|record| {
                record.socket = String::new();
                record.workspace_id = "ws-alpha".into();
                record.cwd = dir.clone();
            })
            .unwrap();
        // A plain shell in the project folder: it carries no `hp_project`.
        let shell = world.core.terminal("hp-alpha-shell", &dir, &[(L_WS, "ws-alpha")]);
        let ctx = world.ctx(&shell.address);
        assert_eq!(resolve_slug_quiet(&ctx).as_deref(), Some("alpha"));
        // A pane that drifted out of the folder says nothing about a project by
        // where it works (a coordinator often sits in `~`); the Space id it was
        // recorded with still does.
        let drifted = world.core.terminal("hp-alpha-coordinator", world.home.path().to_str().unwrap(), &[(L_WS, "ws-alpha")]);
        let ctx = world.ctx(&drifted.address);
        assert_eq!(resolve_slug_quiet(&ctx).as_deref(), Some("alpha"));
    }

    #[test]
    fn an_unrecognized_terminal_resolves_to_nothing_never_to_one_project() {
        let world = World::new();
        let alpha = world.project("alpha");
        world.core.terminal("hp-alpha", &world.dir(&alpha), &[(crate::labels::PROJECT_TOKEN, "alpha"), (L_WS, "ws-alpha")]);
        // A terminal this session does not list: another session, a closed tab.
        let ctx = world.ctx("t-gone");
        assert_eq!(resolve_slug_quiet(&ctx), None);
        assert_eq!(resolve_slug(&ctx, None).unwrap(), Resolved::All, "an unknown pane reads every project, not the first one");
        // No terminal at all: the ticker, the app, a test.
        let ctx = world.ctx("");
        assert_eq!(resolve_slug_quiet(&ctx), None);
        // A label naming a project that is not there is no answer either.
        let ghost = world.core.terminal("hp-ghost", "/tmp", &[(crate::labels::PROJECT_TOKEN, "ghost"), (L_WS, "ws-ghost")]);
        let ctx = world.ctx(&ghost.address);
        assert_eq!(resolve_slug_quiet(&ctx), None);
    }
}
