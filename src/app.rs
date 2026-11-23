//! Talking to the Herdr desktop app, the window over herdr-core.
//!
//! A terminal started with `hc` stays out of that window; the app shows the ones
//! it made, or the ones carrying `herdr.space`. The supported way in is the
//! app's own commands: `herdr-app do space.new` and `herdr-app do tab.new`
//! answer with the ids the app gave the space, the tab and the terminal, and a
//! machine without the app falls back to plain `hc` (reached with `hc attach`).

use std::cell::RefCell;
use std::time::Duration;

use crate::runner::{Cmd, Runner};

// The window's view, held for the length of a ticker pass.
//
// `herdr-app status` and `herdr-app ls` answer in a few milliseconds, but a tick
// asked for them once or twice per coordinator — and every rename path asked
// again to confirm. A pass answers each once and every reader in the pass reads
// that (`begin`); anything that renames or closes forgets it, so the confirming
// read after a rename still goes to the window. Outside a pass nothing is held:
// a one-shot command always sees the window as it is.
thread_local! {
    static HELD: RefCell<Option<Held>> = const { RefCell::new(None) };
}

#[derive(Default)]
struct Held {
    running: Option<bool>,
    ls: Option<Option<serde_json::Value>>,
}

/// Starts a pass: nothing read yet, each answer held until [`end`].
pub fn begin() {
    HELD.with(|held| *held.borrow_mut() = Some(Held::default()));
}

/// Ends it: the next read asks the window.
pub fn end() {
    HELD.with(|held| *held.borrow_mut() = None);
}

/// Drops the held view (a Space or tab was renamed, opened or closed).
pub fn forget() {
    HELD.with(|held| {
        if let Some(held) = held.borrow_mut().as_mut() {
            held.ls = None;
        }
    });
}

/// One answer per pass, fetched by `fetch` the first time it is asked for.
fn held<T>(pick: impl Fn(&mut Held) -> &mut Option<T>, fetch: impl FnOnce() -> T) -> T
where
    T: Clone,
{
    HELD.with(|cell| {
        let mut held = cell.borrow_mut();
        let Some(held) = held.as_mut() else {
            return fetch();
        };
        let slot = pick(held);
        slot.get_or_insert_with(fetch).clone()
    })
}

/// `HERDR_APP_BIN`, else `herdr-app`.
pub fn bin() -> String {
    std::env::var("HERDR_APP_BIN").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "herdr-app".to_string())
}

/// The app answers only while its window is open; `status` is that check.
pub fn running(runner: &dyn Runner) -> bool {
    held(|held| &mut held.running, || {
        runner
            .run(&Cmd::new(bin(), Duration::from_secs(5)).arg("status"))
            .map(|out| out.success())
            .unwrap_or(false)
    })
}

/// The ids a space or a tab came with.
#[derive(Debug, Clone, PartialEq)]
pub struct Placed {
    pub space: String,
    pub tab: String,
    /// The terminal core runs for it (`t32`), which is also a pane id here.
    pub terminal: String,
}

/// `herdr-app do <command> <args...>`, its JSON reply.
fn call(runner: &dyn Runner, command: &str, args: &[String]) -> Option<serde_json::Value> {
    call_strict(runner, command, args).ok()
}

/// [`call`], with the app's own words when it refuses: a rename that did not
/// happen is then a line a person reads, not a silent `false` (t-0051).
fn call_strict(runner: &dyn Runner, command: &str, args: &[String]) -> Result<serde_json::Value, String> {
    let mut cmd = Cmd::new(bin(), Duration::from_secs(20)).args(["do", command]);
    cmd = cmd.args(args.iter().cloned());
    let out = runner.run(&cmd).map_err(|error| format!("{error:#}"))?;
    let text = out.stdout.trim();
    let reply: serde_json::Value = serde_json::from_str(text).map_err(|_| match out.success() {
        true => format!("{command}: {text}"),
        false => format!("{command}: {}", out.error_text()),
    })?;
    match reply.get("error") {
        Some(error) => Err(error.as_str().map(str::to_string).unwrap_or_else(|| error.to_string())),
        None if out.success() => Ok(reply),
        None => Err(format!("{command}: {text}")),
    }
}

fn placed(reply: &serde_json::Value) -> Option<Placed> {
    Some(Placed {
        space: reply["space"].as_str()?.to_string(),
        tab: reply["tab"].as_str()?.to_string(),
        terminal: reply["terminal"].as_str()?.to_string(),
    })
}

/// A Space for a project: named after it, starting in its folder, not stealing
/// focus.
pub fn space_new(runner: &dyn Runner, name: &str, folder: &str) -> Option<Placed> {
    // A Space that did not exist a moment ago: the held view is wrong about
    // the list now.
    let args = vec![
        "--name".to_string(),
        name.to_string(),
        "--folder".to_string(),
        folder.to_string(),
        "--focus".to_string(),
        "false".to_string(),
    ];
    let placed = call(runner, "space.new", &args);
    forget();
    placed_from(placed.as_ref())
}

fn placed_from(reply: Option<&serde_json::Value>) -> Option<Placed> {
    placed(reply?)
}

/// Ends every terminal of a Space, as `hp` means it.
pub fn space_close(runner: &dyn Runner, space: &str) -> bool {
    let args = vec!["--id".to_string(), space.to_string(), "--confirmed".to_string(), "true".to_string()];
    let closed = call(runner, "space.close", &args).is_some();
    forget();
    closed
}

/// Every space, tab and terminal the window shows (`herdr-app ls`).
pub fn ls(runner: &dyn Runner) -> Option<serde_json::Value> {
    held(|held| &mut held.ls, || ls_now(runner))
}

fn ls_now(runner: &dyn Runner) -> Option<serde_json::Value> {
    let out = runner.run(&Cmd::new(bin(), Duration::from_secs(10)).arg("ls")).ok()?;
    if !out.success() {
        return None;
    }
    serde_json::from_str(out.stdout.trim()).ok()
}

/// The tab the window put a terminal in: the app makes one when a terminal
/// carries `herdr.space`, and its name is ours to set.
pub fn tab_of(runner: &dyn Runner, terminal: &str) -> Option<String> {
    let spaces = ls(runner)?;
    for space in spaces["spaces"].as_array()? {
        for tab in space["tabs"].as_array()? {
            if tab["terminals"].as_array()?.iter().any(|t| t["terminal"] == terminal) {
                return tab["id"].as_str().map(str::to_string);
            }
        }
    }
    None
}

/// A fresh space or tab is not in `ls` yet: the app materialises it a moment
/// after the reply, so both the close and the rename retry briefly.
pub fn retry(mut attempt: impl FnMut() -> bool) -> bool {
    for _ in 0..30 {
        if attempt() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(60));
    }
    false
}

pub fn tab_rename(runner: &dyn Runner, tab: &str, name: &str) -> bool {
    tab_name(runner, tab, name).is_ok()
}

/// `tab.rename`, with the app's own words when it refuses.
pub fn tab_name(runner: &dyn Runner, tab: &str, name: &str) -> Result<(), String> {
    let args = vec!["--id".to_string(), tab.to_string(), "--name".to_string(), name.to_string()];
    let renamed = call_strict(runner, "tab.rename", &args).map(|_| ());
    forget();
    renamed
}

/// Names a Space: the name a person reads above its tabs.
pub fn space_rename(runner: &dyn Runner, space: &str, name: &str) -> Result<(), String> {
    let args = vec!["--id".to_string(), space.to_string(), "--name".to_string(), name.to_string()];
    let renamed = call_strict(runner, "space.rename", &args).map(|_| ());
    forget();
    renamed
}

/// What the window shows for a Space and the terminal in it: the Space's own
/// name, the tab the terminal sits in, and that tab's name. `None` while the
/// window does not know the Space, or does not show that terminal yet — a Space
/// hp made in the app is materialised a moment after the reply.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub space: String,
    pub tab: String,
    pub tab_name: String,
}

/// [`shown_in`] over the window's current view.
pub fn shown(runner: &dyn Runner, space: &str, terminal: &str) -> Option<Shown> {
    shown_in(&ls(runner)?, space, terminal)
}

/// The name the window draws for a Space, as `herdr-app ls` has it.
pub fn space_name(runner: &dyn Runner, space: &str) -> Option<String> {
    space_name_in(&ls(runner)?, space)
}

/// [`space_name`] over a view already read.
pub fn space_name_in(view: &serde_json::Value, space: &str) -> Option<String> {
    let found = view["spaces"].as_array()?.iter().find(|s| s["id"] == space)?;
    Some(found["name"].as_str().unwrap_or_default().to_string())
}

/// [`shown`] over a view already read (`herdr-app ls`).
pub fn shown_in(view: &serde_json::Value, space: &str, terminal: &str) -> Option<Shown> {
    let space = view["spaces"].as_array()?.iter().find(|s| s["id"] == space)?;
    let tab = space["tabs"].as_array()?.iter().find(|tab| {
        tab["terminals"]
            .as_array()
            .is_some_and(|terminals| terminals.iter().any(|t| t["terminal"] == terminal))
    })?;
    Some(Shown {
        space: space["name"].as_str().unwrap_or_default().to_string(),
        tab: tab["id"].as_str()?.to_string(),
        tab_name: tab["name"].as_str().unwrap_or_default().to_string(),
    })
}

pub fn tab_close(runner: &dyn Runner, tab: &str) -> bool {
    let args = vec!["--id".to_string(), tab.to_string()];
    let closed = call(runner, "tab.close", &args).is_some();
    forget();
    closed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_placed_reply_names_the_space_tab_and_terminal() {
        let reply = serde_json::json!({
            "space": "spf737521ee96965d6e315bb526093f20b",
            "tab": "tbe937c8bd031987e8026dadab938edeb0",
            "terminal": "t32"
        });
        let ids = placed(&reply).unwrap();
        assert_eq!(ids.space, "spf737521ee96965d6e315bb526093f20b");
        assert_eq!(ids.terminal, "t32");
        assert!(placed(&serde_json::json!({"error": "no space shown"})).is_none());
    }
}
