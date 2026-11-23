//! End-to-end checks of the built binary with a scrubbed environment.

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_hp");

fn hp(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .env_clear()
        .env("HOME", home)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn context_prints_a_usable_prefix_in_a_scrubbed_environment() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("my root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Demo"]).status.success());

    let out = hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let prefix = text.lines().next().unwrap().strip_prefix("Commands: ").unwrap();
    // Fixed shape `<binary> --root <root>`, with the spaced root shell-quoted.
    assert_eq!(prefix, format!("{BIN} --root '{root_arg}'"));

    // The printed prefix works as typed, from a bare shell.
    let listed = Command::new("/bin/sh")
        .env_clear()
        .env("HOME", home.path())
        .args(["-c", &format!("{prefix} list")])
        .output()
        .unwrap();
    assert!(listed.status.success());
    assert_eq!(String::from_utf8_lossy(&listed.stdout), "demo\tno threads\n");
}

#[test]
fn peek_records_nothing_and_context_records_seen_items() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "demo"]).status.success());
    let item = "+++\nid = \"20260917T000000Z-routine-r-1\"\nkind = \"routine\"\nsubject = \"r\"\ncreated = \"x\"\nsummary = \"s\"\n+++\n";
    std::fs::write(root.join("demo/inbox/20260917T000000Z-routine-r-1.md"), item).unwrap();
    let seen = root.join("demo/.state/inbox-seen.json");

    assert!(hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]).status.success());
    assert!(!seen.exists());
    assert!(hp(home.path(), &["--root", root_arg, "context", "demo"]).status.success());
    assert!(std::fs::read_to_string(&seen).unwrap().contains("routine-r-1"));
}

#[test]
fn routine_toggle_leaves_a_journal_line() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "demo"]).status.success());
    let out = hp(home.path(), &["--root", root_arg, "routine", "toggle", "demo", "coordinator-sweep", "--off"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let journal = std::fs::read_to_string(root.join("demo/.state/routine-journal.log")).unwrap();
    let line = journal.lines().last().unwrap();
    assert!(line.contains("  toggle  coordinator-sweep  disabled  source=terminal  binary="), "{line}");
    assert!(line.ends_with(&format!("  root={root_arg}")), "{line}");
}

#[test]
fn path_like_names_and_slugs_are_refused() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(!hp(home.path(), &["--root", root_arg, "new", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "open", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "context", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "thread", "list", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "delete", "../x", "--force"]).status.success());
    assert!(!root.exists());
    assert!(!home.path().join("x").exists());
}

#[test]
fn ticker_start_without_projects_creates_nothing() {
    let home = tempfile::tempdir().unwrap();
    assert!(hp(home.path(), &["ticker", "start"]).status.success());
    assert!(!home.path().join(".herdr-projects").exists());
    assert!(!home.path().join(".config").exists());
}

#[cfg(unix)]

#[test]
fn lane_list_prints_the_identity_line_and_creates_no_state() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Demo"]).status.success());

    // A project that was never opened, with no reachable session: `lane list`
    // still says who it would treat as its coordinator, and says it cannot see
    // any pane. Silence is the failure this line exists to prevent.
    let out = hp(home.path(), &["--root", root_arg, "lane", "list", "demo"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "Coordinators: session unreachable (no pane checked) · default lane c-1");
    assert_eq!(lines[1], "c-1\tdefault\topen\tdefault\tDemo\tlast_active=-\tpanes=-\tthreads=0\tinbox=0");

    // --json answers the same facts.
    let out = hp(home.path(), &["--root", root_arg, "lane", "list", "demo", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let value: serde_json::Value = serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert_eq!(value[0]["id"], "c-1");
    assert_eq!(value[0]["name"], "default");
    assert_eq!(value[0]["label"], "Demo");
    assert_eq!(value[0]["status"], "open");
    assert_eq!(value[0]["default"], true);
    assert_eq!(value[0]["threads"], 0);
    assert_eq!(value[0]["inbox"], 0);
    assert_eq!(value[0]["panes"].as_array().unwrap().len(), 0);

    // Nothing was created by looking: the registry stays lazy (§10), and the
    // digest reads exactly what it read before lanes existed.
    assert!(!root.join("demo/.state/lanes.json").exists(), "a read writes no registry");
    let context = hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]);
    assert!(context.status.success(), "{}", String::from_utf8_lossy(&context.stderr));
    let digest = String::from_utf8(context.stdout).unwrap();
    assert!(
        digest.contains("Coordinators: session unreachable (no pane checked) · default lane c-1\n"),
        "{digest}"
    );
    assert!(!root.join("demo/.state/lanes.json").exists());
    assert!(!root.join("demo/.state/coordinator.json").exists(), "context records no coordinator either");

    // A slug that is not a project is refused, and an unknown lane command too.
    assert!(!hp(home.path(), &["--root", root_arg, "lane", "list", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "lane", "adopt", "demo", "c-1"]).status.success());
}

#[test]
fn task_commands_write_tasks_md_and_work_without_a_lane_registry() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Demo"]).status.success());

    let out = hp(home.path(), &["--root", root_arg, "task", "add", "demo", "Backlog", "Write docs", "--owner", "me", "--notes", "A note"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let tasks = std::fs::read_to_string(root.join("demo/TASKS.md")).unwrap();
    assert_eq!(tasks, "# Tasks\n\n## Backlog\n- [ ] Write docs (me)\n  A note\n");

    let out = hp(home.path(), &["--root", root_arg, "task", "move", "demo", "Write docs", "Later"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let tasks = std::fs::read_to_string(root.join("demo/TASKS.md")).unwrap();
    assert_eq!(tasks, "# Tasks\n\n## Backlog\n\n## Later\n- [ ] Write docs (me)\n  A note\n");

    let out = hp(home.path(), &["--root", root_arg, "task", "done", "demo", "Write docs"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let tasks = std::fs::read_to_string(root.join("demo/TASKS.md")).unwrap();
    assert!(tasks.contains("## Later\n- [x] Write docs (me)\n  A note\n"), "{tasks}");

    // No registry was created, and `context` prints the whole shared file.
    assert!(!root.join("demo/.state/lanes.json").exists());
    let out = hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let context = String::from_utf8(out.stdout).unwrap();
    assert!(context.contains("## Later\n- [x] Write docs (me)\n  notes: A note"), "{context}");
}

/// A `PROJECT.md` written by an older `hp` still carries `goal = "..."`.
/// `goal` is gone from every output and is not a setting any more, but such a
/// file must still read as an ordinary project.
#[test]
fn goal_is_gone_from_the_output_and_refused_as_a_setting() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Demo"]).status.success());
    let md = root.join("demo").join("PROJECT.md");
    let text = std::fs::read_to_string(&md).unwrap();
    let text = match text.contains("goal = \"\"\n") {
        true => text.replacen("goal = \"\"\n", "goal = \"monthly invoices\"\n", 1),
        false => text.replacen("+++\n", "+++\ngoal = \"monthly invoices\"\n", 1),
    };
    std::fs::write(&md, text).unwrap();

    // The stale line is tolerated: `context` still works…
    let out = hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let context = String::from_utf8(out.stdout).unwrap();
    assert!(context.contains("Project: demo\n"), "{context}");
    assert!(!context.contains("Goal"), "{context}");
    assert!(!context.contains("monthly invoices"), "{context}");

    // …and `overview` prints a project line with no goal tail.
    let out = hp(home.path(), &["--root", root_arg, "overview", "demo"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let overview = String::from_utf8(out.stdout).unwrap();
    assert_eq!(overview.lines().next(), Some("demo"), "{overview}");
    assert!(!overview.contains("monthly invoices"), "{overview}");

    // `hp set <slug> goal x` is an unknown setting.
    let out = hp(home.path(), &["--root", root_arg, "set", "demo", "goal", "x"]);
    assert!(!out.status.success(), "setting a goal must be refused");
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("unknown setting `goal`"), "{err}");

    // `hp new` no longer takes `--goal`.
    let out = hp(home.path(), &["--root", root_arg, "new", "Other", "--goal", "x"]);
    assert!(!out.status.success(), "`new --goal` must be refused");
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("--goal"), "{err}");
}

/// A project has two states: in use, or deleted. `pause`, `resume`, `archive`
/// and `unarchive` are gone, so their names are not subcommands any more.
#[test]
fn pause_resume_archive_unarchive_are_unknown_commands() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Demo"]).status.success());
    for command in ["pause", "resume", "archive", "unarchive"] {
        let out = hp(home.path(), &["--root", root_arg, command, "demo"]);
        assert!(!out.status.success(), "`{command}` must not be a subcommand any more");
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(err.contains(&format!("unrecognized subcommand '{command}'")), "{command}: {err}");
    }
}

/// Nothing prints a project's lifecycle state any more: `list` has two columns,
/// `context`'s `Project:` line is the slug alone, and `overview`'s first line is
/// the slug.
#[test]
fn no_output_marks_a_project_active_paused_or_archived() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Demo"]).status.success());

    let out = hp(home.path(), &["--root", root_arg, "list"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "demo\tno threads\n");

    let out = hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let context = String::from_utf8(out.stdout).unwrap();
    assert!(context.contains("Project: demo\n"), "{context}");
    for word in ["(active)", "paused", "archived"] {
        assert!(!context.contains(word), "`{word}` is not part of a project's view: {context}");
    }

    let out = hp(home.path(), &["--root", root_arg, "overview", "demo"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let overview = String::from_utf8(out.stdout).unwrap();
    assert_eq!(overview.lines().next(), Some("demo"), "{overview}");
}

/// With no project named and no terminal to ask, `overview` lists the projects
/// in use — one with a live coordinator or with a thread that is open or
/// starting — and `--all` lists every project there is.
#[test]
fn overview_lists_the_projects_in_use_and_all_lists_every_one() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Alpha"]).status.success());
    assert!(hp(home.path(), &["--root", root_arg, "new", "Beta"]).status.success());
    // Alpha has one open thread; beta has nothing running at all.
    let record = "id = \"t-0001\"\ntitle = \"Do it\"\nstatus = \"open\"\nkind = \"home\"\n";
    std::fs::write(root.join("alpha/threads/t-0001.toml"), record).unwrap();

    let out = hp(home.path(), &["--root", root_arg, "overview"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("alpha\n"), "{text}");
    assert!(!text.contains("beta"), "a project with nothing running is not in the default view: {text}");

    let out = hp(home.path(), &["--root", root_arg, "overview", "--all"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("alpha\n"), "{text}");
    assert!(text.contains("beta\n"), "--all lists every project: {text}");
}

/// `list --json` answers the same numbers as the text, keyed by the group's
/// stable token instead of its prose label: a script reads tokens, never
/// English. Reading writes nothing, as the text form does.
#[test]
fn list_json_carries_the_same_counts_as_the_text() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Alpha"]).status.success());
    assert!(hp(home.path(), &["--root", root_arg, "new", "Beta"]).status.success());
    // Alpha has one open thread; beta has nothing at all.
    let record = "id = \"t-0001\"\ntitle = \"Do it\"\nstatus = \"open\"\nkind = \"home\"\n";
    std::fs::write(root.join("alpha/threads/t-0001.toml"), record).unwrap();

    let text = hp(home.path(), &["--root", root_arg, "list"]);
    assert!(text.status.success(), "{}", String::from_utf8_lossy(&text.stderr));
    let text = String::from_utf8(text.stdout).unwrap();
    let out = hp(home.path(), &["--root", root_arg, "list", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let value: serde_json::Value = serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    let projects = value.as_array().expect("--json is one array of projects");
    assert_eq!(projects.len(), 2, "{value}");

    // Every project the text prints, with the same summary it prints, is here:
    // the groups' labels and counts in rank order join to the text's column.
    for line in text.lines() {
        let (slug, summary) = line.split_once('\t').expect("slug and summary, tab separated");
        let project = projects.iter().find(|p| p["slug"] == slug).unwrap_or_else(|| panic!("`{slug}` is in the text but not in the JSON"));
        assert_eq!(project["folder"], root.join(slug).to_string_lossy().as_ref());
        assert!(!project["name"].as_str().unwrap().is_empty(), "a project says the name a person reads");
        assert_eq!(project["name"], if slug == "alpha" { "Alpha" } else { "Beta" });
        let groups = project["groups"].as_array().unwrap();
        let joined: Vec<String> = groups
            .iter()
            .map(|g| format!("{}: {}", g["label"].as_str().unwrap(), g["count"].as_u64().unwrap()))
            .collect();
        let joined = if joined.is_empty() { "no threads".to_string() } else { joined.join(", ") };
        assert_eq!(joined, summary, "the JSON must answer what the text answers");
        for g in groups {
            assert!(!g["token"].as_str().unwrap().contains(' '), "a token is never prose: {g}");
            assert!(!g["token"].as_str().unwrap().is_empty(), "every group has a token: {g}");
        }
        let total: u64 = groups.iter().map(|g| g["count"].as_u64().unwrap()).sum();
        assert_eq!(project["threads"].as_u64().unwrap(), total);
    }
    assert_eq!(projects[0]["threads"], 1, "{value}");
    assert_eq!(projects[1]["threads"], 0, "{value}");
    assert_eq!(projects[1]["groups"].as_array().unwrap().len(), 0, "{value}");

    // Reading marks nothing seen and writes no state: `list` is a look.
    assert!(!root.join("alpha/.state/inbox-seen.json").exists());
}

/// `PROJECT.md` written by an older `hp` may carry a `status` line (and the old
/// state file a `status` key). Both must read as an ordinary, in-use project.
#[test]
fn an_old_status_line_does_not_make_a_project_anything_but_a_project() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Demo"]).status.success());
    let md = root.join("demo").join("PROJECT.md");
    let text = std::fs::read_to_string(&md).unwrap();
    std::fs::write(&md, text.replacen("+++\n", "+++\nstatus = \"archived\"\n", 1)).unwrap();
    std::fs::write(root.join("demo/.state/project.json"), "{\"status\":\"archived\",\"former_slugs\":[\"old-demo\"]}").unwrap();

    let out = hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]);
    assert!(out.status.success(), "an old `status` line must not break the project: {}", String::from_utf8_lossy(&out.stderr));
    let context = String::from_utf8(out.stdout).unwrap();
    assert!(context.contains("Project: demo\n"), "{context}");
    assert!(!context.contains("archived"), "{context}");

    // It is listed and opened like any other project, not hidden or refused.
    let out = hp(home.path(), &["--root", root_arg, "list"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "demo\tno threads\n");
}

/// The commands the printed help and error messages name have to be the ones
/// that exist, and the flags a user could still type have to be the ones that
/// do something.
#[test]
fn dead_arguments_are_refused_and_nothing_names_the_old_binary() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Demo"]).status.success());

    // `assignable --refresh` re-looked-up other machines' profiles, and the
    // other machines are gone: the flag is not accepted any more.
    let out = hp(home.path(), &["--root", root_arg, "assignable", "demo", "--refresh"]);
    assert!(!out.status.success(), "`assignable --refresh` must be refused");
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("--refresh"), "{err}");

    // `open-file --workspace` promised a tab this binary never opens.
    let file = root.join("demo/notes.txt");
    std::fs::write(&file, "x").unwrap();
    let out = hp(home.path(), &["--root", root_arg, "open-file", file.to_str().unwrap(), "--workspace", "w1"]);
    assert!(!out.status.success(), "`open-file --workspace` must be refused");
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("--workspace"), "{err}");

    // `doctor` checked for `ssh`, which only the deleted remote machines
    // needed, and its failure line is the `hp` command, not the old binary.
    let out = hp(home.path(), &["--root", root_arg, "doctor"]);
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(!text.contains(" ssh:"), "the ssh check is gone: {text}");
    assert!(!text.contains("herdr-projects: "), "the failure line names `hp`: {text}");
    assert!(text.contains("hp: "), "{text}");

    // `safety show` prints commands to copy: they have to be `hp`.
    let out = hp(home.path(), &["--root", root_arg, "safety", "show", "demo"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("hp safety yolo demo on|off|default"), "{text}");
    assert!(text.contains("hp profile list --project demo"), "{text}");
    assert!(!text.contains("herdr-projects safety"), "{text}");
    assert!(!text.contains("herdr-projects profile"), "{text}");
}

/// `--kind` says where a thread works, which is all that is left of it: every
/// kind puts the tab in the project's thread Space, so a help text promising
/// "a Space of its own" is a lie a user can check in the sidebar
/// (`docs/thread-space.md` §2).
#[test]
fn thread_start_help_describes_where_a_thread_works_not_a_space_of_its_own() {
    let home = tempfile::tempdir().unwrap();
    let out = hp(home.path(), &["thread", "start", "--help"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains("Space of its own"), "no kind has a Space of its own any more: {text}");
    // The three placements, by the names `--kind` accepts, and where each works.
    assert!(text.contains("--kind <worktree|checkout|local>") || text.contains("--kind <worktree|local|checkout>"), "{text}");
    for want in ["its own worktree and branch", "the repository's main checkout", "threads/<id>/"] {
        assert!(text.contains(want), "`{want}` is what a placement decides: {text}");
    }
    assert!(text.contains("default with --repo") && text.contains("default without a repo"), "the two defaults are stated: {text}");
}

/// A record written before `tab`/`home` became `local` reads as `local`:
/// every tick, the brief and the restart read that value, and a value that
/// failed to parse would make the thread vanish from all of them.
#[test]
fn a_record_from_before_the_merge_still_reads_and_says_local() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Demo"]).status.success());
    for (id, raw) in [("t-0001", "home"), ("t-0002", "tab")] {
        let dir = root.join("demo").join("threads").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let record = format!(
            "id = \"{id}\"\ntitle = \"研究 {raw}\"\nstatus = \"open\"\nkind = \"{raw}\"\ncwd = \"{}\"\n",
            dir.to_str().unwrap()
        );
        std::fs::write(root.join("demo").join("threads").join(format!("{id}.toml")), record).unwrap();

        let out = hp(home.path(), &["--root", root_arg, "thread", "show", "demo", id]);
        assert!(out.status.success(), "kind = \"{raw}\" must still parse: {}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains("kind = \"local\""), "the old value reads as `local` (and is written that way): {text}");
        assert!(text.contains(&format!("title = \"研究 {raw}\"")), "{text}");
    }
    // And both are in the project's threads, with the placement it now means.
    let out = hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("研究 home") && text.contains("研究 tab"), "both threads are still seen: {text}");
}

/// `--kind tab` and `--kind home` still work (people have the habit), and say
/// once, on stderr, that `local` is the name now. They are not refused.
#[test]
fn the_old_kind_names_still_work_and_say_so() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Demo"]).status.success());
    for old in ["tab", "home"] {
        // No session in this environment, so the start stops after parsing the
        // kind: the warning is what this test is about.
        let out = hp(home.path(), &["--root", root_arg, "thread", "start", "demo", "--title", "x", "--kind", old, "--task-file", "-"]);
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(err.contains(&format!("`{old}` is the old name")), "`--kind {old}` says what to use instead: {err}");
        assert!(!err.contains(&format!("`{old}` is not a thread kind")), "`--kind {old}` is not refused: {err}");
    }
}

/// An on-request profile (`on_request = true`) is the user's to name: no
/// project's allow-list has to carry it, and nothing else may pick it — not
/// the project default, not `open`, not a task owner. `context` says all of
/// this on its own line (t-0088).
#[test]
fn an_on_request_profile_is_listed_apart_and_never_a_default_or_an_owner() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join(".config/herdr-projects");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("config.toml"),
        "[profiles.sol]\nagent = \"codex\"\nmodel = \"gpt-6.2-sol\"\neffort = \"medium\"\ndescription = \"Sol at medium effort\"\non_request = true\n\n[profiles.luna]\nagent = \"omp\"\ndescription = \"Cheap tier\"\n\n[defaults]\nthread_profile = \"luna\"\ncoordinator_profile = \"luna\"\n\n[safety.default]\nthread_profiles = [\"luna\"]\ncoordinator_profiles = [\"luna\"]\n",
    )
    .unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "demo"]).status.success());

    // `context`: its own line, and the ordinary list is untouched.
    let out = hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let line = text.lines().find(|l| l.starts_with("On request only:")).unwrap_or_else(|| panic!("no on-request line: {text}"));
    assert!(line.contains("sol (codex gpt-6.2-sol, medium) — Sol at medium effort"), "{line}");
    assert!(!line.contains("luna"), "{line}");
    assert!(text.contains("- luna: omp · default model — Cheap tier"), "{text}");
    assert!(text.contains("Assignable (agent owners): luna"), "{text}");

    // `profile list` marks it at the end of its line.
    let out = hp(home.path(), &["--root", root_arg, "profile", "list"]);
    let text = String::from_utf8(out.stdout).unwrap();
    let line = text.lines().find(|l| l.trim_start().starts_with("sol ")).unwrap_or_else(|| panic!("no `sol` line: {text}"));
    assert!(line.trim_end().ends_with("· on request"), "{line}");
    assert!(!text.lines().any(|l| l.trim_start().starts_with("luna ") && l.contains("on request")), "{text}");

    // Never a default, not even by a coordinator's own hand.
    let out = hp(home.path(), &["--root", root_arg, "set", "demo", "thread_profile", "sol"]);
    assert!(!out.status.success(), "an on-request profile cannot be a project default");
    assert!(String::from_utf8_lossy(&out.stderr).contains("is not allowed for threads"), "{}", String::from_utf8_lossy(&out.stderr));

    // Never an owner.
    let out = hp(home.path(), &["--root", root_arg, "task", "add", "demo", "Backlog", "Do a thing", "--owner", "sol"]);
    assert!(!out.status.success(), "an on-request profile cannot own a task");
    assert!(String::from_utf8_lossy(&out.stderr).contains("`sol` is on request only"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!root.join("demo/TASKS.md").exists() || !std::fs::read_to_string(root.join("demo/TASKS.md")).unwrap().contains("Do a thing"));
    assert!(hp(home.path(), &["--root", root_arg, "task", "add", "demo", "Backlog", "Do another thing"]).status.success());
    let out = hp(home.path(), &["--root", root_arg, "task", "thread", "demo", "Do another thing", "t-0001", "--owner", "sol"]);
    assert!(!out.status.success(), "an on-request profile cannot own a task");
    assert!(String::from_utf8_lossy(&out.stderr).contains("`sol` is on request only"), "{}", String::from_utf8_lossy(&out.stderr));
    // `assignable --check` refuses it too.
    let out = hp(home.path(), &["--root", root_arg, "assignable", "demo", "--check", "sol"]);
    assert!(!out.status.success(), "`assignable --check` refuses an on-request profile");
    assert!(String::from_utf8_lossy(&out.stderr).contains("assignable: luna"), "{}", String::from_utf8_lossy(&out.stderr));

    // A coordinator may not be opened with it.
    let out = hp(home.path(), &["--root", root_arg, "open", "demo", "--profile", "sol"]);
    assert!(!out.status.success(), "an on-request profile is not for coordinators");
    assert!(String::from_utf8_lossy(&out.stderr).contains("is not allowed for the coordinator"), "{}", String::from_utf8_lossy(&out.stderr));
}
