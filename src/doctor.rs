//! `doctor`: what is installed, where things resolve, and whether it fits.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use crate::herdr::{self, Herdr};
use crate::paths::{self, Ctx, Env, SessionFlags};
use crate::project;
use crate::runner::{Cmd, Runner};

const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// GitHub hosts of the `origin` remotes of this directory's repository and
/// of each project's local repositories, with one `owner/repo` on each.
fn github_hosts(root: &Path, runner: &dyn Runner) -> std::collections::BTreeMap<String, String> {
    let mut dirs: Vec<String> = std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).into_iter().collect();
    for slug in project::list_slugs(root) {
        if let Ok((settings, _)) = project::Project::load(root, &slug).and_then(|p| p.read_project_md()) {
            dirs.extend(settings.repos.into_iter().map(|r| r.path));
        }
    }
    let mut hosts = std::collections::BTreeMap::new();
    for dir in dirs {
        let Ok(out) = runner.run(&Cmd::new("git", TOOL_TIMEOUT).args(["-C", &dir, "remote", "get-url", "origin"])) else {
            continue;
        };
        if let Some(remote) = out.success().then(|| crate::pr::parse_remote(&out.stdout)).flatten() {
            hosts.entry(remote.host).or_insert(remote.repo);
        }
    }
    hosts
}

/// Prints the report and returns whether every required check passed. With
/// `fix`, repairs what the binary owns: the `herdr-projects` link on `PATH`, priming files, `uploads/`, the
/// absolute binary path they carry, and the skill link for a configured harness. Never edits another plugin's entries.
pub fn run(ctx: &Ctx, session: &SessionFlags, fix: bool) -> Result<bool> {
    let (text, healthy) = report(ctx.env, &ctx.root, &ctx.config_dir, session, ctx.runner, fix);
    print!("{text}");
    Ok(healthy)
}

/// §5.4's promise for `doctor`: a pane that is alive but cannot be tied to its
/// lane — an `Ambiguous` pane, or a lane whose recorded pane is alive but no
/// rule recognizes it (`degraded`) — is reported with the command that fixes
/// it. Empty without a lane registry, so a pre-lane project's report is byte
/// for byte what it was.
fn lane_notes(project: &project::Project, agents: &[crate::herdr::Agent]) -> Vec<String> {
    if !crate::lanes::exists(project) {
        return Vec::new();
    }
    let view = crate::lanes::View {
        registry: crate::lanes::Registry::for_project(project),
        agents: agents.to_vec(),
        home: crate::lanes::project_home(project),
        slug: project.slug.clone(),
        this_pane: String::new(),
        reachable: true,
        counts: Default::default(),
    };
    let mut notes: Vec<String> = view
        .sightings()
        .iter()
        .filter_map(|sighting| match &sighting.resolution {
            crate::lanes::LaneResolution::Ambiguous { reason } => {
                Some(format!("pane {} is ambiguous: {reason}", sighting.pane_id))
            }
            _ => None,
        })
        .collect();
    for status in view.lane_statuses().iter().filter(|s| !s.orphan && !s.shown) {
        let name = view
            .registry
            .get(&status.lane_id)
            .map(|lane| if lane.label.is_empty() { lane.name.clone() } else { lane.label.clone() })
            .unwrap_or_default();
        let lane = if name.is_empty() { status.lane_id.clone() } else { format!("{} ({name})", status.lane_id) };
        notes.push(format!(
            "lane {lane} is alive in pane {} but unrecognized ({}); run `hp lane adopt {}` in it",
            status.pane_id.as_deref().unwrap_or(""),
            status.why,
            status.lane_id,
        ));
    }
    notes
}

fn report(
    env: &Env,
    root: &Path,
    config_dir: &Path,
    session: &SessionFlags,
    runner: &dyn Runner,
    fix: bool,
) -> (String, bool) {
    let mut out = String::new();
    let mut healthy = true;
    let mut check = |out: &mut String, ok: Option<bool>, label: &str, detail: String| {
        let mark = match ok {
            Some(true) => "ok  ",
            Some(false) => {
                healthy = false;
                "FAIL"
            }
            None => "warn",
        };
        let _ = writeln!(out, "[{mark}] {label}: {detail}");
    };

    let binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("unknown ({e})"));
    let _ = writeln!(out, "binary:     {binary}");
    let _ = writeln!(out, "version:    {}", crate::VERSION);
    let _ = writeln!(out, "root:       {}", root.display());
    let _ = writeln!(out, "config dir: {}", config_dir.display());
    let _ = writeln!(out);

    if let Some(latest) = crate::update::newer_release(runner, crate::update::own_root().as_deref()) {
        check(&mut out, None, "update", format!("a newer version is available ({latest}): run `hp update`"));
    }

    let app = env.herdr_app_bin();
    if crate::app::running(runner) {
        check(&mut out, Some(true), "app", format!("{app}: this project's terminals are shown in its spaces and tabs"));
    } else {
        check(&mut out, None, "app", format!("{app} is not running: terminals start in core and are reached with `hc attach`"));
    }

    let bin = env.herdr_bin();
    match herdr::version(&bin, runner) {
        Ok(version) if version >= herdr::MIN_VERSION => {
            check(&mut out, Some(true), "herdr", format!("{version} ({bin})"))
        }
        Ok(version) => check(
            &mut out,
            Some(false),
            "herdr",
            format!("{version} ({bin}); {} or later is required", herdr::MIN_VERSION),
        ),
        Err(error) => check(&mut out, Some(false), "herdr", format!("{error:#}")),
    }

    {
        let binary = paths::binary().unwrap_or_default();
        let path_var = crate::USER_PATH.get().map(String::as_str).or(env.var("PATH")).unwrap_or("");
        let (ok, detail) = crate::command_link::check(env, &binary, path_var, fix);
        check(&mut out, ok, "command", detail);
    }

    match paths::resolve_session(session, env, runner) {
        Ok(found) => {
            let reachable = Herdr::new(&bin, &found.socket, runner).reachable();
            // Never print an empty name: the default core is a real session, it
            // just has no `HC_SESSION` to be named by.
            let (label, origin) = match found.name.as_deref() {
                Some(name) => (name.to_string(), "named session"),
                None => ("default".to_string(), "the default core; --session or HC_SESSION names another"),
            };
            check(
                &mut out,
                if reachable { Some(true) } else { None },
                "session",
                format!(
                    "{label} ({origin}){}",
                    if reachable { "" } else { "; not reachable" }
                ),
            );
        }
        Err(error) => check(&mut out, Some(false), "session", format!("{error:#}")),
    }

    for (tool, args, required) in [
        ("git", vec!["--version"], true),
        ("ssh", vec!["-V"], true),
        ("rsync", vec!["--version"], false),
        ("gh", vec!["--version"], false),
    ] {
        let result = runner.run(&Cmd::new(tool, TOOL_TIMEOUT).args(args));
        match result {
            Ok(o) if o.success() => {
                let text = if o.stdout.trim().is_empty() { &o.stderr } else { &o.stdout };
                let line = text.lines().next().unwrap_or("").trim().to_string();
                check(&mut out, Some(true), tool, line);
            }
            Ok(o) => check(&mut out, required.then_some(false), tool, o.error_text()),
            Err(error) => check(&mut out, required.then_some(false), tool, format!("{error:#}")),
        }
    }
    // `gh` per GitHub host the repositories use: the one in this directory
    // and each project's local ones. Off `github.com` the check runs as the
    // pull request follow-up does, with `GH_HOST` set.
    let hosts = github_hosts(root, runner);
    if hosts.is_empty() || hosts.contains_key("github.com") {
        match runner.run(&Cmd::new("gh", TOOL_TIMEOUT).args(["auth", "status"])) {
            Ok(o) if o.success() => check(&mut out, Some(true), "gh auth", "logged in".into()),
            Ok(o) => check(
                &mut out,
                None,
                "gh auth",
                format!(
                    "{}; pull request follow-up will not work",
                    o.error_text().lines().next().unwrap_or("not logged in")
                ),
            ),
            Err(_) => check(&mut out, None, "gh auth", "gh is not installed".into()),
        }
    }
    for (host, repo) in hosts.iter().filter(|(host, _)| host.as_str() != "github.com") {
        let label = format!("github {host}");
        let result = runner.run(&crate::pr::gh(host).args(["api", "user", "--jq", ".login"]));
        match result {
            Ok(o) if o.success() => check(&mut out, Some(true), &label, format!("reachable as {} (GH_HOST={host}, used for {repo})", crate::pr::sanitize(o.stdout.trim()))),
            Ok(o) => check(
                &mut out,
                None,
                &label,
                format!(
                    "gh cannot reach {host} ({}); pull requests of {repo} are not followed. Try `GH_HOST={host} gh auth status`",
                    o.error_text().lines().next().unwrap_or("failed")
                ),
            ),
            Err(error) => check(&mut out, None, &label, format!("gh cannot reach {host} ({error:#}); pull requests of {repo} are not followed")),
        }
    }

    if root.is_dir() {
        let count = project::list_slugs(root).len();
        check(&mut out, Some(true), "root", format!("{count} project(s)"));
    } else {
        check(
            &mut out,
            None,
            "root",
            "does not exist yet; `new` creates it".into(),
        );
    }

    match crate::ticker::lock_state(root) {
        crate::ticker::LockState::Free => check(&mut out, None, "ticker", "not running".into()),
        crate::ticker::LockState::Held(info) => check(
            &mut out,
            Some(true),
            "ticker",
            format!(
                "running, version {} (this binary: {}), root {}",
                info.version,
                crate::VERSION,
                info.root
            ),
        ),
    }

    // Every project's priming files, and the binary path they carry: a
    // `plugin link` from another checkout or a moved plugin root breaks them
    // silently, and `--fix` rewrites them.
    let prefix = crate::coordinator::current_prefix(root).unwrap_or_default();
    let slugs = project::list_slugs(root);
    for slug in &slugs {
        let Ok(project) = project::Project::load(root, slug) else {
            continue;
        };
        let label = format!("files {slug}");
        let problems = project::priming_problems(&project, &prefix);
        if problems.is_empty() {
            check(&mut out, Some(true), &label, "AGENTS.md, CLAUDE.md link and uploads/ are in place".into());
        } else if fix {
            match project::write_priming(&project, &prefix, "doctor-fix") {
                Ok(()) => check(&mut out, Some(true), &label, format!("fixed: {}", problems.join("; "))),
                Err(error) => check(&mut out, Some(false), &label, format!("could not fix ({error:#}): {}", problems.join("; "))),
            }
        } else {
            check(&mut out, None, &label, format!("{}; `doctor --fix` repairs this", problems.join("; ")));
        }
        // The routine journal is append-only and never rotated: a hint, never a
        // failure, once it is long enough that a person may want to clean it.
        if let Some(lines) = crate::routine::journal_lines(&project).filter(|lines| *lines > 2000) {
            check(&mut out, None, &format!("journal {slug}"), format!("routine-journal.log has {lines} lines; it is never rotated, so delete it once the history is no longer needed"));
        }
        for other in &slugs {
            if other > slug && crate::names::collide(slug, other) {
                check(&mut out, Some(false), &format!("names {slug}"), format!("its agent names collide with `{other}` after truncation to 32 characters; rename one project"));
            }
        }
    }

    // Profiles: config.toml loads, and each project's defaults exist and are
    // allowed, or its threads and coordinator do not start.
    match crate::profiles::load(config_dir) {
        Err(error) => check(&mut out, Some(false), "profiles", format!("{error:#}")),
        Ok(config) => {
            let detected = crate::profiles::detect(env);
            check(&mut out, Some(true), "profiles", format!("{} of yours; built-ins (installed and signed in): {}", config.profiles.len(), if detected.is_empty() { "none".to_string() } else { detected.join(", ") }));
            for slug in &slugs {
                let Ok(project) = project::Project::load(root, slug) else {
                    continue;
                };
                let (Ok((settings, _)), Ok(safety)) = (project.read_project_md(), project.safety(config_dir)) else {
                    continue;
                };
                for role in [crate::profiles::Role::Thread, crate::profiles::Role::Coordinator] {
                    if let Err(error) = crate::profiles::resolve(&config, &safety, &settings, role, None, slug) {
                        check(&mut out, Some(false), &format!("profiles {slug}"), format!("{} {error:#}", role.default_key()));
                    }
                }
            }
        }
    }

    // Orphans, as `sweep --dry-run` would list them.
    {
        let ctx = Ctx { env, root: root.to_path_buf(), config_dir: config_dir.to_path_buf(), runner, detached_ticker: false };
        for slug in &slugs {
            let Ok(project) = project::Project::load(root, slug) else {
                continue;
            };
            let orphans = crate::sweep::find(&ctx, &project);
            if !orphans.is_empty() {
                let list: Vec<String> = orphans.iter().map(|o| o.describe()).collect();
                check(&mut out, None, &format!("sweep {slug}"), format!("{}; `sweep {slug}` removes them", list.join("; ")));
            }
        }
    }

    for slug in &slugs {
        let Ok(project) = project::Project::load(root, slug) else {
            continue;
        };
        let label = format!("project {slug}");
        let none = format!("no coordinator running (start any agent in {}, or `open {slug}`)", project.dir().display());
        // With no usable record, the agents the ticker would discover: the
        // ones working in the project folder in this session.
        let (record, mut notes) = match project.coordinator() {
            Some(record) if crate::herdr::session_alive(&bin, &record.socket, runner) => (Some(record), Vec::new()),
            Some(record) => (None, vec![format!("recorded session `{}` no longer runs", record.socket)]),
            None => (None, Vec::new()),
        };
        let socket = match (&record, paths::resolve_session(session, env, runner)) {
            (Some(record), _) => record.socket.clone(),
            (None, Ok(found)) => found.socket,
            (None, Err(_)) => {
                notes.push(none);
                check(&mut out, Some(true), &label, format!("{}; {}", project.status(), notes.join("; ")));
                continue;
            }
        };
        let herdr = Herdr::new(&bin, &socket, runner);
        match (herdr.pane_list(), herdr.agent_list()) {
            (Ok(panes), Ok(agents)) => {
                let dir = project.canonical_dir().to_string_lossy().into_owned();
                let coordinators: Vec<String> = agents
                    .iter()
                    .filter(|a| a.works_in(&dir))
                    .map(|a| if a.name.starts_with("hpc-") { format!("{} ({})", a.pane_id, a.agent) } else { format!("{} ({}, started by hand)", a.pane_id, a.agent) })
                    .collect();
                if let Some(record) = &record {
                    let workspace = crate::coordinator::workspace_open(record, &panes);
                    notes.push(format!("socket {}; workspace {} {}", record.socket, record.workspace_id, if workspace { "open" } else { "closed" }));
                }
                if coordinators.is_empty() {
                    notes.push(none);
                } else {
                    notes.push(format!("coordinator: {}", coordinators.join(", ")));
                    if record.is_none() {
                        notes.push("the ticker records it on its next tick".into());
                    }
                }
                check(
                    &mut out,
                    if coordinators.is_empty() && record.is_some() { None } else { Some(true) },
                    &label,
                    format!("{}; {}", project.status(), notes.join("; ")),
                );
                // §5.4: a lane pane that is alive but unrecognizable is a
                // warning, never a FAIL — the project still works, but the
                // alternative is the silent stall this line exists to end.
                for note in lane_notes(&project, &agents) {
                    check(&mut out, None, &format!("lanes {slug}"), note);
                }
            }
            (Err(error), _) | (_, Err(error)) => check(&mut out, None, &label, format!("session at {socket} unreachable: {error}")),
        }
    }

    // Scheduled routines whose last due run did nothing: no coordinator ran.
    for slug in &slugs {
        let Ok(project) = project::Project::load(root, slug) else {
            continue;
        };
        let states = crate::steps::load_state(&project).routines;
        let now = jiff::Zoned::now();
        let skipped: Vec<String> = crate::routine::load_all(&project)
            .0
            .iter()
            .filter(|r| r.enabled && states.get(&r.name).is_some_and(|s| s.no_coordinator > 0))
            .map(|r| format!("{}: {}", r.name, crate::routine::when_text(r, states.get(&r.name), &now)))
            .collect();
        if !skipped.is_empty() {
            check(&mut out, None, &format!("routines {slug}"), format!("{}; routines run only while a coordinator runs", skipped.join("; ")));
        }
    }

    (out, healthy)
}
