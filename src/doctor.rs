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
            dirs.extend(settings.repos.into_iter().filter(|r| r.machine.is_none()).map(|r| r.path));
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
/// Herdr's config runs this binary in the tab bar and has today's sub-line
/// row and no row or card of an earlier layout (the 0.2.17/0.2.18 headings,
/// the 0.2.19 rails and `$hp_home`); otherwise `--fix` runs `configure`,
/// which swaps them.
fn sidebar_current(text: &str, tab_command: &str) -> bool {
    let legacy = crate::grouping::legacy().iter().any(|t| text.contains(&format!("\"${t}\"")));
    text.contains(tab_command) && text.contains("\"$hp_sub\"") && !legacy
}

pub fn run(ctx: &Ctx, session: &SessionFlags, fix: bool) -> Result<bool> {
    let skill = crate::setup::skill_source();
    let (text, healthy) = report(ctx.env, &ctx.root, &ctx.config_dir, session, ctx.runner, fix, skill.as_deref());
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
    skill: Option<&Path>,
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
        check(&mut out, None, "update", format!("a newer version is available ({latest}): run `herdr-projects update`"));
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
            let name = found.name.as_deref().unwrap_or("-");
            check(
                &mut out,
                if reachable { Some(true) } else { None },
                "session",
                format!(
                    "{} (name: {name}){}",
                    found.socket.display(),
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
            Some(record) if Path::new(&record.socket).exists() => (Some(record), Vec::new()),
            Some(record) => (None, vec![format!("recorded socket {} no longer exists", record.socket)]),
            None => (None, Vec::new()),
        };
        let socket = match (&record, paths::resolve_session(session, env, runner)) {
            (Some(record), _) => record.socket.clone(),
            (None, Ok(found)) => found.socket.to_string_lossy().into_owned(),
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

    // Hooks: ours in place and pointing at this binary; the standalone
    // agent-progress plugin's hooks gone (never edited by this plugin).
    //
    // `--fix` only ever moves these files toward the installation that put them
    // there: a run with another binary or root (a throwaway build under
    // `/tmp`, a scratch root) says so and leaves them alone, instead of
    // repointing every agent's progress reporting at a binary that goes away.
    let journal = crate::setup::load_journal(config_dir);
    let here = crate::setup::Installation::here(root).ok();
    let repair = match &here {
        Some(here) => crate::setup::may_repair(&journal, here),
        None => Err("left alone: this binary's own path is unknown, so it cannot be the installation".to_string()),
    };
    let refusal = repair.as_ref().err().map(String::as_str).unwrap_or_default();
    for agent in crate::setup::AGENTS {
        let file = crate::setup::hook_file(env, agent, None, None);
        let Ok(Some(text)) = crate::setup::read(&file) else {
            continue;
        };
        let label = format!("hooks {agent}");
        if crate::setup::has_agent_progress_hooks(&text) {
            let launcher = text
                .split('"')
                .find(|s| s.contains("herdr-progress") && s.contains(" hook --agent "))
                .and_then(|s| s.split(" hook --agent ").next())
                .unwrap_or("herdr-progress")
                .to_string();
            check(&mut out, None, &label, format!("{} still runs the standalone agent-progress hooks; run `{launcher} unconfigure`, then `herdr plugin disable agent-progress`", file.display()));
        }
        let key = file.to_string_lossy().into_owned();
        let binary = crate::paths::binary().unwrap_or_default();
        let expected = crate::setup::hook_command(&binary, root, agent);
        match journal.get(&key) {
            None => check(&mut out, None, &label, "not configured; `configure` installs the progress hooks".into()),
            Some(_) if text.contains(&expected) => check(&mut out, Some(true), &label, format!("{} runs this binary", file.display())),
            Some(_) if repair.is_err() => check(&mut out, None, &label, format!("{} runs another binary or root; {refusal}", file.display())),
            Some(_) if fix => {
                let options = crate::setup::ConfigureOptions { clients: vec![agent.to_string()], claude_home: None, codex_home: None, dry_run: false, hooks: true, sidebar: false, key: None, herdr_config: None, skill: crate::setup::skill_source() };
                let ctx = Ctx { env, root: root.to_path_buf(), config_dir: config_dir.to_path_buf(), runner, detached_ticker: false };
                match crate::setup::configure(&ctx, &options) {
                    Ok(_) => check(&mut out, Some(true), &label, format!("fixed: {} now runs this binary", file.display())),
                    Err(error) => check(&mut out, Some(false), &label, format!("could not fix: {error:#}")),
                }
            }
            Some(_) => check(&mut out, None, &label, format!("{} runs another binary or root; `doctor --fix` rewrites it", file.display())),
        }
    }

    // The bundled skill, linked where each installed harness looks for skills.
    // `--fix` links it only for a harness the user already ran `configure`
    // for (its hooks or the link are journaled), so `update` alone brings a
    // newly bundled skill to existing users without a new opt-in. The link is
    // global too, so the same rule applies: only the installation relinks it.
    if let Some(source) = skill.map(Path::to_path_buf).filter(|s| s.join("SKILL.md").is_file()) {
        for agent in ["claude", "codex"] {
            if !crate::setup::hook_file(env, agent, None, None).parent().is_some_and(Path::is_dir) {
                continue;
            }
            let link = crate::setup::skill_link(env, agent, None);
            let label = format!("skill {agent}");
            let journaled = journal.contains_key(&*link.to_string_lossy());
            let opted_in = journaled || journal.get(&*crate::setup::hook_file(env, agent, None, None).to_string_lossy()).is_some_and(|o| o.kind == "hooks");
            let state = crate::setup::skill_state(&link, &source);
            let repairable = matches!(state, crate::setup::SkillState::Missing) || matches!(state, crate::setup::SkillState::Elsewhere(_) if journaled);
            if fix && opted_in && repairable && repair.is_err() {
                check(&mut out, None, &label, format!("{} needs relinking; {refusal}", link.display()));
                continue;
            }
            if fix && opted_in && repairable {
                let options = crate::setup::ConfigureOptions { clients: vec![agent.to_string()], claude_home: None, codex_home: None, dry_run: false, hooks: false, sidebar: false, key: None, herdr_config: None, skill: Some(source.clone()) };
                let ctx = Ctx { env, root: root.to_path_buf(), config_dir: config_dir.to_path_buf(), runner, detached_ticker: false };
                match crate::setup::configure(&ctx, &options) {
                    Ok(_) => check(&mut out, Some(true), &label, format!("fixed: {} now links the bundled `{}` skill", link.display(), crate::setup::SKILL)),
                    Err(error) => check(&mut out, Some(false), &label, format!("could not fix: {error:#}")),
                }
                continue;
            }
            let repair_hint = if opted_in { "`doctor --fix`" } else { "`configure`" };
            match state {
                crate::setup::SkillState::Ours => check(&mut out, Some(true), &label, format!("{} links the bundled `{}` skill", link.display(), crate::setup::SKILL)),
                crate::setup::SkillState::Missing => check(&mut out, None, &label, format!("{} is missing; {repair_hint} links the bundled skill", link.display())),
                crate::setup::SkillState::Elsewhere(old) if journaled => check(&mut out, None, &label, format!("{} links {}, another checkout; {repair_hint} relinks it", link.display(), old.display())),
                _ => check(&mut out, None, &label, format!("{} is not this plugin's link, so the bundled skill is not installed; move it away and run {repair_hint}", link.display())),
            }
        }
    }

    // Herdr's config.toml: rows, popup key and a tab-bar entry that runs this binary.
    {
        let file = crate::setup::herdr_config_path(env);
        let binary = crate::paths::binary().unwrap_or_default();
        let expected = crate::setup::tab_command(&binary, root);
        let text = crate::setup::read(&file).ok().flatten().unwrap_or_default();
        let key = file.to_string_lossy().into_owned();
        match journal.get(&key) {
            None => check(&mut out, None, "sidebar", "not configured; `configure` adds the sidebar rows, the popup key and the tab-bar count".into()),
            Some(_) if sidebar_current(&text, &expected) => check(&mut out, Some(true), "sidebar", format!("{} has the rows, the popup key and the tab-bar entry", file.display())),
            Some(_) if repair.is_err() => check(&mut out, None, "sidebar", format!("{} lacks the project grouping rows (or still has the 0.2.18 headings or 0.2.19 rails), or its tab-bar entry runs another binary or root; {refusal}", file.display())),
            Some(_) if fix => {
                let options = crate::setup::ConfigureOptions { clients: vec![], claude_home: None, codex_home: None, dry_run: false, hooks: true, sidebar: true, key: None, herdr_config: None, skill: crate::setup::skill_source() };
                let ctx = Ctx { env, root: root.to_path_buf(), config_dir: config_dir.to_path_buf(), runner, detached_ticker: false };
                let options = crate::setup::ConfigureOptions { clients: vec!["none".into()], ..options };
                match crate::setup::configure(&ctx, &options) {
                    Ok(_) => check(&mut out, Some(true), "sidebar", format!("fixed: {} has the project grouping rows and runs this binary in the tab bar", file.display())),
                    Err(error) => check(&mut out, Some(false), "sidebar", format!("could not fix: {error:#}")),
                }
            }
            Some(_) => check(&mut out, None, "sidebar", format!("{} lacks the project grouping rows (or still has the 0.2.18 headings or 0.2.19 rails), or its tab-bar entry runs another binary or root; `doctor --fix` rewrites it", file.display())),
        }
    }

    // Machines that projects use need an SSH target for report and library copies.
    let mut machines = std::collections::BTreeSet::new();
    for slug in project::list_slugs(root) {
        let Ok(project) = project::Project::load(root, &slug) else {
            continue;
        };
        if let Ok((settings, _)) = project.read_project_md() {
            machines.extend(settings.repos.into_iter().filter_map(|r| r.machine));
        }
        machines.extend(crate::thread::list(&project).into_iter().filter(|t| t.is_remote() && t.status != crate::thread::Status::Resolved).map(|t| t.machine));
    }
    for machine in machines {
        match crate::remote::ssh_target(runner, &bin, config_dir, &machine) {
            Ok(target) => check(&mut out, Some(true), &format!("machine {machine}"), format!("ssh target {target}")),
            Err(error) => check(&mut out, Some(false), &format!("machine {machine}"), format!("{error:#}")),
        }
    }

    (out, healthy)
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_sidebar_without_the_sub_line_or_with_an_earlier_layout_is_not_current() {
        let command = "'/b/herdr-projects' --root /r needs-you --line";
        let old = format!("[ui]\ntab_bar_right = [{{ type = \"command\", command = \"{command}\" }}]\n");
        assert!(!super::sidebar_current(&old, command));
        let added = crate::sidebar::config_edit(&old, &crate::sidebar::Spec { key: "prefix+a".into(), tab_command: command.into() }, false).unwrap();
        assert!(super::sidebar_current(&added, command));
        let with_heading = added.replacen("rows = [", "rows = [[{ token = \"$hp_top\" }], ", 1);
        assert!(!super::sidebar_current(&with_heading, command));
        let with_rail = added.replacen("rows = [", "rows = [[{ token = \"$hp_top_w\" }], ", 1);
        assert!(!super::sidebar_current(&with_rail, command));
    }
    use super::*;
    use crate::runner::fake::{FakeRunner, fail, ok};

    fn runner_with_herdr(version: &str) -> FakeRunner {
        let runner = FakeRunner::new();
        runner.on("herdr --version", ok(version));
        runner.on("session list --json", ok(r#"{"sessions":[]}"#));
        runner.on("git --version", ok("git version 2.50.0\n"));
        runner.on("ssh -V", ok(""));
        runner.on("rsync --version", ok("rsync 3\n"));
        runner.on("gh --version", ok("gh version 2\n"));
        runner.on("gh auth status", fail(1, "not logged in"));
        runner
    }

    #[test]
    fn old_herdr_fails_and_names_the_minimum() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.0\n");
        let (text, healthy) = report(
            &env,
            &home.path().join("root"),
            &home.path().join("cfg"),
            &SessionFlags::default(),
            &runner,
            false,
            None,
        );
        assert!(!healthy);
        assert!(text.contains("[FAIL] herdr: 0.9.0"), "{text}");
        assert!(text.contains("0.9.1 or later"));
    }

    #[test]
    fn new_herdr_passes_and_warnings_do_not_fail() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        let (text, healthy) = report(
            &env,
            &root,
            &home.path().join("cfg"),
            &SessionFlags::default(),
            &runner,
            false,
            None,
        );
        assert!(healthy, "{text}");
        assert!(text.contains("[warn] gh auth"));
        assert!(text.contains("[warn] root"));
        assert!(text.contains(&format!("root:       {}", root.display())));
        assert!(!root.exists(), "doctor must not create the root");
    }

    #[test]
    fn missing_priming_files_are_reported_and_fixed() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        // A project made before AGENTS.md existed: no priming files, no uploads/.
        let project = project::create(&root, "demo", "", vec![]).unwrap();
        std::fs::remove_dir(project.dir().join("uploads")).unwrap();
        let flags = SessionFlags::default();
        let (text, _) = report(&env, &root, &home.path().join("cfg"), &flags, &runner, false, None);
        assert!(text.contains("[warn] files demo: AGENTS.md is missing; CLAUDE.md is not a link to AGENTS.md; uploads/ is missing; `doctor --fix` repairs this"), "{text}");
        assert!(!project.dir().join("AGENTS.md").exists());

        let (text, _) = report(&env, &root, &home.path().join("cfg"), &flags, &runner, true, None);
        assert!(text.contains("[ok  ] files demo: fixed: AGENTS.md is missing"), "{text}");
        assert!(project.dir().join("AGENTS.md").is_file());
        assert!(project.dir().join("uploads").is_dir());
        let (text, _) = report(&env, &root, &home.path().join("cfg"), &flags, &runner, false, None);
        assert!(text.contains("[ok  ] files demo: AGENTS.md, CLAUDE.md link and uploads/ are in place"), "{text}");
    }

    #[test]
    fn doctor_fix_records_the_default_routine_it_restores() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        let project = project::create(&root, "demo", "", vec![]).unwrap();
        assert_eq!(crate::routine::journal_lines(&project), Some(2));
        std::fs::remove_file(project.dir().join(project::COORDINATOR_SWEEP)).unwrap();

        let (text, _) = report(&env, &root, &home.path().join("cfg"), &SessionFlags::default(), &runner, true, None);
        assert!(text.contains("[ok  ] files demo: fixed:"), "{text}");
        assert!(project.dir().join(project::COORDINATOR_SWEEP).is_file());
        let journal = std::fs::read_to_string(crate::routine::journal_path(&project)).unwrap();
        assert_eq!(journal.lines().count(), 3, "{journal}");
        assert!(journal.lines().last().unwrap().contains("  create  coordinator-sweep  enabled  source=doctor-fix  binary="), "{journal}");
    }

    /// exe.dev VMs: `origin` is on `github.localhost` and plain `gh` is logged
    /// in nowhere. Doctor asks that host, not `gh auth status`.
    #[test]
    fn a_repository_off_github_com_is_checked_on_its_own_host() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let root = home.path().join("root");
        let repo = home.path().join("app");
        std::fs::create_dir_all(&repo).unwrap();
        project::create(&root, "demo", "", vec![project::Repo { path: repo.to_string_lossy().into_owned(), machine: None }]).unwrap();
        let on_host = |cmd: &Cmd| cmd.program == "gh" && cmd.env.contains(&("GH_HOST".into(), "github.localhost".into()));
        for reachable in [true, false] {
            let runner = FakeRunner::new();
            runner.on_fn(|cmd| cmd.display().ends_with("/app remote get-url origin"), |_| Ok(ok("http://github.localhost/eliasstravik/app.git\n")));
            if reachable {
                runner.on_fn(on_host, |_| Ok(ok("eliasstravik\n")));
            }
            runner.on("gh api user", fail(1, "HTTP 401: Requires authentication"));
            for (needle, output) in [("herdr --version", "herdr 0.9.1\n"), ("git --version", "git 2\n"), ("gh --version", "gh 2\n")] {
                runner.on(needle, ok(output));
            }
            let (text, _) = report(&env, &root, &home.path().join("cfg"), &SessionFlags::default(), &runner, false, None);
            if reachable {
                assert!(text.contains("[ok  ] github github.localhost: reachable as eliasstravik (GH_HOST=github.localhost, used for eliasstravik/app)"), "{text}");
            } else {
                assert!(text.contains("[warn] github github.localhost: gh cannot reach github.localhost (HTTP 401: Requires authentication); pull requests of eliasstravik/app are not followed. Try `GH_HOST=github.localhost gh auth status`"), "{text}");
            }
            assert!(!text.contains("] gh auth:"), "github.com is not in use: {text}");
            assert_eq!(runner.count("gh auth status"), 0);
        }
    }

    #[test]
    fn routines_skipped_for_want_of_a_coordinator_are_named() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        let project = project::create(&root, "demo", "", vec![]).unwrap();
        std::fs::write(project.dir().join("routines/standup.md"), "+++\nschedule = \"every 5m\"\n+++\nGo.\n").unwrap();
        let (text, _) = report(&env, &root, &home.path().join("cfg"), &SessionFlags::default(), &runner, false, None);
        assert!(!text.contains("routines demo"), "{text}");
        let mut state = crate::steps::State::default();
        state.routines.insert("standup".into(), crate::routine::State { last_run: "2026-09-24T09:00:00Z".into(), no_coordinator: 2, ..Default::default() });
        crate::steps::save_state(&project, &state).unwrap();
        let (text, _) = report(&env, &root, &home.path().join("cfg"), &SessionFlags::default(), &runner, false, None);
        assert!(text.contains("[warn] routines demo: standup: last "), "{text}");
        assert!(text.contains("skipped: no coordinator (2 run(s)); routines run only while a coordinator runs"), "{text}");
    }

    #[test]
    fn fix_links_the_skill_only_for_a_configured_harness_and_never_over_a_foreign_one() {
        let home = tempfile::tempdir().unwrap();
        let claude = home.path().join("claude-config");
        let env = Env::for_test(home.path(), &[("CLAUDE_CONFIG_DIR", claude.to_str().unwrap())]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        let cfg = home.path().join("cfg");
        let flags = SessionFlags::default();
        std::fs::create_dir_all(&claude).unwrap();
        let source = home.path().join("plugin/skill/autoproject");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("SKILL.md"), "---\nname: autoproject\n---\n").unwrap();
        let link = claude.join("skills").join(crate::setup::SKILL);

        // Never configured: `--fix` only points to `configure`.
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, true, Some(&source));
        assert!(text.contains("[warn] skill claude:") && text.contains("`configure` links the bundled skill"), "{text}");
        assert_eq!(crate::setup::skill_state(&link, &source), crate::setup::SkillState::Missing);

        // Configured before the skill shipped: hooks journaled, no link yet.
        let hooks = crate::setup::Owned { before: None, after: "{}".into(), kind: "hooks".into(), command: Some("x".into()), binary: None, root: None };
        crate::setup::save_journal(&cfg, &[(claude.join("settings.json").to_string_lossy().into_owned(), hooks)].into()).unwrap();
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, false, Some(&source));
        assert!(text.contains("`doctor --fix` links the bundled skill"), "{text}");
        let (text, healthy) = report(&env, &root, &cfg, &flags, &runner, true, Some(&source));
        assert!(healthy && text.contains("[ok  ] skill claude: fixed:"), "{text}");
        assert_eq!(crate::setup::skill_state(&link, &source), crate::setup::SkillState::Ours);
        assert!(!claude.join("settings.json").exists(), "the hooks were touched");

        // A moved checkout: our journaled link is relinked.
        let moved = home.path().join("moved/skill/autoproject");
        std::fs::create_dir_all(&moved).unwrap();
        std::fs::write(moved.join("SKILL.md"), "x").unwrap();
        report(&env, &root, &cfg, &flags, &runner, true, Some(&moved));
        assert_eq!(crate::setup::skill_state(&link, &moved), crate::setup::SkillState::Ours);

        // A directory of the same name is never touched.
        std::fs::remove_file(&link).unwrap();
        std::fs::create_dir(&link).unwrap();
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, true, Some(&moved));
        assert!(text.contains("is not this plugin's link"), "{text}");
        assert_eq!(crate::setup::skill_state(&link, &moved), crate::setup::SkillState::Foreign);
    }

    /// A machine whose hooks and Herdr config were installed by `binary` with
    /// `home/projects` as the root, as `configure` leaves it: the two files and
    /// the journal entries that record which installation wrote them.
    fn configured_machine(home: &Path, binary: &Path) -> (Env, std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let claude = home.join("claude-config");
        let herdr_config = home.join("herdr/config.toml");
        let root = home.join("projects");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::create_dir_all(herdr_config.parent().unwrap()).unwrap();
        let env = Env::for_test(
            home,
            &[("CLAUDE_CONFIG_DIR", claude.to_str().unwrap()), ("HERDR_CONFIG_PATH", herdr_config.to_str().unwrap())],
        );
        if binary.starts_with(home) && !binary.exists() {
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(binary, "").unwrap();
        }
        let command = crate::setup::hook_command(binary, &root, "claude");
        let tab = crate::setup::tab_command(binary, &root);
        let hooks = crate::setup::hooks("{}", &command, false).unwrap();
        let config = crate::sidebar::config_edit("", &crate::sidebar::Spec { key: "prefix+a".into(), tab_command: tab.clone() }, false).unwrap();
        std::fs::write(claude.join("settings.json"), &hooks).unwrap();
        std::fs::write(&herdr_config, &config).unwrap();
        let owned = |after: &str, kind: &str, command: String| crate::setup::Owned::new(Some(String::new()), after.to_string(), kind, Some(command), binary, &root);
        crate::setup::save_journal(
            &home.join("cfg"),
            &[
                (claude.join("settings.json").to_string_lossy().into_owned(), owned(&hooks, "hooks", command)),
                (herdr_config.to_string_lossy().into_owned(), owned(&config, "config", tab)),
            ]
            .into(),
        )
        .unwrap();
        (env, root, claude.join("settings.json"), herdr_config)
    }

    /// The incident this guards: a verification run with a throwaway build and
    /// a scratch root repointed every harness hook and Herdr's tab bar at a
    /// binary under `/tmp`, and nothing said so. `--fix` from that run must
    /// leave both files byte for byte, and name the installation instead.
    #[test]
    fn a_fix_from_another_binary_or_root_rewrites_no_global_file() {
        let home = tempfile::tempdir().unwrap();
        let installed = home.path().join("plugin/target/release/herdr-projects");
        let (env, _root, hooks_file, herdr_config) = configured_machine(home.path(), &installed);
        let before = (std::fs::read_to_string(&hooks_file).unwrap(), std::fs::read_to_string(&herdr_config).unwrap());
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let scratch = home.path().join("scratch-root");
        let (text, _) = report(&env, &scratch, &home.path().join("cfg"), &SessionFlags::default(), &runner, true, None);
        assert_eq!(std::fs::read_to_string(&hooks_file).unwrap(), before.0, "a throwaway run rewrote the hooks: {text}");
        assert_eq!(std::fs::read_to_string(&herdr_config).unwrap(), before.1, "a throwaway run rewrote Herdr's config: {text}");
        assert!(text.contains("[warn] hooks claude: ") && text.contains("left alone: this is not the installation"), "{text}");
        let recorded = format!("{} --root {}", installed.display(), home.path().join("projects").display());
        assert!(text.contains(&recorded), "the recorded installation is named: {text}");
        assert!(text.contains("[warn] sidebar: ") && text.contains("left alone: this is not the installation"), "the sidebar obeys the same rule: {text}");

        // The real root, and a binary that is not the installation: still refused.
        let (text, _) = report(&env, &home.path().join("projects"), &home.path().join("cfg"), &SessionFlags::default(), &runner, true, None);
        assert_eq!(std::fs::read_to_string(&hooks_file).unwrap(), before.0, "another binary rewrote the hooks: {text}");
        assert!(text.contains("[warn] hooks claude: ") && text.contains("left alone: this is not the installation"), "{text}");
    }

    /// The other half: the installation that wrote the journal repairs an entry
    /// that points elsewhere, which is what `--fix` is for.
    #[test]
    fn a_fix_from_the_installation_still_repairs_what_points_elsewhere() {
        let home = tempfile::tempdir().unwrap();
        let binary = crate::paths::binary().unwrap();
        let (env, root, hooks_file, herdr_config) = configured_machine(home.path(), &binary);
        let cfg = home.path().join("cfg");
        let journal = crate::setup::load_journal(&cfg);
        let here = crate::setup::Installation { binary: binary.clone(), root: root.clone() };
        assert_eq!(crate::setup::may_repair(&journal, &here), Ok(()));
        // Another binary, or another root, is not the installation.
        let other = |binary: std::path::PathBuf, root: std::path::PathBuf| crate::setup::Installation { binary, root };
        assert!(crate::setup::may_repair(&journal, &other(home.path().join("tmp/release/herdr-projects"), root.clone())).is_err());
        assert!(crate::setup::may_repair(&journal, &other(binary.clone(), home.path().join("scratch-root"))).is_err());

        // A hook file and a tab bar pointing at a binary that moved away.
        std::fs::write(&hooks_file, crate::setup::hooks("{}", &crate::setup::hook_command(Path::new("/moved/herdr-projects"), &root, "claude"), false).unwrap()).unwrap();
        let tab = crate::setup::tab_command(Path::new("/moved/herdr-projects"), &root);
        std::fs::write(&herdr_config, crate::sidebar::config_edit("", &crate::sidebar::Spec { key: "prefix+a".into(), tab_command: tab }, false).unwrap()).unwrap();
        let runner = runner_with_herdr("herdr 0.9.1\n");
        // The sidebar repair reads Herdr's built-in keys and has it check the result.
        runner.on("--default-config", crate::runner::fake::ok("[keys]\n"));
        runner.on("config check", crate::runner::fake::ok(""));
        let (text, _) = report(&env, &root, &cfg, &SessionFlags::default(), &runner, true, None);
        assert!(text.contains("[ok  ] hooks claude: fixed:") && !text.contains("left alone: this is not the installation"), "{text}");
        assert!(text.contains("[ok  ] sidebar: fixed:"), "{text}");
        assert!(std::fs::read_to_string(&hooks_file).unwrap().contains(&crate::setup::hook_command(&binary, &root, "claude")), "{text}");
        assert!(std::fs::read_to_string(&herdr_config).unwrap().contains(&crate::setup::tab_command(&binary, &root)), "{text}");
    }

    /// A machine nobody configured stays unconfigured, and `--fix` invents
    /// nothing: `configure` is the way to install the hooks.
    #[test]
    fn a_machine_with_no_journal_entry_is_not_configured_and_a_fix_writes_nothing() {
        let home = tempfile::tempdir().unwrap();
        let claude = home.path().join("claude-config");
        std::fs::create_dir_all(&claude).unwrap();
        let settings = claude.join("settings.json");
        std::fs::write(&settings, "{\"theme\":\"dark\"}").unwrap();
        let env = Env::for_test(home.path(), &[("CLAUDE_CONFIG_DIR", claude.to_str().unwrap())]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let (text, _) = report(&env, &home.path().join("root"), &home.path().join("cfg"), &SessionFlags::default(), &runner, true, None);
        assert!(text.contains("[warn] hooks claude: not configured; `configure` installs the progress hooks"), "{text}");
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{\"theme\":\"dark\"}");
        assert!(crate::setup::load_journal(&home.path().join("cfg")).is_empty());
    }

    #[test]
    fn a_project_without_a_lane_registry_gets_no_lane_warning() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        project::create(&root, "demo", "", vec![]).unwrap();
        let (text, _) = report(&env, &root, &home.path().join("cfg"), &SessionFlags::default(), &runner, false, None);
        assert!(!text.contains("lanes demo"), "{text}");
    }

    #[test]
    fn an_ambiguous_lane_pane_is_a_warning_that_names_lane_adopt() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let root = home.path().join("root");
        let project = project::create(&root, "demo", "", vec![]).unwrap();
        let cwd = project.canonical_dir().to_string_lossy().into_owned();
        let lane = |id: &str, label: &str| crate::lanes::Lane { id: id.into(), name: id.into(), label: label.into(), ..Default::default() };
        crate::lanes::Registry { lanes: vec![lane("c-1", "fix"), lane("c-2", "books")], default: "c-1".into(), next: 3 }
            .save(&project)
            .unwrap();
        let runner = runner_with_herdr("herdr 0.9.1\n");
        runner.on("pane list", ok(r#"{"result":{"panes":[]}}"#));
        let agent = crate::scenarios::agent_json("w1", "w1:t1", "w1:p1", &cwd, "hpc-demo", "idle");
        runner.on("agent list", ok(&format!(r#"{{"result":{{"agents":[{agent}]}}}}"#)));
        let (text, healthy) = report(&env, &root, &home.path().join("cfg"), &SessionFlags::default(), &runner, false, None);
        assert!(healthy, "a warning must not fail the report: {text}");
        assert!(text.contains("[warn] lanes demo: pane w1:p1 is ambiguous"), "{text}");
        assert!(text.contains("hp lane adopt"), "{text}");
    }

    #[test]
    fn a_lane_whose_live_pane_is_unrecognized_is_a_warning_that_names_lane_adopt() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let root = home.path().join("root");
        let project = project::create(&root, "demo", "", vec![]).unwrap();
        crate::lanes::Registry {
            lanes: vec![crate::lanes::Lane {
                id: "c-1".into(),
                name: "fix".into(),
                label: "抢修".into(),
                pane_id: "w1:p1".into(),
                agent_session: "sess-old".into(),
                ..Default::default()
            }],
            default: "c-1".into(),
            next: 2,
        }
        .save(&project)
        .unwrap();
        let runner = runner_with_herdr("herdr 0.9.1\n");
        runner.on("pane list", ok(r#"{"result":{"panes":[]}}"#));
        let agent = crate::scenarios::agent_json_session("w1", "w1:p1", "/elsewhere", "sess-new");
        runner.on("agent list", ok(&format!(r#"{{"result":{{"agents":[{agent}]}}}}"#)));
        let (text, healthy) = report(&env, &root, &home.path().join("cfg"), &SessionFlags::default(), &runner, false, None);
        assert!(healthy, "a warning must not fail the report: {text}");
        assert!(text.contains("[warn] lanes demo: lane c-1 (抢修) is alive in pane w1:p1 but unrecognized"), "{text}");
        assert!(text.contains("hp lane adopt c-1"), "{text}");
    }

    #[test]
    fn colliding_agent_names_fail_the_report() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        let long = "x".repeat(30);
        for suffix in ["a", "b"] {
            let project = project::create(&root, &format!("{long}-{suffix}"), "", vec![]).unwrap();
            project::write_priming(&project, &crate::coordinator::current_prefix(&root).unwrap(), "doctor-fix").unwrap();
        }
        let (text, healthy) = report(&env, &root, &home.path().join("cfg"), &SessionFlags::default(), &runner, false, None);
        assert!(!healthy);
        assert!(text.contains("[FAIL] names"), "{text}");
    }
}
