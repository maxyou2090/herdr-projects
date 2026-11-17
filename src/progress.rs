//! Built-in progress self-reporting: the agent runs `report` in its own pane,
//! the harness hooks (`hook`) inject the instructions and a reminder, one JSON
//! record per pane under `<root>/.progress/` feeds the ticker. The binding is
//! the pane id Herdr hands every pane shell; there is nothing to mint and no
//! daemon. Outside a Herdr pane both commands do nothing.

//! herdr-core agents report through `hp report`; Herdr 1's per-harness hook
//! normalization is gone, and what is left of that path is dead until pruned.
#![allow(dead_code)]
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::herdr::{CALL_TIMEOUT, Herdr};
use crate::paths::{Ctx, Env};
use crate::runner::Runner;

pub const ACTIVITY_COLUMNS: usize = 40;
pub const ACTIVITY_TTL_MS: u64 = 300_000;
/// Reminders go out at most about once a minute.
pub const REMIND_SECS: i64 = 60;
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
    pub agent: String,
    pub activity: String,
    /// The agent said its whole task is finished.
    pub done: bool,
    /// Unix seconds of the last `report`; 0 when none since the session started.
    pub reported_at: i64,
    /// Unix seconds of the last reminder the hook injected.
    pub reminded_at: i64,
    pub session_started_at: i64,
    /// The harness's own session id from the last SessionStart: Copilot CLI
    /// runs a subagent's hooks under another id with no other marker.
    pub session_id: String,
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

/// A report recent enough for its activity to show in the sidebar: silence
/// clears it after `ACTIVITY_TTL_MS`.
pub fn fresh(record: &Record) -> bool {
    record.reported() && now() - record.reported_at < (ACTIVITY_TTL_MS / 1000) as i64
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

/// The calling pane, when this process runs inside a Herdr pane: the id
/// Herdr shows for it (a moved pane keeps its launch-time `HERDR_PANE_ID`),
/// its terminal id and the agent Herdr detects. `None` outside Herdr.
pub struct Current {
    pub socket: String,
    pub pane_id: String,
    pub terminal_id: String,
    pub agent: String,
    /// The whole `pane current` reply: workspace, tab and both directories.
    pub pane: crate::herdr::Pane,
}

pub fn current(env: &Env, runner: &dyn Runner) -> Option<Current> {
    current_within(env, runner, CALL_TIMEOUT)
}

/// The hook uses a short timeout: a slow server must not stall every tool call.
pub fn current_within(env: &Env, runner: &dyn Runner, _timeout: std::time::Duration) -> Option<Current> {
    // core names the terminal in `HC_TERMINAL_ID`; `HERDR_*` was Herdr 1's.
    let address = env.var("HC_TERMINAL_ID")?.to_string();
    let socket = env.var("HC_SESSION").unwrap_or("").to_string();
    let herdr = Herdr::new(env.herdr_bin(), &socket, runner);
    let pane = herdr.pane_get(&address).ok()?;
    let agent = herdr.agent_kind(&address).unwrap_or_default();
    Some(Current {
        socket,
        pane_id: pane.pane_id.clone(),
        terminal_id: pane.terminal_id.clone(),
        agent,
        pane,
    })
}

/// True when the calling pane is one of a project's coordinators or an open
/// local thread on the same server: the only panes the hooks speak in.
///
/// Every tool call in every pane on the machine runs the hook, so files are
/// read first and Herdr is asked only when the pane could be a project's (its
/// directory is a project home, or an open thread records its pane id). Fails
/// closed: when Herdr cannot list the agents, the hook stays silent.
fn managed(ctx: &Ctx, current: &Current) -> bool {
    use crate::{coordinator, project, thread};
    let pane = &current.pane;
    // Ids are only unique within one server: a project whose coordinator was
    // recorded on another socket cannot own this pane. A project never
    // opened (no record) may still be coordinated by an agent started by hand.
    let projects: Vec<_> = project::list_slugs(&ctx.root)
        .iter()
        .filter_map(|slug| project::Project::load(&ctx.root, slug).ok())
        .map(|project| {
            let record = project.coordinator();
            (project, record)
        })
        .filter(|(_, record)| record.as_ref().is_none_or(|r| r.socket == current.socket))
        .collect();
    let in_home = |project: &project::Project| {
        let home = project.canonical_dir().to_string_lossy().into_owned();
        crate::herdr::own_dir(&pane.cwd, &pane.foreground_cwd) == home
    };
    // A thread record carries no socket of its own: only the project's
    // coordinator record ties its pane ids to this server.
    let threads = |project: &project::Project, record: &Option<project::Coordinator>| -> Vec<thread::Thread> {
        if record.is_none() {
            return Vec::new();
        }
        thread::list(project).into_iter().filter(|t| !t.is_remote() && t.status != thread::Status::Resolved && thread::pane_matches(t, pane)).collect()
    };
    let candidates: Vec<_> = projects.iter().filter(|(project, record)| in_home(project) || !threads(project, record).is_empty()).collect();
    if candidates.is_empty() {
        return false;
    }
    if candidates.iter().any(|(_, record)| record.as_ref().is_some_and(|r| coordinator::pane_matches(r, pane))) {
        return true;
    }
    let herdr = Herdr::new(ctx.env.herdr_bin(), &current.socket, ctx.runner);
    let Ok(agents) = herdr.agent_list() else {
        return false;
    };
    let mut agents: Vec<_> = agents.into_iter().filter(|a| a.pane_id == pane.pane_id && a.workspace_id == pane.workspace_id && a.tab_id == pane.tab_id && a.terminal_id == pane.terminal_id).collect();
    if agents.is_empty() && current.agent.is_empty() {
        // At SessionStart Herdr may not have detected the agent yet; the hook
        // itself shows one is starting here, so stand in the pane for it.
        agents.push(crate::herdr::Agent {
            pane_id: pane.pane_id.clone(),
            workspace_id: pane.workspace_id.clone(),
            tab_id: pane.tab_id.clone(),
            cwd: pane.cwd.clone(),
            foreground_cwd: pane.foreground_cwd.clone(),
            terminal_id: pane.terminal_id.clone(),
            ..Default::default()
        });
    }
    candidates.iter().any(|(project, record)| {
        // A coordinator `open --new` or the user started that the ticker has
        // not recorded yet: the ticker's own discovery rule.
        coordinator::found(project, &current.socket, "", &agents).is_some_and(|r| coordinator::pane_matches(&r, pane))
            || threads(project, record).iter().any(|t| agents.iter().any(|a| thread::agent_matches(t, a)))
    })
}

/// `report [--done] --activity "..."`, run by the agent in its own terminal.
/// Writes the record; the ticker reads it for the thread's group while it is
/// [`fresh`]. There is no percentage: nothing shows one any more, and "waiting
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
    record.agent = pane.agent.clone();
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
        Some(id) => (ctx.env.var("HERDR_SOCKET_PATH").unwrap_or("").to_string(), id.to_string()),
        None => match current(ctx.env, ctx.runner) {
            Some(c) => (c.socket, c.pane_id),
            None => bail!("not inside a Herdr pane; pass --pane ID"),
        },
    };
    match load(&ctx.root, &socket, &pane_id) {
        Some(record) => println!("{}", serde_json::to_string_pretty(&record)?),
        None => println!("null"),
    }
    Ok(())
}

// ---------------------------------------------------------------- hooks

/// The instructions the SessionStart hook injects, with the exact command.
pub fn instructions(prefix: &str, pane_id: &str) -> String {
    format!("# Progress (herdr-projects)\n\n{}", guidance(prefix, Some(pane_id)))
}

/// The body of the progress instructions. Hooks inject it at session start;
/// every other agent reads it in its thread brief or the coordinator skill,
/// so any harness that can run a shell command reports.
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

pub fn reminder(prefix: &str) -> String {
    format!("Progress check-in is due if this is a natural boundary: `{prefix} report --percent N --activity '...'`. Reassess the current task; do not invent progress. Report `Waiting for you` before a question to the user.")
}

/// Events the reporter reacts to: the top-level agent's own SessionStart,
/// UserPromptSubmit and PostToolUse, never a subagent's, and never the
/// PostToolUse of the `report` call itself.
pub fn eligible(event: &serde_json::Value) -> bool {
    if ["agent_id", "subagent_id", "agent_transcript_path"].iter().any(|key| event.get(key).is_some_and(|v| !v.is_null())) {
        return false;
    }
    if event["transcript_path"].as_str().is_some_and(|p| p.contains("/subagents/")) {
        return false;
    }
    let name = event["hook_event_name"].as_str().unwrap_or("");
    if !matches!(name, "SessionStart" | "PostToolUse" | "UserPromptSubmit") {
        return false;
    }
    if name == "PostToolUse" && event["tool_input"].to_string().contains("herdr-projects") && event["tool_input"].to_string().contains(" report ") {
        return false;
    }
    true
}

/// What the hook answers for one event: the text to inject, and whether the
/// record changed. Pure, so it is testable without a pane.
pub fn respond(record: &mut Record, kind: &str, prefix: &str, now: i64) -> Option<String> {
    match kind {
        "SessionStart" => {
            // A new session in this pane: the old report no longer describes it.
            let keep = (record.socket.clone(), record.pane_id.clone(), record.terminal_id.clone(), record.agent.clone());
            *record = Record { socket: keep.0, pane_id: keep.1, terminal_id: keep.2, agent: keep.3, session_started_at: now, reminded_at: now, ..Record::default() };
            Some(instructions(prefix, &record.pane_id))
        }
        "UserPromptSubmit" => {
            record.reminded_at = now;
            // The user answered: an old "Waiting for you" no longer holds.
            if record.activity == WAITING {
                record.activity.clear();
                record.reported_at = 0;
            }
            Some(format!("Before task tools or a blocking question, check whether this request starts new work; if so report a fresh estimate. Then report before waiting for the user, for example `{prefix} report --unknown --activity 'Waiting for you'`."))
        }
        "PostToolUse" => {
            let done = record.done;
            let due = now - record.reminded_at >= REMIND_SECS && !done && (record.reported_at == 0 || now - record.reported_at >= REMIND_SECS);
            if due {
                record.reminded_at = now;
                Some(reminder(prefix))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Whether an event belongs to another session than the one this pane
/// started: a subagent's, where the harness gives it an id of its own.
pub fn foreign_session(record: &Record, event: &serde_json::Value) -> bool {
    let id = event["session_id"].as_str().unwrap_or("");
    !record.session_id.is_empty() && !id.is_empty() && id != record.session_id
}

/// The event under its Claude Code name, which the rest of the reporter
/// uses: `hook_event_name` rewritten from the harness's own name.
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
