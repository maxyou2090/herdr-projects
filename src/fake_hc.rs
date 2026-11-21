//! A fake herdr-core for tests: an in-memory set of terminals that answers the
//! `hc` commands this binary runs, so a test exercises the real adapter
//! ([`crate::core`]) instead of a canned CLI.
//!
//! Only what the adapter calls is modelled: `--version`, `server status`, `ls`,
//! `get`, `run --bg`, `label`, `send`, `keys`, `read`, `kill`, `rm`. Behaviour
//! follows the real core where a test would notice: `run` answers with the
//! short row `ls` prints, `get` with the whole terminal, `send` refuses a
//! `blocked` program, and a killed terminal is `finished`.

use std::cell::RefCell;

use anyhow::Result;

use crate::runner::{Cmd, Output, Runner};

#[derive(Debug, Clone, Default)]
pub struct Terminal {
    pub address: String,
    pub name: String,
    pub labels: std::collections::BTreeMap<String, String>,
    pub cwd: String,
    /// What the program last said over OSC 7501: idle, working, done, blocked…
    pub status: Option<String>,
    pub turn: u64,
    pub state: String,
    pub screen: String,
    pub argv: Vec<String>,
    /// Why core ended it, once it is `finished`; empty while it runs.
    pub reason: String,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    /// Sends still to refuse the way a terminal whose program exited does, for a
    /// pane that dies while `open` is between listing it and typing the launch
    /// (see [`FakeCore::finished_on_send`]).
    pub fail_sends: usize,
    /// When a killed program will be gone, for a terminal that takes its time
    /// to die (see [`FakeCore::dies_in`]). `None` means the kill lands at once.
    pub exits_at: Option<std::time::Instant>,
}

#[derive(Default)]
pub struct FakeCore {
    pub terminals: RefCell<Vec<Terminal>>,
    /// Every command line the adapter ran, for assertions.
    pub calls: RefCell<Vec<String>>,
    next: RefCell<u32>,
}

fn ok(stdout: impl Into<String>) -> Result<Output> {
    Ok(Output { code: Some(0), stdout: stdout.into(), stderr: String::new(), timed_out: false })
}

fn fail(kind: &str, message: &str) -> Result<Output> {
    let body = serde_json::json!({"error": {"kind": kind, "message": message, "hint": ""}});
    Ok(Output { code: Some(1), stdout: String::new(), stderr: body.to_string(), timed_out: false })
}

fn short(term: &Terminal) -> serde_json::Value {
    serde_json::json!({
        "address": term.address,
        "name": (!term.name.is_empty()).then(|| term.name.clone()),
        "cwd": term.cwd,
        "state": term.state,
        "status": term.status,
        "turn": term.turn,
    })
}

fn full(term: &Terminal) -> serde_json::Value {
    serde_json::json!({
        "address": term.address,
        // core reports no name for a terminal it did not name itself
        "name": (!term.name.is_empty()).then(|| term.name.clone()),
        "labels": term.labels,
        "cwd": term.cwd,
        "last_cwd": term.cwd,
        "state": term.state,
        "status": term.status.as_ref().map(|state| serde_json::json!({"app": "pi", "state": state})),
        "turn": term.turn,
        "reported": term.status.is_some(),
        "argv": term.argv,
        // The real core writes an explicit `null` here while the terminal runs:
        // a fake that answers `""` would let a `String` field pass a test the
        // real reply fails.
        "reason": (!term.reason.is_empty()).then(|| term.reason.clone()),
        "pid": term.pid,
        "exit_code": term.exit_code,
    })
}

impl FakeCore {
    pub fn new() -> Self {
        FakeCore::default()
    }

    /// A live terminal running `argv`, as `hc run --bg` would leave it.
    pub fn terminal(&self, name: &str, cwd: &str, labels: &[(&str, &str)]) -> Terminal {
        let mut next = self.next.borrow_mut();
        *next += 1;
        let address = format!("t{}", *next);
        let term = Terminal {
            address,
            name: name.to_string(),
            labels: labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            cwd: cwd.to_string(),
            status: None,
            turn: 0,
            state: "live".into(),
            screen: String::new(),
            argv: vec!["/bin/zsh".into()],
            reason: String::new(),
            pid: None,
            exit_code: None,
            fail_sends: 0,
            exits_at: None,
        };
        self.terminals.borrow_mut().push(term.clone());
        term
    }

    /// Ends a terminal the way core does when the program dies: `ls` stops
    /// listing it, `get` keeps answering with `state: finished` and why.
    pub fn finish(&self, name: &str, reason: &str) {
        if let Some(mut term) = self.find_mut(name) {
            term.state = "finished".into();
            term.status = None;
            term.reason = reason.to_string();
            term.pid = Some(2515);
            term.exit_code = Some(1);
        }
    }

    /// Makes a killed terminal take `after` to be gone, the way a real program
    /// does: core hangs up, the process takes tens of milliseconds to exit, and
    /// until then `get`/`ls` still say `live` and `rm` is refused (`live`, "…
    /// is still running"). Without this a kill lands in the same instant, which
    /// is the shape no real program has — and the fake asserted nothing about
    /// waiting for the exit (t-0071).
    pub fn dies_in(&self, name: &str, after: std::time::Duration) {
        if let Some(mut term) = self.find_mut(name) {
            term.exits_at = Some(std::time::Instant::now() + after);
        }
    }

    /// Terminals whose program is now gone: a killed one whose grace has passed
    /// is `finished`, with the reason core reports for a hangup.
    fn expire(&self) {
        let now = std::time::Instant::now();
        for term in self.terminals.borrow_mut().iter_mut() {
            if let Some(at) = term.exits_at
                && term.state == "live"
                && now >= at
            {
                term.state = "finished".into();
                term.status = None;
                term.reason = "killed".into();
                term.pid = Some(2515);
                term.exit_code = Some(1);
                term.exits_at = None;
            }
        }
    }

    /// Makes the next `times` sends to this terminal fail the way core fails one
    /// whose program has exited — `finished`, "‹pane› has finished" — while `ls`
    /// and `get` keep answering as for a live one. That is the shape `open` sees
    /// when the pane it just listed is closed before it types the launch
    /// (my-net, 2026-11-21).
    pub fn finished_on_send(&self, name: &str, times: usize) {
        if let Some(mut term) = self.find_mut(name) {
            term.fail_sends = times;
        }
    }

    pub fn set_status(&self, name: &str, status: &str) {
        if let Some(mut term) = self.find_mut(name) {
            if term.status.as_deref() == Some("working") && status != "working" {
                term.turn += 1;
            }
            term.status = Some(status.to_string());
        }
    }

    pub fn set_screen(&self, name: &str, text: &str) {
        if let Some(mut term) = self.find_mut(name) {
            term.screen = text.to_string();
        }
    }

    fn find(&self, id: &str) -> Option<Terminal> {
        self.terminals.borrow().iter().find(|t| t.name == id || t.address == id).cloned()
    }

    fn find_mut(&self, id: &str) -> Option<std::cell::RefMut<'_, Terminal>> {
        let index = self.terminals.borrow().iter().position(|t| t.name == id || t.address == id)?;
        let terminals = self.terminals.borrow_mut();
        Some(std::cell::RefMut::map(terminals, move |all| &mut all[index]))
    }

    /// The one call path: `hc --json <command> <args...>`.
    fn dispatch(&self, args: &[String]) -> Result<Output> {
        self.expire();
        let mut args: Vec<&str> = args.iter().map(String::as_str).collect();
        let json = !args.is_empty() && args[0] == "--json";
        if json {
            args.remove(0);
        }
        match (args.first().copied().unwrap_or_default(), &args[1..]) {
            ("--version", _) => ok("hc 0.1.21\n"),
            ("server", ["status"]) => ok("{\"core\":{\"id\":\"fake\"}}\n"),
            ("ls", _) => {
                let live: Vec<serde_json::Value> = self
                    .terminals
                    .borrow()
                    .iter()
                    .filter(|t| t.state == "live")
                    .map(short)
                    .collect();
                let finished = self.terminals.borrow().iter().filter(|t| t.state != "live").count();
                ok(serde_json::json!({"terminals": live, "hidden_finished": finished}).to_string())
            }
            ("get", [id]) => match self.find(id) {
                Some(term) => ok(full(&term).to_string()),
                None => fail("not_found", &format!("no terminal {id}")),
            },
            ("run", rest) => {
                let mut name = String::new();
                let mut cwd = String::new();
                let mut labels: Vec<(String, String)> = Vec::new();
                let mut index = 0;
                while index < rest.len() {
                    match rest[index] {
                        "--bg" | "--no-parent" | "--keep" => index += 1,
                        "--name" => {
                            name = rest.get(index + 1).copied().unwrap_or_default().to_string();
                            index += 2;
                        }
                        "--cwd" => {
                            cwd = rest.get(index + 1).copied().unwrap_or_default().to_string();
                            index += 2;
                        }
                        "--label" => {
                            if let Some(pair) = rest.get(index + 1)
                                && let Some((k, v)) = pair.split_once('=')
                            {
                                labels.push((k.to_string(), v.to_string()));
                            }
                            index += 2;
                        }
                        _ => index += 1,
                    }
                }
                if self.terminals.borrow().iter().any(|t| t.name == name && t.state == "live") {
                    return fail("name_conflict", &format!("a live terminal is named {name}"));
                }
                let term = self.terminal(&name, &cwd, &labels.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>());
                ok(short(&term).to_string())
            }
            ("label", [id, pairs @ ..]) => {
                if let Some(mut term) = self.find_mut(id) {
                    for pair in pairs.iter() {
                        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                        if value.is_empty() {
                            term.labels.remove(key);
                        } else {
                            term.labels.insert(key.to_string(), value.to_string());
                        }
                    }
                    return ok(full(&term).to_string());
                }
                fail("not_found", &format!("no terminal {id}"))
            }
            ("send", [id, text @ ..]) => {
                let Some(mut term) = self.find_mut(id) else {
                    return fail("not_found", &format!("no terminal {id}"));
                };
                if term.fail_sends > 0 {
                    term.fail_sends -= 1;
                    return fail("finished", &format!("{id} has finished"));
                }
                if term.status.as_deref() == Some("blocked") {
                    return fail("blocked", "the program is waiting for a person; pass --force to type anyway");
                }
                // The typed line: an `hp-agent` launch reports a status at once,
                // exactly as the real wrapper does.
                let line = text.join(" ");
                if line.contains("hp-agent") {
                    term.status = Some("idle".into());
                } else if term.status.is_some() {
                    term.status = Some("working".into());
                    term.turn += 1;
                }
                ok(serde_json::json!({"status": term.status, "turn": term.turn, "started": term.status.is_some()}).to_string())
            }
            ("keys", [id, keys @ ..]) => {
                let _ = keys;
                match self.find(id) {
                    Some(term) => ok(serde_json::json!({"status": term.status, "turn": term.turn}).to_string()),
                    None => fail("not_found", &format!("no terminal {id}")),
                }
            }
            ("read", [id, rest @ ..]) => {
                let _ = rest;
                match self.find(id) {
                    Some(term) => ok(serde_json::json!({"text": term.screen, "complete": true, "how": "screen"}).to_string()),
                    None => fail("not_found", &format!("no terminal {id}")),
                }
            }
            // The hangup lands at once, the program it hit need not: a
            // terminal with a grace still says `live` until it is gone.
            ("kill", [id]) => match self.find_mut(id) {
                Some(mut term) => {
                    if term.exits_at.is_none() {
                        term.state = "finished".into();
                        term.status = None;
                        term.reason = "killed".into();
                    }
                    ok(short(&term).to_string())
                }
                None => fail("not_found", &format!("no terminal {id}")),
            },
            ("rm", [id]) => match self.find(id) {
                // Core refuses a terminal whose program still runs, with
                // `kind: live`.
                Some(term) if term.state == "live" => fail("live", &format!("{} is still running", term.address)),
                Some(_) => {
                    self.terminals.borrow_mut().retain(|t| t.name != *id && t.address != *id);
                    ok("{\"removed\":\"\"}".to_string())
                }
                None => fail("not_found", &format!("no terminal {id}")),
            },
            (other, _) => fail("unsupported", &format!("the fake core does not model `{other}`")),
        }
    }
}

impl Runner for FakeCore {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        self.calls.borrow_mut().push(cmd.display());
        self.dispatch(&cmd.args)
    }

    fn run_foreground(&self, cmd: &Cmd, _poll: &mut dyn FnMut() -> bool) -> Result<Option<i32>> {
        self.calls.borrow_mut().push(cmd.display());
        Ok(Some(0))
    }
}

/// The desktop app over a [`FakeCore`]: `herdr-app status`, `ls` and
/// `do space.new / tab.close / tab.rename`. A space it makes starts with a
/// placeholder tab (the app will not leave a space without one), and every
/// core terminal carrying `herdr.space` shows up in a tab of its own, exactly
/// as the window puts it there.
pub struct FakeApp<'a> {
    pub core: &'a FakeCore,
    /// Every command line, `herdr-app` and `hc` alike.
    pub calls: RefCell<Vec<String>>,
    /// The spaces it made: (space id, the name it was asked for).
    pub spaces: RefCell<Vec<(String, String)>>,
    /// Every `tab.rename` it was asked for: (tab id, name).
    pub renamed: RefCell<Vec<(String, String)>>,
    /// The name each tab shows; a tab it never named is called `shell`, like
    /// the placeholder tab of a Space it made.
    pub tab_names: RefCell<std::collections::BTreeMap<String, String>>,
    /// A command the window refuses, by name: how a failed rename is tested.
    pub refuse: RefCell<Option<String>>,
    next: RefCell<u32>,
}

impl<'a> FakeApp<'a> {
    pub fn new(core: &'a FakeCore) -> Self {
        FakeApp {
            core,
            calls: RefCell::new(Vec::new()),
            spaces: RefCell::new(Vec::new()),
            renamed: RefCell::new(Vec::new()),
            tab_names: RefCell::new(std::collections::BTreeMap::new()),
            refuse: RefCell::new(None),
            next: RefCell::new(0),
        }
    }

    /// A Space the window already shows, the way a person made it: a name and a
    /// terminal in it, with none of hp's labels on that terminal.
    pub fn shown_space(&self, name: &str) -> String {
        let mut next = self.next.borrow_mut();
        *next += 1;
        let space = format!("sp{next}");
        drop(next);
        self.spaces.borrow_mut().push((space.clone(), name.to_string()));
        space
    }

    /// The name each tab shows.
    fn tab_name(&self, tab: &str) -> String {
        self.tab_names.borrow().get(tab).cloned().unwrap_or_else(|| "shell".to_string())
    }

    /// The tab the window ties each core terminal to.
    fn tab_for(address: &str) -> String {
        format!("apptab-{address}")
    }

    /// `herdr-app ls`: spaces, their tabs, and the terminals in each.
    fn ls_json(&self) -> String {
        let terminals = self.core.terminals.borrow();
        let spaces: Vec<serde_json::Value> = self
            .spaces
            .borrow()
            .iter()
            .map(|(space, name)| {
                let mut tabs = vec![serde_json::json!({
                    "id": format!("placeholder-{space}"),
                    "name": self.tab_name(&format!("placeholder-{space}")),
                    "terminals": [],
                })];
                for term in terminals.iter().filter(|t| t.labels.get("herdr.space") == Some(space)) {
                    let tab = Self::tab_for(&term.address);
                    tabs.push(serde_json::json!({
                        "id": tab,
                        "name": self.tab_name(&tab),
                        "terminals": [{"terminal": term.address}],
                    }));
                }
                serde_json::json!({"id": space, "name": name, "tabs": tabs})
            })
            .collect();
        serde_json::json!({"spaces": spaces}).to_string()
    }

    fn dispatch(&self, args: &[String]) -> Result<Output> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        if let Some(refused) = self.refuse.borrow().as_deref()
            && args.get(1) == Some(&refused)
        {
            return fail("refused", &format!("the window did not run {refused}"));
        }
        match args.as_slice() {
            ["status"] => ok("{\"version\":\"0.1.16\"}"),
            ["ls"] => ok(self.ls_json()),
            ["do", "space.new", rest @ ..] => {
                let name = flag(rest, "--name");
                let mut next = self.next.borrow_mut();
                *next += 1;
                let space = format!("sp{next}");
                drop(next);
                self.spaces.borrow_mut().push((space.clone(), name));
                ok(serde_json::json!({"space": space, "tab": format!("placeholder-{space}"), "terminal": "tshell"}).to_string())
            }
            ["do", "space.rename", rest @ ..] => {
                let id = flag(rest, "--id");
                let name = flag(rest, "--name");
                match self.spaces.borrow_mut().iter_mut().find(|(space, _)| *space == id) {
                    Some(entry) => {
                        entry.1 = name;
                        ok("{}")
                    }
                    None => fail("not_found", &format!("no space {id}")),
                }
            }
            ["do", "tab.close", _rest @ ..] => ok("{}"),
            ["do", "tab.rename", rest @ ..] => {
                let id = flag(rest, "--id");
                let name = flag(rest, "--name");
                self.tab_names.borrow_mut().insert(id.clone(), name.clone());
                self.renamed.borrow_mut().push((id, name));
                ok("{}")
            }
            other => fail("unsupported", &format!("the fake app does not model `{}`", other.join(" "))),
        }
    }
}

/// `--name`'s value: the app takes its arguments as flags.
fn flag(args: &[&str], name: &str) -> String {
    args.iter().position(|arg| *arg == name).and_then(|at| args.get(at + 1)).copied().unwrap_or_default().to_string()
}

impl Runner for FakeApp<'_> {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        self.calls.borrow_mut().push(cmd.display());
        if cmd.program == crate::app::bin() {
            self.dispatch(&cmd.args)
        } else {
            self.core.run(cmd)
        }
    }

    fn run_foreground(&self, cmd: &Cmd, poll: &mut dyn FnMut() -> bool) -> Result<Option<i32>> {
        self.calls.borrow_mut().push(cmd.display());
        self.core.run_foreground(cmd, poll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{Core, L_TAB_LABEL};

    fn core() -> (tempfile::TempDir, FakeCore) {
        (tempfile::tempdir().unwrap(), FakeCore::new())
    }

    #[test]
    fn creating_a_space_and_a_tab_yields_distinct_ids_and_live_panes() {
        let (home, fake) = core();
        let herdr = Core::new("hc", "", &fake);
        let dir = home.path();
        let first = herdr.workspace_create(dir, "Demo", "Demo").unwrap();
        let second = herdr.tab_create(&first.workspace_id, dir, "thread").unwrap();
        assert_ne!(first.pane_id, second.pane_id, "each terminal is its own pane");
        assert_ne!(first.tab_id, second.tab_id, "a tab is its own terminal");
        assert_eq!(herdr.pane_list().unwrap().len(), 2);
        assert_eq!(herdr.workspace_list().unwrap().len(), 1, "both are in one Space");
    }

    #[test]
    fn labels_are_the_tokens_the_lane_resolver_reads() {
        let (_home, fake) = core();
        let herdr = Core::new("hc", "", &fake);
        let term = fake.terminal("hp-demo-1", "/tmp/demo", &[("hp_agent", "hpc-demo"), ("hp_kind", "pi")]);
        fake.set_status("hp-demo-1", "idle");
        herdr.set_labels(&term.name, &[("hp_lane".to_string(), "c-1".to_string())]).unwrap();
        let agent = herdr.agent_list().unwrap().into_iter().find(|a| a.pane_id == "hp-demo-1").unwrap();
        assert_eq!(agent.tokens.get("hp_lane").map(String::as_str), Some("c-1"));
        assert_eq!(agent.name, "hpc-demo");
        assert!(agent.ready(), "a program that reported idle is ready");
    }

    #[test]
    fn a_prompt_is_refused_while_the_program_is_blocked() {
        let (_home, fake) = core();
        let herdr = Core::new("hc", "", &fake);
        fake.terminal("hp-demo-1", "/tmp/demo", &[("hp_agent", "hpc-demo")]);
        fake.set_status("hp-demo-1", "blocked");
        let error = herdr.agent_prompt("hp-demo-1", "hello").unwrap_err();
        assert_eq!(error.code, "agent_blocked");
        assert!(crate::core::refused_before_typing(&error));
    }

    #[test]
    fn starting_an_agent_types_the_wrapper_and_waits_for_its_first_word() {
        let (_home, fake) = core();
        let herdr = Core::new("hc", "", &fake);
        fake.terminal("hp-demo-1", "/tmp/demo", &[]);
        let agent = herdr.agent_start("hpc-demo", "pi", "hp-demo-1", &["--model".into(), "auto/x".into()]).unwrap();
        assert_eq!(agent.name, "hpc-demo");
        assert_eq!(agent.agent, "pi");
        let sent = fake.calls.borrow().iter().any(|c| c.contains("hp-agent") && c.contains("--model")).to_string();
        assert_eq!(sent, "true", "the launch line runs the wrapper, not the bare agent");
        assert_eq!(herdr.agent_kind("hp-demo-1").as_deref(), Some("pi"));
    }

    #[test]
    fn reading_returns_the_screen_the_screen_check_parses() {
        let (_home, fake) = core();
        let herdr = Core::new("hc", "", &fake);
        fake.terminal("hp-demo-1", "/tmp/demo", &[]);
        fake.set_screen("hp-demo-1", "hello\n❯ \n");
        assert_eq!(herdr.agent_read("hp-demo-1", None).unwrap(), "hello\n❯ \n");
    }

    #[test]
    fn a_thread_space_is_the_project_s_and_its_tabs_are_the_thread_ids() {
        let (home, child) = core();
        let fake = FakeApp::new(&child);
        let herdr = Core::new("hc", "", &fake);
        let space = crate::labels::threads_space_label("Herdr Projects");
        let tab = crate::labels::thread_tab_label("t-0047");
        let created = herdr.workspace_create(home.path(), &space, &tab).unwrap();
        assert_eq!(fake.spaces.borrow()[0].1, "Herdr Projects Threads", "the Space is the project's, not the thread's");
        let address = &child.terminals.borrow()[0].address;
        assert_eq!(fake.renamed.borrow().last(), Some(&(FakeApp::tab_for(address), tab.clone())), "the app's tab carries the id too");
        let label = child.terminals.borrow()[0].labels.get(L_TAB_LABEL).cloned();
        assert_eq!(label.as_deref(), Some("t-0047"), "the core label says the same, so there is one name");
        assert!(!created.pane_id.is_empty() && !created.tab_id.is_empty());
    }

    /// A second thread joins the Space through `tab_create`, on a Space the app
    /// made: the new terminal has to carry the Space's name as well, or
    /// `workspace_list` reports the Space under an empty label as soon as it
    /// lists that terminal first, and the next thread opens a second Space.
    #[test]
    fn a_tab_created_in_an_app_space_carries_the_space_s_name() {
        let (home, child) = core();
        let fake = FakeApp::new(&child);
        let herdr = Core::new("hc", "", &fake);
        let space = crate::labels::threads_space_label("REXA");
        let first = herdr.workspace_create(home.path(), &space, "t-0001").unwrap();
        let second = herdr.tab_create(&first.workspace_id, home.path(), "t-0002").unwrap();
        let listed = herdr.workspace_list().unwrap();
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].label, "REXA Threads", "the second terminal says the name too");
        assert_eq!(listed[0].pane_count, 2);
        assert!(listed[0].worktree.is_none());
        assert_eq!(second.workspace_id, first.workspace_id);
        assert_ne!(second.tab_id, first.tab_id);
    }

    /// Two worktrees in one Space: the Space is not a worktree Space, and a
    /// removal keyed on a pane takes that pane's worktree and no other.
    #[test]
    fn a_space_holding_two_worktrees_is_not_a_worktree_space() {
        let (_home, fake) = core();
        let herdr = Core::new("hc", "", &fake);
        let one = herdr.workspace_create(std::path::Path::new("/tmp"), "Demo Threads", "t-0001").unwrap();
        let two = herdr.tab_create(&one.workspace_id, std::path::Path::new("/tmp"), "t-0002").unwrap();
        herdr.worktree_mark(&one.pane_id, "/wt/a").unwrap();
        herdr.worktree_mark(&two.pane_id, "/wt/b").unwrap();
        let listed = herdr.workspace_list().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].worktree.is_none(), "many worktrees at once, so this Space is not one of them");
        // One worktree alone: still a worktree Space, as before.
        let alone = core().1;
        let herdr = Core::new("hc", "", &alone);
        herdr.workspace_create(std::path::Path::new("/tmp"), "old", "t-0001").unwrap();
        let pane = herdr.pane_list().unwrap()[0].pane_id.clone();
        herdr.worktree_mark(&pane, "/wt/only").unwrap();
        assert!(herdr.workspace_list().unwrap()[0].worktree.is_some());
    }

    #[test]
    fn a_project_space_keeps_its_coordinator_tab() {
        let (home, child) = core();
        let fake = FakeApp::new(&child);
        let herdr = Core::new("hc", "", &fake);
        herdr.workspace_create(home.path(), "Herdr Projects", "coordinator").unwrap();
        assert_eq!(fake.spaces.borrow()[0].1, "Herdr Projects", "the Space is the project, not the tab");
        let address = &child.terminals.borrow()[0].address;
        assert_eq!(fake.renamed.borrow().last(), Some(&(FakeApp::tab_for(address), "coordinator".to_string())));
        let label = child.terminals.borrow()[0].labels.get(L_TAB_LABEL).cloned();
        assert_eq!(label.as_deref(), Some("coordinator"));
    }
}
