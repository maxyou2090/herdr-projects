//! Built-in progress self-reporting: the agent runs `report` in its own pane,
//! one JSON record per pane under `<root>/.progress/` feeds the ticker. The
//! binding is the terminal id core hands every pane shell; there is nothing to
//! mint and no daemon. Outside a core terminal both commands do nothing.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::herdr::Herdr;
use crate::paths::{Ctx, Env};
use crate::runner::Runner;

pub const ACTIVITY_COLUMNS: usize = 40;
pub const ACTIVITY_TTL_MS: u64 = 300_000;
pub const WAITING: &str = "Waiting for you";
pub const DONE: &str = "Done";

/// One pane's self-report.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Record {
    pub socket: String,
    pub pane_id: String,
    /// Pane ids restart from `w1` after a server restart; the terminal id tells
    /// a stale record from a live pane that reused the id.
    pub terminal_id: String,
    pub activity: String,
    /// The agent said its whole task is finished.
    pub done: bool,
    /// Unix seconds of the last `report`; 0 when none since the session started.
    pub reported_at: i64,
}

impl Record {
    pub fn waiting(&self) -> bool {
        self.reported_at > 0 && self.activity == WAITING
    }

    pub fn done(&self) -> bool {
        self.reported_at > 0 && self.done
    }

    pub fn reported(&self) -> bool {
        self.reported_at > 0
    }
}

pub fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

pub fn dir(root: &Path) -> PathBuf {
    root.join(".progress")
}

/// `<pane id>-<short hash of the socket path>.json`: pane ids repeat across sessions.
pub fn path(root: &Path, socket: &str, pane_id: &str) -> PathBuf {
    let hash = &crate::thread::sha256_hex(socket.as_bytes())[..8];
    dir(root).join(format!("{}-{hash}.json", pane_id.replace(':', "_")))
}

pub fn load(root: &Path, socket: &str, pane_id: &str) -> Option<Record> {
    crate::project::read_json(&path(root, socket, pane_id))
}

pub fn save(root: &Path, record: &Record) -> Result<()> {
    std::fs::create_dir_all(dir(root))?;
    crate::project::write_json(&path(root, &record.socket, &record.pane_id), record)
}

pub fn remove(root: &Path, socket: &str, pane_id: &str) {
    let _ = std::fs::remove_file(path(root, socket, pane_id));
}

/// Every record under the root, for the ticker's cleanup.
pub fn all(root: &Path) -> Vec<Record> {
    let Ok(entries) = std::fs::read_dir(dir(root)) else {
        return Vec::new();
    };
    entries.flatten().filter_map(|e| crate::project::read_json::<Record>(&e.path())).collect()
}

/// At most 40 columns, no control or bidi characters, trimmed. 100% is "Done".
pub fn clean(input: &str, columns: usize) -> String {
    let mut width = 0;
    input
        .chars()
        .filter(|c| !c.is_control() && !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .take(120)
        .take_while(|c| {
            // East Asian wide characters take two columns; everything else one.
            width += if ('\u{1100}'..='\u{115f}').contains(c) || ('\u{2e80}'..='\u{a4cf}').contains(c) || ('\u{ac00}'..='\u{d7a3}').contains(c) || ('\u{f900}'..='\u{faff}').contains(c) || ('\u{fe30}'..='\u{fe4f}').contains(c) || ('\u{ff00}'..='\u{ff60}').contains(c) || ('\u{ffe0}'..='\u{ffe6}').contains(c) || ('\u{1f300}'..='\u{1f64f}').contains(c) || ('\u{1f900}'..='\u{1f9ff}').contains(c) || ('\u{20000}'..='\u{3fffd}').contains(c) { 2 } else { 1 };
            width <= columns
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// The calling pane, when this process runs inside a core terminal: its
/// terminal address (`HC_TERMINAL_ID`) and the pane id core shows for it (a moved
/// pane keeps its launch-time name), plus the session. `None` outside a core
/// terminal.
pub struct Current {
    pub socket: String,
    pub pane_id: String,
    pub terminal_id: String,
}

pub fn current(env: &Env, runner: &dyn Runner) -> Option<Current> {
    // core names the terminal in `HC_TERMINAL_ID`; `HERDR_*` was Herdr 1's.
    let address = env.var("HC_TERMINAL_ID")?.to_string();
    let socket = env.var("HC_SESSION").unwrap_or("").to_string();
    let herdr = Herdr::new(env.herdr_bin(), &socket, runner);
    let pane = herdr.pane_get(&address).ok()?;
    Some(Current { socket, pane_id: pane.pane_id.clone(), terminal_id: pane.terminal_id.clone() })
}

/// `report [--done] --activity "..."`, run by the agent in its own terminal.
/// Writes the record; the ticker reads it for the thread's group while it is
/// fresh. There is no percentage: nothing shows one any more, and "waiting
/// for you" is the activity [`WAITING`].
pub fn report(ctx: &Ctx, done: bool, activity: &str) -> Result<()> {
    let Some(pane) = current(ctx.env, ctx.runner) else {
        println!("not inside a core terminal; nothing reported");
        return Ok(());
    };
    let activity = if done { DONE.to_string() } else { clean(activity, ACTIVITY_COLUMNS) };
    if activity.is_empty() {
        bail!("`--activity` is empty");
    }
    let mut record = load(&ctx.root, &pane.socket, &pane.pane_id).unwrap_or_default();
    record.socket = pane.socket.clone();
    record.pane_id = pane.pane_id.clone();
    record.terminal_id = pane.terminal_id.clone();
    record.activity = activity.clone();
    record.done = done;
    record.reported_at = now();
    save(&ctx.root, &record)?;
    println!("recorded: {activity}");
    Ok(())
}

/// `progress [--pane ID]`: the record for the calling pane, or the given one.
pub fn print(ctx: &Ctx, pane: Option<&str>) -> Result<()> {
    let (socket, pane_id) = match pane {
        Some(id) => (ctx.env.var("HC_SESSION").unwrap_or("").to_string(), id.to_string()),
        None => match current(ctx.env, ctx.runner) {
            Some(c) => (c.socket, c.pane_id),
            None => bail!("not inside a core terminal; pass --pane ID"),
        },
    };
    match load(&ctx.root, &socket, &pane_id) {
        Some(record) => println!("{}", serde_json::to_string_pretty(&record)?),
        None => println!("null"),
    }
    Ok(())
}

/// The body of the progress instructions: every agent reads it in its thread
/// brief or the coordinator skill, so any harness that can run a shell
/// command reports.
pub fn guidance(prefix: &str, pane_id: Option<&str>) -> String {
    let pane = pane_id.map(|id| format!("this terminal ({id})")).unwrap_or_else(|| "this terminal".into());
    format!(
        "Report what the user's whole current task is doing through `{prefix} report`. Reporting failures must never stop the actual work: give one short diagnostic and continue, without retries.\n\n\
         Give a two-to-four-word activity, such as `Reading code`, `Testing changes` or `Waiting for you`:\n\n\
         `{prefix} report --activity 'Reading code'`\n\n\
         Report at the start of new work, after meaningful milestones, when the activity changes and at blockers. Report `--activity 'Waiting for you'` whenever you stop to ask the user something. Add `--done` only when the entire requested outcome and its checks are finished; if more work is requested afterwards, report again without it.\n\n\
         Only the top-level agent in {pane} reports; subagents and helpers do not. Await each report command; do not run it in the background."
    )
}

/// The record's contribution to a thread's live state, or nothing when the
/// record is missing or describes an earlier pane with the same id.
pub fn self_report(root: &Path, socket: &str, pane_id: &str, terminal_id: &str) -> Option<Record> {
    let record = load(root, socket, pane_id)?;
    if !terminal_id.is_empty() && !record.terminal_id.is_empty() && record.terminal_id != terminal_id {
        return None;
    }
    record.reported().then_some(record)
}

/// Drops records whose pane is no longer listed in the session they belong to.
/// Only records of `session` are judged: other sessions' records are theirs.
pub fn prune(root: &Path, session: &str, live_pane_ids: &[String]) {
    for record in all(root) {
        if record.socket == session && !live_pane_ids.contains(&record.pane_id) {
            remove(root, &record.socket, &record.pane_id);
        }
    }
}
