//! `hp-agent <command> [args...]`: run a coding agent and tell herdr-core what
//! it is doing over OSC 7501, the only status channel herdr-core reads.
//!
//! herdr-core draws nothing and keeps no agent registry: a program's status is
//! whatever it prints as `ESC ] 7501 ; state=... ST`. herdr-core itself reports
//! idle, working, done, blocked and error with optional kind, progress, app,
//! title and msg.
//!
//! Two kinds of harness need two kinds of help:
//!
//! - **A harness that reports its own status** (pi, the Program Status
//!   Protocol) is left alone. It reports `working` when a run starts, `blocked`
//!   when one of its own dialogs waits for a person, `done` when a run ends and
//!   `idle` when one is cancelled, so a second writer here would only make the
//!   terminal's state flap.
//! - **A harness that prints nothing** would read `idle` for the whole turn.
//!   For those this wrapper polls its own pane (`hc read`) and reports the
//!   states its screen proves (`agent_state`): `blocked` on a trust or review
//!   screen, `working` while the screen is changing, `idle` once an empty input
//!   box has sat still. That is a read of the screen core already keeps, not a
//!   PTY inside this process: the agent keeps the terminal it was given, its
//!   stdin, its window size and its signals, so nothing about its interactive
//!   experience changes. `docs/herdr-core-port.md` §8.1 records why the relay
//!   was rejected.
//!
//! Either way the wrapper's own words are unchanged: `idle` at start, `done` or
//! `error` at exit. `HP_AGENT_WATCH=0` turns the polling off for a debugging
//! session.

#[path = "agent_state.rs"]
mod agent_state;
#[path = "prompt_box.rs"]
mod prompt_box;
#[path = "trust_screen.rs"]
mod trust_screen;

use std::io::Write as _;
use std::process::{Child, Command, ExitCode, ExitStatus, Stdio};
use std::time::Duration;

use agent_state::{State, Watcher};

/// Kinds that report their own 7501 status, so this wrapper must not speak for
/// them. pi documents its Program Status Protocol in its own manual
/// ("Program status": working, blocked, done, error, idle); a harness that
/// starts reporting natively belongs in this list, and one that stops belongs
/// out of it.
const SELF_REPORTING: &[&str] = &["pi"];

/// How long one `hc read` may take before it is killed. A core that stops
/// answering must not stop this wrapper from noticing that its agent exited.
const READ_TIMEOUT_MS: u64 = 2_000;

/// Failed reads in a row (one poll apart) after which the wrapper stops
/// watching and only waits: half a minute of "cannot see this pane" is a pane
/// that stays unseen for this turn.
const MISSES_BEFORE_GIVING_UP: u32 = 15;

/// Writes one OSC 7501 record straight to the terminal, unbuffered and in a
/// single write: the agent draws with the same stream, so a record split across
/// writes could split one of its escape sequences.
fn report(fields: &str) {
    let line = format!("\x1b]7501;{fields}\x1b\\");
    let mut out = std::io::stdout();
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
}

fn state_report(state: State, app: &str) {
    report(&format!("state={}:app={app}", state.name()));
}

/// The kind to watch for this command, or `None` when the wrapper should only
/// bracket the agent's life: a kind whose screen this binary cannot read, a
/// kind that reports itself, or `HP_AGENT_WATCH=0`.
fn watch_kind(command: &str) -> Option<String> {
    let kind = std::path::Path::new(command).file_name()?.to_string_lossy().into_owned();
    if !prompt_box::knows(&kind) || SELF_REPORTING.contains(&kind.as_str()) {
        return None;
    }
    if std::env::var("HP_AGENT_WATCH").is_ok_and(|value| value == "0") {
        return None;
    }
    Some(kind)
}

/// What core shows in terminal `terminal` now, or `None` when it cannot be
/// read: no `hc` on `PATH`, no such terminal, a core that is not running,
/// output that is not the JSON this asks for. A read that fails is not news, so
/// the wrapper simply says nothing.
fn screen(terminal: &str) -> Option<String> {
    let mut child = Command::new("hc")
        .args(["--json", "read", terminal])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut waited = 0;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(_) => return None,
        }
        if waited >= READ_TIMEOUT_MS {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
        waited += 50;
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    screen_text(&out.stdout)
}

/// The `text` of `hc --json read`'s answer — the pane's screen, styling
/// dropped, which is what `prompt_box` and `trust_screen` read.
fn screen_text(stdout: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    Some(value.get("text")?.as_str()?.to_string())
}

/// Runs the agent to its end, reporting the states its screen proves while it
/// runs. Falls back to a plain wait when there is no terminal to read or the
/// core cannot be reached.
fn watch(child: &mut Child, kind: &str, app: &str) -> std::io::Result<ExitStatus> {
    let Some(terminal) = std::env::var("HC_TERMINAL_ID").ok().filter(|id| !id.is_empty()) else {
        return child.wait();
    };
    let mut watcher = Watcher::new(kind);
    // Reads that keep failing (no `hc`, no core) mean this pane cannot be seen
    // at all: after a while, stop asking and just wait for the agent.
    let mut misses = 0;
    loop {
        // Sleep in slices so an agent that exits is reported at once, not up to
        // a whole poll later.
        for _ in 0..(agent_state::POLL_SECS * 10) {
            std::thread::sleep(Duration::from_millis(100));
            if let Some(status) = child.try_wait()? {
                return Ok(status);
            }
        }
        match screen(&terminal) {
            Some(seen) => {
                misses = 0;
                if let Some(state) = watcher.observe(&seen) {
                    state_report(state, app);
                }
            }
            None => {
                misses += 1;
                if misses >= MISSES_BEFORE_GIVING_UP {
                    return child.wait();
                }
            }
        }
    }
}

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // `hp-agent -- cmd` is what a shell writes; `hp-agent cmd` is what a
    // template writes. Both mean the same thing.
    if args.first().map(String::as_str) == Some("--") {
        args.remove(0);
    }
    let Some(command) = args.first().cloned() else {
        eprintln!("usage: hp-agent <command> [args...]");
        return ExitCode::from(2);
    };

    let app = std::path::Path::new(&command)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| command.clone());
    report(&format!("state=idle:app={app}"));

    let mut child = match Command::new(&command).args(&args[1..]).spawn() {
        Ok(child) => child,
        Err(error) => {
            report(&format!("state=error:app={app}:msg={}", error.kind()));
            eprintln!("hp-agent: cannot run `{command}`: {error}");
            return ExitCode::from(127);
        }
    };
    let status = match watch_kind(&command) {
        Some(kind) => watch(&mut child, &kind, &app),
        None => child.wait(),
    };

    match status {
        Ok(status) if status.success() => {
            report(&format!("state=done:app={app}"));
            ExitCode::SUCCESS
        }
        Ok(status) => {
            let code = status.code().unwrap_or(1);
            report(&format!("state=error:app={app}:msg=exit {code}"));
            ExitCode::from(u8::try_from(code).unwrap_or(1))
        }
        Err(error) => {
            report(&format!("state=error:app={app}:msg={}", error.kind()));
            eprintln!("hp-agent: lost `{command}`: {error}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_kind_whose_screen_says_something_is_watched() {
        assert_eq!(watch_kind("claude").as_deref(), Some("claude"));
        assert_eq!(watch_kind("/usr/local/bin/codex").as_deref(), Some("codex"));
        // pi reports its own status: two writers would flap.
        assert_eq!(watch_kind("pi"), None);
        // A kind this binary cannot read a screen for, and not an agent at all.
        assert_eq!(watch_kind("kimi"), None);
        assert_eq!(watch_kind("./run-my-agent"), None);
    }

    #[test]
    fn a_record_is_one_well_formed_7501_write() {
        // The shape core parses: `ESC ] 7501 ; fields ST`, app named.
        let line = format!("\x1b]7501;state={}:app={}\x1b\\", State::Working.name(), "claude");
        assert_eq!(line, "\u{1b}]7501;state=working:app=claude\u{1b}\\");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn a_screen_is_the_text_of_cores_answer_and_nothing_else_counts() {
        let answer = r#"{"complete":true,"how":"screen","text":"❯ \n"}"#.as_bytes();
        assert_eq!(screen_text(answer).as_deref(), Some("❯ \n"));
        // A core that is gone, an error, a shape this does not expect: no news.
        assert_eq!(screen_text(b""), None);
        assert_eq!(screen_text(b"no such terminal"), None);
        assert_eq!(screen_text(br#"{"error":{"kind":"not_found"}}"#), None);
        assert_eq!(screen_text(br#"{"text":null}"#), None);
    }
}
