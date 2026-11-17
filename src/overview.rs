//! The text overview, and how commands without a slug find their project.

use std::fmt::Write as _;
use std::io::{BufRead, IsTerminal, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::paths::Ctx;
use crate::project::{self, Project, Status};
use crate::thread::{self, Group};
use crate::threads::{self, Row};

/// The project a herdr workspace belongs to, only among projects whose
/// recorded socket is the current one (workspace ids repeat across sessions).
/// First by where the workspace's panes work (the focused pane first): in the
/// project folder or in an open thread's worktree. Recorded ids go stale when
/// a coordinator is reopened or started elsewhere, so they come second: the
/// coordinator's workspace, a live coordinator's, or an open local thread's.
pub fn project_for_workspace(ctx: &Ctx, workspace_id: &str, socket: &str) -> Option<String> {
    if workspace_id.is_empty() || socket.is_empty() {
        return None;
    }
    let projects: Vec<(Project, crate::project::Coordinator)> = project::list_slugs(&ctx.root)
        .into_iter()
        .filter_map(|slug| Project::load(&ctx.root, &slug).ok())
        .filter(|p| p.status() != Status::Archived)
        .filter_map(|p| p.coordinator().filter(|r| r.socket == socket).map(|r| (p, r)))
        .collect();
    if projects.is_empty() {
        return None;
    }
    let herdr = crate::herdr::Herdr::new(ctx.env.herdr_bin(), socket, ctx.runner);
    let mut panes: Vec<crate::herdr::Pane> = herdr.pane_list().unwrap_or_default().into_iter().filter(|p| p.workspace_id == workspace_id).collect();
    let focused = ctx.env.var("HERDR_PANE_ID").unwrap_or("");
    panes.sort_by_key(|p| p.pane_id != focused);
    let open_threads = |project: &Project| thread::list(project).into_iter().filter(|t| t.status != thread::Status::Resolved).collect::<Vec<_>>();
    for pane in &panes {
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

/// The current workspace's project, without asking.
pub fn resolve_slug_quiet(ctx: &Ctx) -> Option<String> {
    let workspace = ctx.env.var("HERDR_WORKSPACE_ID").unwrap_or("");
    let socket = ctx.env.var("HERDR_SOCKET_PATH").unwrap_or("");
    project_for_workspace(ctx, workspace, socket)
}

pub enum Resolved {
    Slug(String),
    /// Nothing resolved and there is no terminal to ask on.
    All,
}

/// An explicit slug, else the current herdr workspace, else a numbered picker
/// when on a terminal, else every project.
pub fn resolve_slug(ctx: &Ctx, slug: Option<&str>) -> Result<Resolved> {
    if let Some(slug) = slug {
        project::validate_slug(slug)?;
        return Ok(Resolved::Slug(slug.to_string()));
    }
    let workspace = ctx.env.var("HERDR_WORKSPACE_ID").unwrap_or("");
    let socket = ctx.env.var("HERDR_SOCKET_PATH").unwrap_or("");
    if let Some(slug) = project_for_workspace(ctx, workspace, socket) {
        return Ok(Resolved::Slug(slug));
    }
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return pick(ctx).map(Resolved::Slug);
    }
    Ok(Resolved::All)
}

/// The slug to act on, for commands that cannot act on "all projects".
fn visible_slugs(ctx: &Ctx) -> Vec<String> {
    project::list_slugs(&ctx.root)
        .into_iter()
        .filter(|slug| Project::load(&ctx.root, slug).is_ok_and(|p| p.status() != Status::Archived))
        .collect()
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
    let goal = project.read_project_md().map(|(s, _)| s.goal).unwrap_or_default();
    let _ = write!(out, "{} ({})", project.slug, project.status());
    if !goal.is_empty() {
        let _ = write!(out, " — {goal}");
    }
    let _ = writeln!(out);
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
            } else if matches!(t.kind, thread::Kind::Tab | thread::Kind::Home) {
                "tab".to_string()
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

pub fn run(ctx: &Ctx, slug: Option<&str>, wait: bool) -> Result<()> {
    let slugs = match resolve_slug(ctx, slug)? {
        Resolved::Slug(slug) => vec![slug],
        Resolved::All => visible_slugs(ctx),
    };
    if slugs.is_empty() {
        println!("there are no projects in {}", ctx.root.display());
    }
    for (index, slug) in slugs.iter().enumerate() {
        let project = Project::load(&ctx.root, slug)?;
        if index > 0 {
            println!();
        }
        print!("{}", render(&project, &threads::rows(ctx, &project)));
    }
    // Only a popup wants to be held open; an agent calling this never waits.
    if wait && std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
        print!("\nPress Enter to close ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
    }
    Ok(())
}
