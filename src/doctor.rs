//! `doctor`: what is installed, where things resolve, and whether it fits.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use crate::core::{self, Core};
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
fn lane_notes(project: &project::Project, agents: &[crate::core::Agent]) -> Vec<String> {
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

/// `tasks <slug>`: the blank lines that `hp task add`, `move` and `delete`
/// leave behind, since they only ever move whole task blocks. Read only and a
/// hint, never a failure: the file still parses, and only `hp task tidy` may
/// change it. Pruning is off here: an empty `## Backlog` is the skeleton `hp
/// task add` writes, so reporting it would warn on every fresh project (and
/// `tidy` would then delete what `add` puts back).
fn tasks_line(project: &project::Project) -> (Option<bool>, String) {
    let findings = crate::tasks::untidiness(&crate::tasks::read(&project.dir()), false);
    if findings.is_empty() {
        return (Some(true), "TASKS.md is tidy".into());
    }
    let shown: Vec<String> = findings.iter().take(2).map(crate::tasks::Finding::summary).collect();
    let more = if findings.len() > 2 { format!(" (and {} more)", findings.len() - 2) } else { String::new() };
    (None, format!("{}{more}; `hp task tidy {}` cleans them", shown.join("; "), project.slug))
}

/// How long `doctor --fix` waits for the ticker it just loaded to take the lock.
const AGENT_WAIT: Duration = Duration::from_secs(10);

/// `ticker agent`: the LaunchAgent that starts the ticker after a reboot and
/// after any exit, and the loop it is supposed to be starting. `None` where this
/// machine has no LaunchAgents, so `doctor` there prints exactly what it printed
/// before.
///
/// `Some((None, …))` is the warning the 2026-11-17 accident asks for: the ticker
/// may be running right now, but nothing will start it again — or launchd has
/// the plist and never loaded it, which is the state the second disappearance
/// left behind (`launchctl print` empty while the file sat on disk). `fix`
/// rewrites and loads the plist, and checks that a process really took the
/// lock rather than trusting `launchctl bootstrap` alone. It never unloads a
/// job a running ticker holds (2026-11-19: that SIGTERM cost seven hours), so
/// where the job is not loaded it says so instead of killing the loop.
fn ticker_agent_line(env: &Env, root: &Path, runner: &dyn Runner, fix: bool, running: bool) -> Option<(Option<bool>, String)> {
    let binary = paths::binary().ok()?;
    let state = crate::ticker::agent_state(&env.home, &binary, root, runner)?;
    let path = state.path.display();
    if fix && !(state.installed && state.current && state.loaded && running) {
        return Some(match crate::ticker::reload_agent(&env.home, &binary, root, runner) {
            Ok(report) if report.kept_running => {
                let what = if report.wrote_plist { format!("wrote {path}") } else { format!("{path} is written") };
                (
                    Some(true),
                    format!("{what}; the ticker that is running was left alone, so launchd has no job for it until it next starts one — `hp ticker start` after it stops"),
                )
            }
            Ok(_) => {
                let loaded = crate::ticker::agent_state(&env.home, &binary, root, runner).is_some_and(|after| after.installed && after.current && after.loaded);
                let started = running || crate::ticker::wait_for_ticker(root, AGENT_WAIT);
                match (loaded, started) {
                    (true, true) => (Some(true), format!("fixed: {path} is written (KeepAlive is unconditional), loaded, and a ticker holds the lock")),
                    (true, false) => (
                        Some(false),
                        format!("reloaded {path}, but no ticker took the lock within {}s; read {}", AGENT_WAIT.as_secs(), root.join(".ticker.log").display()),
                    ),
                    (false, _) => (Some(false), format!("wrote {path} but launchd did not load it; run `hp ticker start`")),
                }
            }
            Err(error) => (Some(false), format!("could not install {path}: {error:#}")),
        });
    }
    if !state.installed {
        // Between the two labels: this root still runs under the shared
        // `dev.hp.ticker` job, which keeps the ticker alive exactly as the
        // derived one would. Not a warning, and named, because `ticker stop`
        // and `ticker start` is what moves it (t-0083).
        if let Some(legacy) = &state.legacy {
            return Some((
                state.loaded.then_some(true),
                if state.loaded {
                    format!(
                        "{path} is not written yet; this root still runs under the old shared LaunchAgent {}, and `hp ticker stop && hp ticker start` moves it to the label `{}`",
                        legacy.display(),
                        crate::ticker::agent_label(root)
                    )
                } else {
                    format!(
                        "{} is this root's old shared LaunchAgent but launchd has not loaded it, and {path} is not written; run `hp ticker stop && hp ticker start`",
                        legacy.display()
                    )
                },
            ));
        }
        return Some((
            None,
            format!(
                "no LaunchAgent at {path}: the ticker dies with its terminal and is not started after a reboot; run `hp ticker start` (or `hp doctor --fix`), which writes the plist and loads it"
            ),
        ));
    }
    if !state.current {
        return Some((
            None,
            format!(
                "{path} is out of date (its KeepAlive does not restart every exit); run `hp doctor --fix` (or `hp ticker start`), which rewrites the plist and loads it again"
            ),
        ));
    }
    if !state.loaded {
        return Some((
            None,
            format!(
                "{path} is written (label `{}`) but launchd has not loaded it, so nothing starts the ticker; run `hp doctor --fix` or `hp ticker start` to load it (`launchctl bootstrap gui/<uid> {path}`)",
                crate::ticker::agent_label(root)
            ),
        ));
    }
    Some((Some(true), format!("{path} is written (KeepAlive is unconditional) and loaded")))
}

/// The whole report, as text, and whether every required check passed.
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
    // Which LaunchAgent belongs to this root: the label is derived from the
    // canonical root (t-0083), so naming both halves here is what lets a
    // person tell two roots' jobs apart — the real root's and a scratch
    // root's (`scripts/dev-hp`) now have one each.
    if cfg!(target_os = "macos") {
        let _ = writeln!(
            out,
            "ticker:     label {}, plist {}",
            crate::ticker::agent_label(root),
            crate::ticker::agent_path(&env.home, root).display()
        );
    }
    let _ = writeln!(out);

    let app = env.herdr_app_bin();
    if crate::app::running(runner) {
        check(&mut out, Some(true), "app", format!("{app}: this project's terminals are shown in its spaces and tabs"));
    } else {
        check(&mut out, None, "app", format!("{app} is not running: terminals start in core and are reached with `hc attach`"));
    }

    let bin = env.hc_bin();
    match core::version(&bin, runner) {
        Ok(version) if version >= core::MIN_VERSION => {
            check(&mut out, Some(true), "herdr", format!("{version} ({bin})"))
        }
        Ok(version) => check(
            &mut out,
            Some(false),
            "herdr",
            format!("{version} ({bin}); {} or later is required", core::MIN_VERSION),
        ),
        Err(error) => check(&mut out, Some(false), "herdr", format!("{error:#}")),
    }

    {
        let binary = paths::binary().unwrap_or_default();
        let path_var = crate::USER_PATH.get().map(String::as_str).or(env.var("PATH")).unwrap_or("");
        let (ok, detail) = crate::command_link::check(env, &binary, path_var, fix);
        check(&mut out, ok, "command", detail);
    }

    match paths::resolve_session(session, env) {
        Ok(found) => {
            let reachable = Core::new(&bin, &found.socket, runner).reachable();
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

    let running = match crate::ticker::lock_state(root) {
        crate::ticker::LockState::Free => {
            check(
                &mut out,
                None,
                "ticker",
                "not running; run `hp ticker start` (it installs the LaunchAgent and starts the loop)".into(),
            );
            false
        }
        crate::ticker::LockState::Held(info) => {
            check(
                &mut out,
                Some(true),
                "ticker",
                format!(
                    "running, version {} (this binary: {}), root {}",
                    info.version,
                    crate::VERSION,
                    info.root
                ),
            );
            true
        }
    };

    // A running ticker is not enough: without a loaded LaunchAgent it dies with
    // its terminal and nothing starts it again (2026-11-17: the loop vanished
    // with no stop line as well). Worth a warning even while it runs.
    if let Some((ok, detail)) = ticker_agent_line(env, root, runner, fix, running) {
        check(&mut out, ok, "ticker agent", detail);
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
        let (mark, detail) = tasks_line(&project);
        check(&mut out, mark, &format!("tasks {slug}"), detail);
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

    // What the window draws, read once: a coordinator's Space and tab names are
    // the app's to set, and a name that never landed is silent otherwise
    // (t-0051). The ticker renames both on every tick, so this warns rather
    // than fails.
    let app_view = crate::app::running(runner).then(|| crate::app::ls(runner)).flatten();

    for slug in &slugs {
        let Ok(project) = project::Project::load(root, slug) else {
            continue;
        };
        let label = format!("project {slug}");
        let none = format!("no coordinator running (start any agent in {}, or `open {slug}`)", project.dir().display());
        // With no usable record, the agents the ticker would discover: the
        // ones working in the project folder in this session.
        let (record, mut notes) = match project.coordinator() {
            Some(record) if crate::core::session_alive(&bin, &record.socket, runner) => (Some(record), Vec::new()),
            Some(record) => (None, vec![format!("recorded session `{}` no longer runs", record.socket)]),
            None => (None, Vec::new()),
        };
        let socket = match (&record, paths::resolve_session(session, env)) {
            (Some(record), _) => record.socket.clone(),
            (None, Ok(found)) => found.socket,
            (None, Err(_)) => {
                notes.push(none);
                check(&mut out, Some(true), &label, notes.join("; "));
                continue;
            }
        };
        let herdr = Core::new(&bin, &socket, runner);
        match (herdr.pane_list(), herdr.agent_list()) {
            (Ok(panes), Ok(agents)) => {
                let dir = project.canonical_dir().to_string_lossy().into_owned();
                let coordinators = coordinator_rows(&project, record.as_ref(), &agents, &dir);
                if let Some(record) = &record {
                    let workspace = crate::coordinator::workspace_open(record, &panes);
                    notes.push(format!("socket {}; workspace {} {}", record.socket, record.workspace_id, if workspace { "open" } else { "closed" }));
                }
                if coordinators.is_empty() {
                    // Why it is empty matters: a coordinator whose terminal core
                    // reaped is invisible to every list, and the difference
                    // between "you closed the tab" and "the ticker can never
                    // prompt it again" is the whole 2026-11-18 incident.
                    if let Some(record) = &record
                        && !record.pane_id.is_empty()
                        && let fate = crate::coordinator::fate(&herdr, &project, record, &agents, &crate::lanes::default_lane_id(&project))
                        && !matches!(fate, crate::coordinator::Fate::Running)
                    {
                        let detail = match &fate {
                            // Named pane and named missing labels on purpose: "no
                            // coordinator running" sends a person to `open`, which
                            // would start a second coordinator beside this live one.
                            crate::coordinator::Fate::Unlabeled { .. } => format!(
                                "pane {} belongs to `{slug}` and its program reports a status, but it carries no `hp_agent`/`hp_kind`, so it does not count as an agent anywhere in hp. The ticker writes both labels on its next tick (about 15 seconds); do not `open` a second coordinator before that",
                                record.pane_id
                            ),
                            crate::coordinator::Fate::Lost { detail } => format!(
                                "{detail}; the ticker holds its inbox items instead of prompting, so `open {slug}` is the fix (its records are not rewritten here)"
                            ),
                            crate::coordinator::Fate::Moved(agent) => format!(
                                "its agent is running in pane {} while the record still names {}; the ticker follows it on its next tick",
                                agent.pane_id, record.pane_id
                            ),
                            crate::coordinator::Fate::Running => String::new(),
                        };
                        notes.push(detail);
                    }
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
                    notes.join("; "),
                );
                if let Some(view) = &app_view {
                    for note in coordinator_name_notes(&herdr, &project, view, record.as_ref()) {
                        check(&mut out, None, &format!("names {slug}"), note);
                    }
                }
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

/// The coordinators of one project, as its `project <slug>` line names them:
/// the lane resolver's answer first (token, record + session, folder), then a
/// hand-started agent working in the project folder that claims no other
/// project.
///
/// **Not** the folder alone. A coordinator's shell sits wherever it last `cd`-ed
/// to: law's pane t5h has a shell in rexa's folder while the pane says
/// `hp_project=law`, and judging by the folder put law's coordinator in rexa's
/// row and left law's own row saying "no coordinator running" — two lies from
/// one line, each pointing at the wrong fix (2026-11-18, t-0053). A pane whose
/// `hp_project` names another project is that project's, whatever its shell's
/// directory says; the folder stays only as the weakest signal, for an agent hp
/// never labelled at all.
fn coordinator_rows(project: &project::Project, record: Option<&project::Coordinator>, agents: &[crate::core::Agent], dir: &str) -> Vec<String> {
    let identity = crate::lanes::classify_all(project, &record.cloned().unwrap_or_default(), agents);
    agents
        .iter()
        .zip(&identity)
        .filter(|(agent, resolution)| resolution.counts_as_coordinator() || (agent.works_in(dir) && crate::lanes::ours(agent, &project.slug)))
        .map(|(agent, _)| {
            if agent.name.starts_with("hpc-") {
                format!("{} ({})", agent.pane_id, agent.agent)
            } else {
                format!("{} ({}, started by hand)", agent.pane_id, agent.agent)
            }
        })
        .collect()
}

/// The app's names against the ones a coordinator's Space and tab must show.
/// `Core::name_space_and_tab` (run by `open` and by the ticker every tick) is
/// what makes them match, so a line here means the rename keeps failing, or the
/// terminal is not in the window at all — a warning with the command that
/// redoes it, never a FAIL.
fn coordinator_name_notes(
    herdr: &Core,
    project: &project::Project,
    view: &serde_json::Value,
    record: Option<&project::Coordinator>,
) -> Vec<String> {
    // What a coordinator's Space and tab must be called: the project's display
    // name and one word (t-0051). Only where those go is checked here.
    let label = project.display_name();
    let tab_label = crate::labels::coordinator_tab_label();
    let mut panes: Vec<String> = record.map(|record| record.pane_id.clone()).into_iter().filter(|pane| !pane.is_empty()).collect();
    if let Some(registry) = crate::lanes::Registry::load(project) {
        panes.extend(registry.lanes.iter().map(|lane| lane.pane_id.clone()).filter(|pane| !pane.is_empty()));
    }
    panes.sort();
    panes.dedup();
    let mut notes = Vec::new();
    for pane in panes {
        let (Ok((space, tab)), Ok(address)) = (herdr.locate(&pane), herdr.pane_get(&pane).map(|p| p.terminal_id)) else {
            continue;
        };
        if space.is_empty() || tab.is_empty() {
            notes.push(format!(
                "coordinator {pane} carries no `hp_ws`/`hp_tab` and the app knows no Space for it; `open {}` names it",
                project.slug
            ));
            continue;
        }
        match crate::app::shown_in(view, &space, &address) {
            Some(shown) if shown.space == label && shown.tab_name == tab_label => {}
            Some(shown) => notes.push(format!(
                "coordinator {pane} sits in the app's Space `{}` tab `{}`, not `{label}` / `{tab_label}`; `open {}` renames both (the ticker does it every tick too)",
                shown.space, shown.tab_name, project.slug
            )),
            None => notes.push(format!(
                "the app does not show coordinator {pane} ({address}) in Space {space}; `open {}` puts its Space and tab names on it",
                project.slug
            )),
        }
    }
    notes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::fake::{FakeRunner, fail, ok};

    /// A coordinator the window shows under its own default names: the Space
    /// keeps whatever name the person gave it and the tab is `1`, because hp
    /// never wrote `hp_ws`/`hp_tab` for it (t-0051). Doctor says so, with the
    /// command that renames both.
    #[test]
    fn doctor_warns_about_a_coordinator_whose_app_tab_is_not_named() {
        let home = tempfile::tempdir().unwrap();
        let project = project::create(home.path(), "REXA", vec![]).unwrap();
        let fake = crate::fake_hc::FakeCore::new();
        let app = crate::fake_hc::FakeApp::new(&fake);
        let herdr = Core::new("hc", "", &app);
        let space = app.shown_space("rexa");
        let term = fake.terminal("t52", "/r/rexa", &[(crate::core::L_APP_SPACE, space.as_str())]);
        let tab = format!("apptab-{}", term.address);
        fake.terminals.borrow_mut().iter_mut().find(|t| t.address == term.address).unwrap().labels.insert(crate::core::L_APP_TAB.into(), tab.clone());
        app.tab_names.borrow_mut().insert(tab.clone(), "1".into());
        let record = project.update_coordinator(|c| {
            c.pane_id = term.address.clone();
            c.workspace_id = space.clone();
            c.tab_id = tab.clone();
        }).unwrap();
        let view = crate::app::ls(&app).unwrap();

        let notes = coordinator_name_notes(&herdr, &project, &view, Some(&record));
        assert!(notes.iter().any(|note| note.contains("tab `1`") && note.contains("coordinator") && note.contains("open rexa")), "{notes:?}");

        // Once the names are put right the line is gone: the check is about a
        // name that is wrong, not about the app being used.
        let (found_space, found_tab) = crate::coordinator::keep_names(&herdr, &project, &term.address);
        assert_eq!((found_space.as_str(), found_tab.as_str()), (space.as_str(), tab.as_str()));
        let view = crate::app::ls(&app).unwrap();
        assert!(coordinator_name_notes(&herdr, &project, &view, Some(&record)).is_empty());
    }

    /// A plist written by `install_agent`, under a temporary HOME, and the root
    /// it points at. `print` answers whether launchd has the job loaded.
    fn installed(home: &Path, root: &Path, runner: &FakeRunner) {
        let binary = paths::binary().unwrap();
        crate::ticker::install_agent(home, &binary, root, runner).unwrap();
    }

    fn plist_path(home: &Path, root: &Path) -> std::path::PathBuf {
        crate::ticker::agent_path(home, root)
    }

    /// 2026-11-18: law's coordinator pane (t5h) has a shell sitting in rexa's
    /// folder while the pane says `hp_project=law`. Judging by the folder put it
    /// in rexa's row and left law's row saying "no coordinator running".
    #[test]
    fn a_coordinator_is_named_by_its_own_claim_not_by_its_shells_folder() {
        let root = tempfile::tempdir().unwrap();
        let rexa = project::create(root.path(), "rexa", vec![]).unwrap();
        let law = project::create(root.path(), "law", vec![]).unwrap();
        let rexa_dir = rexa.canonical_dir().to_string_lossy().into_owned();
        let law_dir = law.canonical_dir().to_string_lossy().into_owned();
        let mut agent = crate::core::Agent {
            pane_id: "t5h".into(),
            cwd: rexa_dir.clone(),
            name: "hpc-law".into(),
            agent: "pi".into(),
            ..Default::default()
        };
        agent.tokens.insert(crate::labels::PROJECT_TOKEN.into(), "law".into());
        agent.tokens.insert(crate::lanes::TOKEN.into(), "c-1".into());
        let law_record = law
            .update_coordinator(|c| {
                c.pane_id = "t5h".into();
                c.cwd = law_dir.clone();
            })
            .unwrap();

        let one = std::slice::from_ref(&agent);
        assert!(
            coordinator_rows(&rexa, None, one, &rexa_dir).is_empty(),
            "another project's agent is not rexa's, whatever its shell's directory says"
        );
        assert_eq!(coordinator_rows(&law, Some(&law_record), one, &law_dir), vec!["t5h (pi)".to_string()]);

        // A hand-started agent in the folder, claiming no project, is still
        // worth showing: nothing else would name it.
        let hand = crate::core::Agent { pane_id: "t9".into(), cwd: rexa_dir.clone(), agent: "claude".into(), ..Default::default() };
        assert_eq!(coordinator_rows(&rexa, None, std::slice::from_ref(&hand), &rexa_dir), vec!["t9 (claude, started by hand)".to_string()]);
    }

    /// The blank lines `hp task delete` leaves behind sit in no task, so until
    /// `hp task tidy` existed only a person could clean them up (`doctor` is the
    /// reminder, never the repair).
    #[test]
    fn doctor_points_at_blank_lines_the_task_list_kept() {
        let home = tempfile::tempdir().unwrap();
        let project = project::create(home.path(), "demo", vec![]).unwrap();
        std::fs::write(project.dir().join("TASKS.md"), "# Tasks\n\n## Backlog\n\n\n\n- [x] A\n").unwrap();
        let (mark, detail) = tasks_line(&project);
        assert_eq!(mark, None, "a hint, never a failure: {detail}");
        assert!(detail.contains("3 blank line(s) under \"## Backlog\""), "{detail}");
        assert!(detail.contains("`hp task tidy demo` cleans them"), "{detail}");

        // More places than the line shows are counted, not listed.
        std::fs::write(
            project.dir().join("TASKS.md"),
            "# Tasks\n\n\n## Backlog\n- [ ] A\n\n\n## Later\n- [ ] B\n\n\n## Empty\n\n",
        )
        .unwrap();
        let (mark, detail) = tasks_line(&project);
        assert_eq!(mark, None, "{detail}");
        assert!(detail.contains("(and 2 more)"), "{detail}");

        // Tidied, the line is an ordinary ok line.
        crate::tasks::tidy(&project, false, false).unwrap();
        assert_eq!(tasks_line(&project), (Some(true), "TASKS.md is tidy".to_string()));
    }

    /// The skeleton `hp new` writes (`# Tasks\n\n## Backlog\n`) is tidy, and
    /// so is what `hp task tidy` leaves behind: `doctor` never warns about an
    /// empty heading, because `hp task add` keeps writing that heading back.
    #[test]
    fn doctor_calls_the_house_skeleton_tidy() {
        let home = tempfile::tempdir().unwrap();
        let project = project::create(home.path(), "demo", vec![]).unwrap();
        assert_eq!(tasks_line(&project), (Some(true), "TASKS.md is tidy".to_string()));
        // An empty `## Backlog` under a blank line or two is a blank-line
        // finding only; the heading itself is never the complaint.
        std::fs::write(project.dir().join("TASKS.md"), "# Tasks\n\n## Backlog\n\n\n- [ ] A\n").unwrap();
        let (mark, detail) = tasks_line(&project);
        assert_eq!(mark, None, "{detail}");
        assert!(detail.contains("2 blank line(s) under \"## Backlog\""), "{detail}");
        assert!(!detail.contains("empty section"), "{detail}");
    }

    #[test]
    fn doctor_warns_about_a_missing_launch_agent() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        runner.on("launchctl print", fail(113, "Could not find service"));
        let (mark, detail) = ticker_agent_line(&env, root.path(), &runner, false, true).unwrap();
        assert_eq!(mark, None, "{detail}");
        assert!(detail.contains("no LaunchAgent"), "{detail}");
        assert!(detail.contains("hp ticker start"), "{detail}");
    }

    /// Between the two labels (t-0083): this root's own plist is not written
    /// yet and the job keeping the ticker alive is the shared `dev.hp.ticker`
    /// one. That is the state after an upgrade and before the ticker's next
    /// start, so it is not a warning — and the line names the command that
    /// moves it.
    #[test]
    fn doctor_accepts_the_old_shared_agent_until_the_next_start() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        runner.on("launchctl print", ok(""));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));
        installed(home.path(), root.path(), &runner);
        // The same plist, moved back to the shared label the old release wrote.
        let derived = crate::ticker::agent_path(home.path(), root.path());
        let legacy = home.path().join("Library/LaunchAgents/dev.hp.ticker.plist");
        let text = std::fs::read_to_string(&derived).unwrap();
        std::fs::write(&legacy, text.replace(&crate::ticker::agent_label(root.path()), crate::ticker::LEGACY_AGENT_LABEL)).unwrap();
        std::fs::remove_file(&derived).unwrap();

        let (mark, detail) = ticker_agent_line(&env, root.path(), &runner, false, true).unwrap();
        assert_eq!(mark, Some(true), "{detail}");
        assert!(detail.contains("old shared LaunchAgent"), "{detail}");
        assert!(detail.contains("dev.hp.ticker.plist"), "{detail}");
        assert!(detail.contains("hp ticker stop"), "{detail}");

        // launchd does not have it either: now nothing keeps the ticker alive.
        let orphan = FakeRunner::new();
        orphan.on("launchctl print", fail(113, "Could not find service"));
        let (mark, detail) = ticker_agent_line(&env, root.path(), &orphan, false, true).unwrap();
        assert_eq!(mark, None, "{detail}");
        assert!(detail.contains("has not loaded it"), "{detail}");
    }

    #[test]
    fn doctor_warns_about_a_plist_launchd_did_not_load() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        runner.on("launchctl print", fail(113, "Could not find service"));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));
        installed(home.path(), root.path(), &runner);
        let (mark, detail) = ticker_agent_line(&env, root.path(), &runner, false, true).unwrap();
        assert_eq!(mark, None, "{detail}");
        assert!(detail.contains("has not loaded it"), "{detail}");
        // It names this root's own label, which is what `launchctl bootstrap`
        // needs and what tells two roots' jobs apart (t-0083).
        assert!(detail.contains(&crate::ticker::agent_label(root.path())), "{detail}");
    }

    #[test]
    fn doctor_warns_about_the_plist_the_accident_left_behind() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        runner.on("launchctl print", ok(""));
        let path = plist_path(home.path(), root.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "<plist><dict><key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict></dict></plist>").unwrap();
        let (mark, detail) = ticker_agent_line(&env, root.path(), &runner, false, true).unwrap();
        assert_eq!(mark, None, "{detail}");
        assert!(detail.contains("out of date"), "{detail}");
        assert!(detail.contains("hp doctor --fix"), "{detail}");

        // `--fix` rewrites it and loads it again.
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));
        let (mark, detail) = ticker_agent_line(&env, root.path(), &runner, true, true).unwrap();
        assert_eq!(mark, Some(true), "{detail}");
        assert!(detail.contains("fixed:"), "{detail}");
        assert!(std::fs::read_to_string(&path).unwrap().contains("<key>KeepAlive</key><true/>"));
    }

    /// A stand-in for the loop a fix starts: it takes the root's lock a moment
    /// later, which is the only thing that shows a ticker really runs.
    fn pretend_ticker(root: &Path) -> std::thread::JoinHandle<()> {
        let path = root.join(".ticker.lock");
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let file = std::fs::File::options().create(true).truncate(false).read(true).write(true).open(&path).unwrap();
            file.lock().unwrap();
            std::thread::sleep(std::time::Duration::from_secs(3));
            drop(file);
        })
    }

    #[test]
    fn doctor_fix_starts_the_loop_it_loads() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        runner.on("launchctl print", ok(""));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));
        installed(home.path(), root.path(), &runner);
        // Written and loaded, and still no ticker: the second disappearance.
        let (mark, detail) = ticker_agent_line(&env, root.path(), &runner, false, false).unwrap();
        assert_eq!(mark, Some(true), "{detail}");

        let ticker = pretend_ticker(root.path());
        let (mark, detail) = ticker_agent_line(&env, root.path(), &runner, true, false).unwrap();
        assert_eq!(mark, Some(true), "{detail}");
        assert!(detail.contains("a ticker holds the lock"), "{detail}");
        ticker.join().unwrap();
    }

    #[test]
    fn doctor_is_happy_with_a_written_and_loaded_launch_agent() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        runner.on("launchctl print", ok(""));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));
        installed(home.path(), root.path(), &runner);
        let (mark, detail) = ticker_agent_line(&env, root.path(), &runner, false, true).unwrap();
        assert_eq!(mark, Some(true), "{detail}");
        assert!(detail.contains("KeepAlive is unconditional"), "{detail}");
    }
}
