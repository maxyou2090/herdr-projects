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
