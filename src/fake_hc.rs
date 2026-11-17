//! A fake herdr-core for tests: an in-memory set of terminals that answers the
//! `hc` commands this binary runs, so a test exercises the real adapter
//! ([`crate::herdr`]) instead of a canned CLI.
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
        "name": term.name,
        "cwd": term.cwd,
        "state": term.state,
        "status": term.status,
        "turn": term.turn,
    })
}

fn full(term: &Terminal) -> serde_json::Value {
    serde_json::json!({
        "address": term.address,
        "name": term.name,
        "labels": term.labels,
        "cwd": term.cwd,
        "last_cwd": term.cwd,
        "state": term.state,
        "status": term.status.as_ref().map(|state| serde_json::json!({"state": state})),
        "turn": term.turn,
        "reported": term.status.is_some(),
        "argv": term.argv,
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
        };
        self.terminals.borrow_mut().push(term.clone());
        term
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

    pub fn labels(&self, name: &str) -> std::collections::BTreeMap<String, String> {
        self.terminals.borrow().iter().find(|t| t.name == name || t.address == name).map(|t| t.labels.clone()).unwrap_or_default()
    }

    fn find(&self, id: &str) -> Option<Terminal> {
        self.terminals.borrow().iter().find(|t| t.name == id || t.address == id).cloned()
    }

    fn find_mut(&self, id: &str) -> Option<std::cell::RefMut<'_, Terminal>> {
        let index = self.terminals.borrow().iter().position(|t| t.name == id || t.address == id)?;
        let terminals = self.terminals.borrow_mut();
        Some(std::cell::RefMut::map(terminals, move |all| &mut all[index]))
    }

    /// The one call path: `hc [--on M] --json <command> <args...>`.
    fn dispatch(&self, args: &[String]) -> Result<Output> {
        let mut args: Vec<&str> = args.iter().map(String::as_str).collect();
        let is_machine = args.first() == Some(&"--on");
        if is_machine {
            args.drain(..2);
        }
        let _ = is_machine;
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
                ok(serde_json::json!({"terminals": live, "hidden_finished": 0}).to_string())
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
            ("kill", [id]) | ("rm", [id]) => match self.find_mut(id) {
                Some(mut term) => {
                    term.state = "finished".into();
                    term.status = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::Herdr;

    fn core() -> (tempfile::TempDir, FakeCore) {
        (tempfile::tempdir().unwrap(), FakeCore::new())
    }

    #[test]
    fn creating_a_space_and_a_tab_yields_distinct_ids_and_live_panes() {
        let (home, fake) = core();
        let herdr = Herdr::new("hc", "", &fake);
        let dir = home.path();
        let first = herdr.workspace_create(dir, "Demo", false).unwrap();
        let second = herdr.tab_create(&first.workspace_id, dir, "thread", false).unwrap();
        assert_ne!(first.pane_id, second.pane_id, "each terminal is its own pane");
        assert_ne!(first.tab_id, second.tab_id, "a tab is its own terminal");
        assert_eq!(herdr.pane_list().unwrap().len(), 2);
        assert_eq!(herdr.workspace_list().unwrap().len(), 1, "both are in one Space");
    }

    #[test]
    fn labels_are_the_tokens_the_lane_resolver_reads() {
        let (_home, fake) = core();
        let herdr = Herdr::new("hc", "", &fake);
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
        let herdr = Herdr::new("hc", "", &fake);
        fake.terminal("hp-demo-1", "/tmp/demo", &[("hp_agent", "hpc-demo")]);
        fake.set_status("hp-demo-1", "blocked");
        let error = herdr.agent_prompt("hp-demo-1", "hello").unwrap_err();
        assert_eq!(error.code, "agent_blocked");
        assert!(crate::herdr::refused_before_typing(&error));
    }

    #[test]
    fn starting_an_agent_types_the_wrapper_and_waits_for_its_first_word() {
        let (_home, fake) = core();
        let herdr = Herdr::new("hc", "", &fake);
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
        let herdr = Herdr::new("hc", "", &fake);
        fake.terminal("hp-demo-1", "/tmp/demo", &[]);
        fake.set_screen("hp-demo-1", "hello\n❯ \n");
        assert_eq!(herdr.agent_read("hp-demo-1", None).unwrap(), "hello\n❯ \n");
    }
}
