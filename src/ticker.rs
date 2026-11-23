//! The ticker: one background loop per projects root.
//!
//! Everything it does is "check on an interval, compare with last time, act".
//! It exits on request through a stop file, never through signals, and every
//! exit — the stop file, the five-minute idle one, an error, a panic — leaves
//! a line in `.ticker.log`: a ticker that stops is never a silent one. On macOS
//! a LaunchAgent with an unconditional `KeepAlive` starts it again.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::coordinator;
use crate::core::{Agent, Core, Pane};
use crate::paths::Ctx;
use crate::project::{self, Project};
use crate::runner::{Cmd, Runner};
use crate::steps::{self, Memory, Transition};
use crate::{inbox, thread};

pub const TICK: Duration = Duration::from_secs(15);
/// How often a brief is checked for between ticks after an agent start.
const BRIEF_POLL: Duration = Duration::from_secs(2);
const STOP_WAIT: Duration = Duration::from_secs(60);
const IDLE_EXIT: Duration = Duration::from_secs(300);
/// How long `ticker start` waits for the loop it just started to take the lock.
const START_WAIT: Duration = Duration::from_secs(10);
const LOG_CAP: u64 = 1_000_000;

/// `HP_TICK_TIMINGS=1` in the ticker's environment: every tick writes one line
/// per stage of every project and one line for the round, all in milliseconds
/// (`timings <slug>: 8412ms (lists 900, keep_names 700, …)`).
///
/// Anything else — unset, empty, `0`, `false`, `off` — is today's ticker
/// exactly: no extra line, no extra call, no extra `Instant::now()`.
///
/// This exists because the loop was measured by guesswork and the guesses were
/// wrong: a 15-second tick took 90–100 seconds a round (2026-11-23, t-0091),
/// and the only honest way to cut it is to see which stage costs what.
pub const TIMINGS_VAR: &str = "HP_TICK_TIMINGS";

pub fn timings_from_env() -> bool {
    let value = std::env::var(TIMINGS_VAR).unwrap_or_default();
    let value = value.trim().to_ascii_lowercase();
    !(value.is_empty() || value == "0" || value == "false" || value == "no" || value == "off")
}

/// Where a round's milliseconds went, per project, and — for every round, asked
/// for or not — how each stage adds up across the projects of the round.
///
/// Stage marks are boundaries: `stage` closes the stage that was running and
/// opens the named one, so a sequence of marks around the calls of a pass costs
/// one call per stage. The per-owner record is only written to the log when
/// [`TIMINGS_VAR`] asks; the per-stage totals are the standing measurement
/// (t-0095: a deployed ticker was five times slower per stage than the same
/// binary in a terminal, and there was no way to see it from the deployed one —
/// its environment is the plist's, so no variable could be added to it).
#[derive(Debug, Default)]
pub struct Timings {
    /// Whether the per-project lines are written to `.ticker.log`.
    log: bool,
    open: Option<(&'static str, String, Instant)>,
    spans: Vec<(String, Vec<(&'static str, Duration)>)>,
}

impl Timings {
    pub fn off() -> Timings {
        Timings::default()
    }

    pub fn on() -> Timings {
        Timings { log: true, ..Timings::default() }
    }

    /// The timing record every round keeps, and the per-project log lines only
    /// when [`TIMINGS_VAR`] asks for them.
    pub fn from_env() -> Timings {
        if timings_from_env() { Timings::on() } else { Timings::off() }
    }

    /// Closes the running stage and opens `name` under `owner` (a project slug,
    /// or `round` for the whole pass). Recorded whether or not the log lines
    /// were asked for: it is a handful of `Instant::now()` calls per project
    /// (about 250 a round), and the round's stage totals are what a deployed
    /// ticker has to answer with.
    pub fn stage(&mut self, owner: &str, name: &'static str) {
        self.close();
        self.open = Some((name, owner.to_string(), Instant::now()));
    }

    /// Closes the running stage and opens none: the end of a pass.
    pub fn stop(&mut self) {
        self.close();
    }

    /// Milliseconds an owner's stages add up to.
    #[cfg(test)]
    pub fn total(&self, owner: &str) -> Duration {
        self.spans
            .iter()
            .filter(|(name, _)| name == owner)
            .flat_map(|(_, spans)| spans.iter().map(|(_, took)| *took))
            .sum()
    }

    fn close(&mut self) {
        if let Some((name, owner, at)) = self.open.take() {
            match self.spans.iter_mut().find(|(who, _)| *who == owner) {
                Some((_, spans)) => spans.push((name, at.elapsed())),
                None => self.spans.push((owner, vec![(name, at.elapsed())])),
            }
        }
    }

    /// Every stage of the round, its milliseconds summed over the projects that
    /// ran it, most expensive first. Read before [`Timings::drain`], which is
    /// what empties the record.
    fn stage_totals(&self) -> Vec<Stage> {
        let mut totals: Vec<Stage> = Vec::new();
        for (_, spans) in &self.spans {
            for (name, took) in spans {
                let ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
                match totals.iter_mut().find(|stage| stage.stage == *name) {
                    Some(stage) => stage.ms += ms,
                    None => totals.push(Stage { stage: name.to_string(), ms }),
                }
            }
        }
        totals.sort_by(|a, b| b.ms.cmp(&a.ms).then_with(|| a.stage.cmp(&b.stage)));
        totals
    }

    /// One line per owner, stages in the order they ran, and — for a whole
    /// tick — the round's own wall time. Empties the record, so the next round
    /// starts clean, and writes the lines only when [`TIMINGS_VAR`] asks.
    fn drain(&mut self, round: Option<Duration>) -> Vec<String> {
        let spans = std::mem::take(&mut self.spans);
        if !self.log {
            return Vec::new();
        }
        let mut lines = Vec::new();
        for (owner, spans) in spans {
            if owner == ROUND {
                continue;
            }
            let total: Duration = spans.iter().map(|(_, took)| *took).sum();
            let stages: Vec<String> = spans.iter().map(|(name, took)| format!("{name} {}", took.as_millis())).collect();
            lines.push(format!("timings {owner}: {}ms ({})", total.as_millis(), stages.join(", ")));
        }
        if let Some(round) = round {
            lines.push(format!("timings round: {}ms", round.as_millis()));
        }
        lines
    }
}

/// The [`Timings`] owner of the whole pass. Named so the per-project lines can
/// skip it: its time is the one the round line prints.
const ROUND: &str = "round";

/// How old the held session view may be when the empty-Space sweep judges on
/// it. A normal round is well under this, so the sweep reads the snapshot the
/// round started with; a round that ran long (a `gh` call, a copy) drops it and
/// takes its own before closing anything.
const VIEW_FRESH: Duration = Duration::from_secs(3);

fn lock_path(root: &Path) -> PathBuf {
    root.join(".ticker.lock")
}

fn stop_path(root: &Path) -> PathBuf {
    root.join(".ticker.stop")
}

fn log_path(root: &Path) -> PathBuf {
    root.join(".ticker.log")
}

/// How many rounds [`Rounds::round_ms_recent`] keeps: enough for a median that
/// one slow round cannot move, short enough that the file stays a few lines.
pub const ROUNDS_KEPT: usize = 10;

/// `<root>/.state/ticker.json`: what the last rounds cost.
///
/// One file per root, not per project: a round is one pass over every project
/// under the root, and `TICK` is the loop's own 15 seconds, so the number is
/// about the loop and not about any one project. Each project's own
/// `.state/ticker.json` stays what it always was (what that project compares
/// against last tick). Written once per round, next to the round's line in
/// `.ticker.log` and without `HP_TICK_TIMINGS` having to be on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rounds {
    /// The last round, in milliseconds.
    pub round_ms: u64,
    /// The last [`ROUNDS_KEPT`] rounds, oldest first, for the median. Missing
    /// in a file written before this existed, which reads as no history.
    pub round_ms_recent: Vec<u64>,
    /// The last round's stages, most expensive first: each stage's milliseconds
    /// summed over every project that ran it, plus the round's own stages
    /// (`rename`, `headless`). Only the last round is kept, and the number of
    /// stages is the fixed set of names the code marks, so this file stays a
    /// few dozen lines. Missing in a file written before this existed: an old
    /// round, whose stages nobody recorded.
    pub round_stages: Vec<Stage>,
}

/// One stage of a round, as `doctor` and `ticker status` print it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stage {
    pub stage: String,
    pub ms: u64,
}

pub fn rounds_path(root: &Path) -> PathBuf {
    root.join(".state").join("ticker.json")
}

/// The rounds recorded for `root`; the default (no round yet) when the file is
/// missing, unreadable or older than these fields. Never an error: `doctor`
/// asks a root that may have no ticker at all.
pub fn load_rounds(root: &Path) -> Rounds {
    crate::project::read_json(&rounds_path(root)).unwrap_or_default()
}

/// Records one round: one read, one write, in the same 15-second beat as
/// `tick_at` (no `Instant::now()` and no file of its own).
fn record_round(root: &Path, took: Duration, stages: Vec<Stage>) -> Result<Rounds> {
    let mut rounds = load_rounds(root);
    let ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
    rounds.round_ms = ms;
    rounds.round_ms_recent.push(ms);
    let excess = rounds.round_ms_recent.len().saturating_sub(ROUNDS_KEPT);
    rounds.round_ms_recent.drain(..excess);
    rounds.round_stages = stages;
    let path = rounds_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    crate::project::write_json(&path, &rounds)?;
    Ok(rounds)
}

fn median_of(values: &[u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) { (sorted[middle - 1] + sorted[middle]) / 2 } else { sorted[middle] }
}

/// How the last rounds look against the `TICK` the loop must keep up with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundHealth {
    /// No round recorded: a fresh root, or a ticker that has not finished one.
    /// Not a zero-millisecond round, and said as such.
    NoData,
    Fine,
    /// The last round, or the median of the recent ones, is over the tick.
    Slow,
}

/// The one line about round times: `doctor` prints it as `ticker round` and
/// `ticker status` as `round`, so the two can never disagree.
pub fn round_line(root: &Path) -> (RoundHealth, String) {
    rounds_line(&load_rounds(root))
}

pub fn rounds_line(rounds: &Rounds) -> (RoundHealth, String) {
    let recent = &rounds.round_ms_recent;
    // No file, or one written before this existed: say so rather than print a
    // zero that would read as a very fast round.
    if recent.is_empty() && rounds.round_ms == 0 {
        return (RoundHealth::NoData, "no round recorded yet".to_string());
    }
    let last = rounds.round_ms;
    let median = if recent.is_empty() { last } else { median_of(recent) };
    let tick = u64::try_from(TICK.as_millis()).unwrap_or(u64::MAX);
    let mut text = format!("last {last}ms");
    if !recent.is_empty() {
        text.push_str(&format!(", median of {} {median}ms", recent.len()));
    }
    text.push_str(&format!(" (TICK {tick}ms)"));
    let over = last > tick || median > tick;
    if over {
        // Where the last round went, in the words of the stages that took it:
        // the deployed ticker's environment is the plist's, so this line is the
        // only way to learn that from the deployed one.
        let worst: Vec<String> = rounds
            .round_stages
            .iter()
            .filter(|stage| stage.ms > 0)
            .take(3)
            .map(|stage| format!("{} {}ms", stage.stage, stage.ms))
            .collect();
        text.push_str(&format!(" — over the {tick}ms TICK;"));
        if worst.is_empty() {
            text.push_str(&format!(" set {TIMINGS_VAR}=1 for the per-stage breakdown"));
        } else {
            text.push_str(&format!(" most of it: {}", worst.join(", ")));
        }
    }
    (if over { RoundHealth::Slow } else { RoundHealth::Fine }, text)
}

/// What the lock holder writes into the lock file, for `ticker status` and
/// `doctor`. The pid is for display only; nothing signals it.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Info {
    pub version: String,
    pub pid: u32,
    pub root: String,
    pub started: String,
    /// Where the ticker resolves its tools from its own environment, which may
    /// differ from the user's shell.
    pub tools: Vec<(String, String)>,
}

#[derive(Debug, PartialEq)]
pub enum LockState {
    Free,
    Held(Info),
}

/// Probes the lock without keeping it. The file is never created here.
pub fn lock_state(root: &Path) -> LockState {
    let Ok(mut file) = File::options().read(true).write(true).open(lock_path(root)) else {
        return LockState::Free;
    };
    match file.try_lock() {
        Ok(()) => LockState::Free,
        Err(_) => {
            let mut text = String::new();
            let _ = file.read_to_string(&mut text);
            LockState::Held(serde_json::from_str(&text).unwrap_or_default())
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum StartAction {
    Spawn,
    Nothing,
    StopThenSpawn,
}

/// The `ticker start` decision. A healthy ticker of the same version is never
/// replaced; a different version, or a stop in progress, is stopped first so
/// `open` never ends with no ticker.
pub fn decide_start(lock: &LockState, my_version: &str, stop_file_exists: bool) -> StartAction {
    match lock {
        LockState::Free => StartAction::Spawn,
        LockState::Held(info) if info.version == my_version && !stop_file_exists => StartAction::Nothing,
        LockState::Held(_) => StartAction::StopThenSpawn,
    }
}

/// The LaunchAgent label every root used before labels were derived from the
/// root (t-0083). One label for the whole machine meant `scripts/dev-hp open`
/// booted the **real** ticker out and rewrote its plist. Kept as the prefix of
/// [`agent_label`] and for the one-time migration, and as the label a ticker
/// started before the change is still running under.
pub const LEGACY_AGENT_LABEL: &str = "dev.hp.ticker";

const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(10);

/// How long launchd waits before starting the ticker again after it exited:
/// long enough that a crash loop (a panic at startup) costs one start a
/// minute rather than a spinning CPU, short enough that a real outage is
/// recovered while somebody is still looking at the screen.
const THROTTLE_SECS: u32 = 60;

/// The LaunchAgent label of `root`'s ticker: [`LEGACY_AGENT_LABEL`] plus a
/// fingerprint of the canonical root. Two roots keep two jobs and two plists,
/// so a scratch root (`scripts/dev-hp`) can neither unload nor overwrite the
/// real one's. `ticker start`, `stop`, `status` and `doctor` all read the label
/// from here, so they cannot disagree about which job is this root's.
pub fn agent_label(root: &Path) -> String {
    format!("{LEGACY_AGENT_LABEL}.{}", root_tag(root))
}

/// A short fingerprint of the root: FNV-1a over its canonical path, folded to
/// 32 bits. It is stable across builds on purpose — a hash that moved with the
/// compiler would mint a new label on every rebuild and leave the previous job
/// loaded for ever.
fn root_tag(root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in canonical.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:08x}", ((hash >> 32) as u32) ^ (hash as u32))
}

fn agent_path_in(home: &Path, label: &str) -> PathBuf {
    home.join("Library/LaunchAgents").join(format!("{label}.plist"))
}

/// The plist of `root`'s ticker.
pub fn agent_path(home: &Path, root: &Path) -> PathBuf {
    agent_path_in(home, &agent_label(root))
}

/// The retired shared plist, and the copy of it moved aside by
/// [`migrate_legacy_agent`] while a ticker still runs under that job.
fn legacy_paths(home: &Path) -> [PathBuf; 2] {
    let live = agent_path_in(home, LEGACY_AGENT_LABEL);
    let aside = PathBuf::from(format!("{}.migrated", live.display()));
    [live, aside]
}

/// The plist text for a ticker of `root`.
///
/// `KeepAlive` is unconditional and `RunAtLoad` is set, so launchd starts the
/// ticker after a reboot and after *any* exit: a crash, a panic, `ticker stop`
/// without the plist being removed, and the five-minute idle exit. Until
/// 2026-11-17 it was `<dict><key>SuccessfulExit</key><false/></dict>`, which
/// restarted only a non-zero exit — a ticker that exited cleanly (or was
/// killed so that its exit code looked clean) stayed dead, and nothing said
/// so: the 2026-11-17 14:39Z disappearance had no stop line, no crash line and
/// no LaunchAgent loaded at all.
///
/// `ProcessType` is `Standard`, not `Background` (t-0095). `Background` puts the
/// whole loop in launchd's throttled background policy, and every process it
/// starts inherits it: measured on the same binary and root, a round took
/// 9–25 s under `Background` and 0.9–1.5 s under `Standard`, because one
/// `hc`/`herdr-app` spawn cost 40–55 ms there against 8–12 ms in a terminal
/// (a `true` cost 15 ms against 3 ms). A loop with a 15-second beat cannot pay
/// that: it must keep up, and the work it does is unavoidable subprocess calls,
/// not CPU it could yield. `Standard` is launchd's own default for an agent,
/// so writing it is saying what was already true for everything else.
fn agent_plist(binary: &Path, root: &Path, log: &Path) -> String {
    let escape = |text: &str| text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
  <key>Label</key><string>{label}</string>\n\
  <key>ProgramArguments</key>\n  <array>\n\
    <string>{binary}</string>\n    <string>--root</string>\n    <string>{root}</string>\n    <string>ticker</string>\n    <string>run</string>\n\
  </array>\n\
  <key>RunAtLoad</key><true/>\n\
  <key>KeepAlive</key><true/>\n\
  <key>ThrottleInterval</key><integer>{THROTTLE_SECS}</integer>\n\
  <key>ProcessType</key><string>Standard</string>\n\
  <key>StandardOutPath</key><string>{log}</string>\n\
  <key>StandardErrorPath</key><string>{log}</string>\n\
</dict>\n</plist>\n",
        label = agent_label(root),
        binary = escape(&binary.to_string_lossy()),
        root = escape(&root.to_string_lossy()),
        log = escape(&log.to_string_lossy()),
    )
}

/// The program a plist runs — the first `<string>` after `ProgramArguments`.
/// Only for saying *what* an out-of-date plist points at: the 2026-11-19 accident
/// had a plist rewritten to a build in a worktree, and a name in the report is
/// what makes that visible.
fn plist_program(text: &str) -> Option<String> {
    let rest = text.split("<key>ProgramArguments</key>").nth(1)?;
    let start = rest.find("<string>")? + "<string>".len();
    let end = rest[start..].find("</string>")? + start;
    Some(rest[start..end].to_string())
}

/// The `--root` a plist's ticker runs, from its `ProgramArguments`.
fn plist_root(text: &str) -> Option<String> {
    let rest = text.split("<key>ProgramArguments</key>").nth(1)?;
    let after = rest.split("<string>--root</string>").nth(1)?;
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")? + start;
    // `agent_plist` escapes these three; anything else is written as it is.
    Some(after[start..end].replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&"))
}

/// Whether the root a plist names is `root`, canonical where both exist (a
/// root that is not there yet has no canonical form).
fn same_root(named: &str, root: &Path) -> bool {
    let named_path = Path::new(named);
    match (named_path.canonicalize(), root.canonicalize()) {
        (Ok(one), Ok(other)) => one == other,
        _ => named_path == root,
    }
}

/// Whether the plist at `path` is this root's ticker.
fn plist_of_root(path: &Path, root: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| plist_root(&text).is_some_and(|named| same_root(&named, root)))
}

/// This root's plist under the shared label, still on disk or moved aside.
pub fn legacy_plist(home: &Path, root: &Path) -> Option<PathBuf> {
    legacy_paths(home).into_iter().find(|path| plist_of_root(path, root))
}

/// The one-time move off the shared `dev.hp.ticker` job (t-0083).
///
/// That label was the same string for every root, so a `scripts/dev-hp open`
/// booted the real ticker out (SIGTERM through `bootout`) and rewrote its
/// plist to the scratch root. The new label is [`agent_label`]; this retires
/// the old job **only when its plist says it is this root's** (`--root` in
/// `ProgramArguments`). A plist that names another root, or names none, is
/// left exactly where it is: it is that root's ticker.
///
/// `bootout` is SIGTERM, so a job a ticker is still running under is not
/// unloaded: its plist is moved aside instead (a reboot then loads only the new
/// one), and `ticker stop` boots the job out with that file as its proof.
fn migrate_legacy_agent(home: &Path, root: &Path, log: &Log, runner: &dyn Runner, running: bool) {
    let [live, aside] = legacy_paths(home);
    if live.exists() && !plist_of_root(&live, root) {
        log.line(&format!(
            "left {} alone: it is the LaunchAgent of another root, not {}",
            live.display(),
            root.display()
        ));
        return;
    }
    if !live.exists() && !plist_of_root(&aside, root) {
        return;
    }
    let uid = unsafe { libc_getuid() };
    if running {
        // The ticker that holds the lock was started by this very job: one
        // `bootout` would be its SIGTERM. Moving the plist aside is enough to
        // keep a reboot from loading it beside the new job.
        if live.exists() && std::fs::rename(&live, &aside).is_ok() {
            log.line(&format!(
                "the shared LaunchAgent `{LEGACY_AGENT_LABEL}` ({}) is this root's and a ticker still runs under it, so its job was left loaded and its plist moved to {}; a reboot loads only `{}`, and `hp ticker stop` unloads the old job",
                live.display(),
                aside.display(),
                agent_label(root)
            ));
        }
        return;
    }
    launchctl(runner, &["bootout", &format!("gui/{uid}/{LEGACY_AGENT_LABEL}")]);
    let removed = live.exists() || aside.exists();
    let _ = std::fs::remove_file(&live);
    let _ = std::fs::remove_file(&aside);
    if removed {
        log.line(&format!(
            "moved this root off the shared LaunchAgent `{LEGACY_AGENT_LABEL}` to `{}`: the old job is unloaded and its plist removed, and nothing was running under it",
            agent_label(root)
        ));
    }
}

/// The LaunchAgent as `doctor` sees it: whether the plist is there, whether it
/// is the text [`install_agent`] would write now (an old `KeepAlive` is what
/// the 2026-11-17 accident left on disk), and whether launchd has the job
/// loaded — a written plist nobody loaded starts nothing.
pub struct AgentState {
    pub path: PathBuf,
    pub installed: bool,
    pub current: bool,
    pub loaded: bool,
    /// This root's plist under the shared label t-0083 retired, while it is
    /// still on disk: the state between the two labels, after an upgrade and
    /// before the ticker's next start.
    pub legacy: Option<PathBuf>,
    /// The program the plist on disk runs, when it could be read.
    pub program: Option<String>,
}

/// `None` where this machine has no LaunchAgents (not macOS), so `doctor`
/// there prints exactly what it printed before.
pub fn agent_state(home: &Path, binary: &Path, root: &Path, runner: &dyn Runner) -> Option<AgentState> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let path = agent_path(home, root);
    let text = std::fs::read_to_string(&path).ok();
    let legacy = legacy_plist(home, root);
    Some(AgentState {
        installed: text.is_some(),
        current: text.as_deref().is_some_and(|text| text == agent_plist(binary, root, &log_path(root))),
        // The question `doctor` and `ticker status` ask is "would launchd start
        // the ticker again", and the old shared job answers it just as well as
        // the derived one while the two overlaps.
        loaded: agent_loaded(root, runner) || (legacy.is_some() && label_loaded(LEGACY_AGENT_LABEL, runner)),
        legacy,
        program: text.as_deref().and_then(plist_program),
        path,
    })
}

/// Whether launchd holds this root's job, asked of launchd itself rather than
/// guessed from the plist: `launchctl print` fails for a label that is not
/// loaded.
fn agent_loaded(root: &Path, runner: &dyn Runner) -> bool {
    label_loaded(&agent_label(root), runner)
}

fn label_loaded(label: &str, runner: &dyn Runner) -> bool {
    let uid = unsafe { libc_getuid() };
    let print = Cmd::new("launchctl", LAUNCHCTL_TIMEOUT).args(["print", &format!("gui/{uid}/{label}")]);
    runner.run(&print).map(|out| out.success()).unwrap_or(false)
}

/// The LaunchAgent label this process runs under, when launchd started it:
/// this root's own label, or the shared one every root used before t-0083.
/// Only those two count — `XPC_SERVICE_NAME` set to anything else is not a job
/// of this ticker, and unloading it would end somebody else's.
fn ticker_job_label(root: &Path) -> Option<String> {
    let name = std::env::var("XPC_SERVICE_NAME").ok()?;
    (name == agent_label(root) || name == LEGACY_AGENT_LABEL).then_some(name)
}

fn launchctl(runner: &dyn Runner, args: &[&str]) {
    let _ = runner.run(&Cmd::new("launchctl", LAUNCHCTL_TIMEOUT).args(args.iter().copied()));
}

/// Writes the plist and makes sure launchd has the job loaded, so a stopped or
/// rebooted machine brings the ticker back without anybody running a command.
///
/// Idempotent, and **it never bounces a ticker that is running**: a plist that is
/// out of date is rewritten and takes effect at the ticker's next start, because
/// `bootout` is SIGTERM and the 2026-11-19 accident lost seven hours to a
/// command that unloaded a healthy job to make a strictly better plist
/// effective. `bootout` is run only when nothing holds the root's lock.
pub fn install_agent(home: &Path, binary: &Path, root: &Path, runner: &dyn Runner) -> Result<LoadReport> {
    load_agent(home, binary, root, runner, false)
}

/// [`install_agent`], plus the reload a job that launchd has loaded and nothing
/// is running under (`doctor --fix`; `hp ticker status` when it heals one).
/// Even this one leaves a running ticker alone: a loop that is up is not
/// something a repair has to kill.
pub fn reload_agent(home: &Path, binary: &Path, root: &Path, runner: &dyn Runner) -> Result<LoadReport> {
    load_agent(home, binary, root, runner, true)
}

/// What loading the LaunchAgent did or refused to do, so a caller can say it.
#[derive(Debug, Default, PartialEq)]
pub struct LoadReport {
    /// The plist on disk was not the text this binary and root would write.
    pub wrote_plist: bool,
    /// `launchctl bootstrap` ran and launchd accepted the job.
    pub bootstrapped: bool,
    /// The job was unloaded first.
    pub bounced: bool,
    /// A ticker held the root's lock, so it was left running.
    pub kept_running: bool,
}

fn agent_lock_path(root: &Path) -> PathBuf {
    root.join(".ticker.agent.lock")
}

/// Serializes the unload-and-load pair across `hp` processes: two commands run
/// at once used to interleave them, and the one whose `bootstrap` landed on a
/// label the other had already loaded failed.
pub(crate) struct AgentLock(Option<File>);

impl Drop for AgentLock {
    fn drop(&mut self) {
        if let Some(file) = &self.0 {
            let _ = file.unlock();
        }
    }
}

fn agent_lock(root: &Path) -> AgentLock {
    let file = File::options().create(true).truncate(false).read(true).write(true).open(agent_lock_path(root)).ok();
    if let Some(file) = &file {
        let _ = file.lock();
    }
    AgentLock(file)
}

fn load_agent(home: &Path, binary: &Path, root: &Path, runner: &dyn Runner, force: bool) -> Result<LoadReport> {
    let path = agent_path(home, root);
    let text = agent_plist(binary, root, &log_path(root));
    let log = Log { path: log_path(root) };
    let uid = unsafe { libc_getuid() };
    // One `hp` command at a time writes and loads: the pair below is what a
    // second process used to split (both unloaded, one loaded).
    let _serial = agent_lock(root);
    let previous = std::fs::read_to_string(&path).ok();
    let current = previous.as_deref() == Some(text.as_str());
    // Asked of launchd, not guessed from the plist: a written plist nobody
    // loaded starts nothing. It can also fail on a launchd hiccup while the job
    // is loaded, which is why a failure here never leads to a `bootout` of a
    // ticker that holds the lock.
    let loaded = agent_loaded(root, runner);
    let running = lock_state(root) != LockState::Free;
    // Before anything else, and even when this root's text is already right: a
    // job left on the shared label is this root's only if its plist says so
    // (t-0083).
    migrate_legacy_agent(home, root, &log, runner, running);
    let mut report = LoadReport::default();
    if current && loaded && !force {
        return Ok(report);
    }
    if !current {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
        }
        std::fs::write(&path, &text).with_context(|| format!("could not write {}", path.display()))?;
        report.wrote_plist = true;
    }
    // A ticker holds the lock: it is the loop this LaunchAgent exists to keep
    // alive, so it is left running. A new plist is used the next time launchd
    // starts the job, and a job launchd has not loaded is loaded after it exits
    // (`hp ticker start`, `hp ticker status`).
    if running {
        report.kept_running = true;
        if report.wrote_plist {
            log.line(&format!(
                "wrote {} (the text was out of date: it now runs {}); a ticker holds {}, so it left it running — the new text takes effect at its next start",
                path.display(),
                binary.display(),
                lock_path(root).display()
            ));
        }
        return Ok(report);
    }
    if loaded {
        launchctl(runner, &["bootout", &format!("gui/{uid}/{}", agent_label(root))]);
        report.bounced = true;
    }
    let bootstrap = || Cmd::new("launchctl", LAUNCHCTL_TIMEOUT).args(["bootstrap", &format!("gui/{uid}"), &path.to_string_lossy()]);
    match runner.run(&bootstrap()) {
        Ok(out) if out.success() => {
            report.bootstrapped = true;
            if report.bounced {
                log.line(&format!("loaded {} again (nothing was running under it)", path.display()));
            }
            Ok(report)
        }
        Ok(out) => {
            // Nothing is loaded now, and a failed load that leaves no trace is
            // the other half of the 2026-11-19 accident: the reason used to
            // reach only the stdout of whichever command happened to run.
            let refused = format!("launchctl bootstrap failed: {}", out.error_text());
            log.line(&refused);
            revert_plist(&path, previous.as_deref(), &log, runner, &bootstrap);
            bail!("{refused} (the LaunchAgent is not loaded, so nothing starts the ticker; `hp ticker start` repairs it, see {})", log_path(root).display())
        }
        Err(error) => {
            log.line(&format!("could not run launchctl bootstrap ({error:#})"));
            Err(error).context("could not run launchctl")
        }
    }
}

/// Puts the plist launchd refused back to what it was, and tries that one: a
/// newer text that will not load must not be the one left on disk.
fn revert_plist(path: &Path, previous: Option<&str>, log: &Log, runner: &dyn Runner, bootstrap: &dyn Fn() -> Cmd) {
    match previous {
        Some(old) => {
            let _ = std::fs::write(path, old);
            match runner.run(&bootstrap()).map(|out| out.success()) {
                Ok(true) => log.line("the previous text was put back and launchd loaded it"),
                _ => log.line("the previous text was put back, but launchd would not load that one either: nothing of this agent is loaded"),
            }
        }
        None => {
            let _ = std::fs::remove_file(path);
            log.line("the plist this run wrote was removed again: launchd has the job it had before");
        }
    }
}

/// Unloads the agent and removes its plist: `ticker stop` means stopped.
///
/// `home` and `runner` are parameters rather than the process's own, so a test
/// can never unload the LaunchAgent of the machine it runs on.
pub fn remove_agent(home: &Path, root: &Path, runner: &dyn Runner) {
    let path = agent_path(home, root);
    let uid = unsafe { libc_getuid() };
    launchctl(runner, &["bootout", &format!("gui/{uid}/{}", agent_label(root))]);
    let _ = std::fs::remove_file(&path);
    // The old shared job this root still runs under, if any: a loaded job with
    // `KeepAlive` is what would start the ticker again right after `stop`. Only
    // a plist that names this root is unloaded — another root's job is its
    // own, which is the whole point of the derived label.
    for file in legacy_paths(home) {
        if plist_of_root(&file, root) {
            launchctl(runner, &["bootout", &format!("gui/{uid}/{LEGACY_AGENT_LABEL}")]);
            let _ = std::fs::remove_file(&file);
        }
    }
}

unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

/// Spawns the detached loop unless there is nothing to watch. It creates
/// nothing when the root does not exist or contains no projects, so a linked
/// plugin's `[[startup]]` is harmless in sessions that have no projects.
pub fn start(ctx: &Ctx) -> Result<()> {
    start_with(ctx, START_WAIT, &spawn)
}

/// [`start`] with the wait and the fallback passed in, so a test can follow the
/// stop-then-spawn path without starting a process.
fn start_with(ctx: &Ctx, wait: Duration, spawn_directly: &dyn Fn(&Path) -> Result<()>) -> Result<()> {
    let root = &ctx.root;
    if !ctx.detached_ticker || project::list_slugs(root).is_empty() {
        return Ok(());
    }
    let stop_exists = stop_path(root).exists();
    match decide_start(&lock_state(root), crate::VERSION, stop_exists) {
        StartAction::Nothing => Ok(()),
        StartAction::Spawn => {
            // A leftover stop file would make the new ticker exit at once.
            let _ = std::fs::remove_file(stop_path(root));
            launch_with(ctx, wait, spawn_directly)
        }
        StartAction::StopThenSpawn => {
            stop(ctx)?;
            launch_with(ctx, wait, spawn_directly)
        }
    }
}

/// Starts the loop the way this machine can keep it: through its LaunchAgent
/// where there is one, so a version change does not leave a ticker that nothing
/// restarts (that is how the 2026-11-17 disappearance left no agent behind); a
/// plain detached process otherwise (`spawn_directly`).
///
/// Loading the agent is not the same as the ticker running: `launchctl
/// bootstrap` can succeed while nothing takes the lock (a stale job, a launchd
/// in a bad state). So the loop that was started is looked for in the lock file,
/// and when it does not appear, one is spawned directly and the reason is
/// printed — `hp ticker start` never reports success with no process behind it.
/// The wait and the fallback are parameters, so a test can see both without
/// starting a process.
fn launch_with(ctx: &Ctx, wait: Duration, spawn_directly: &dyn Fn(&Path) -> Result<()>) -> Result<()> {
    if cfg!(target_os = "macos") {
        // Both ways out are logged as well as printed: a failure that reaches
        // only the stdout of one command is invisible to everybody else, which
        // is how the 2026-11-19 disappearance left nothing in the log to read.
        let log = Log { path: log_path(&ctx.root) };
        let installed = crate::paths::binary().and_then(|binary| install_agent(&ctx.env.home, &binary, &ctx.root, ctx.runner));
        match installed {
            Ok(_) if wait_for_ticker(&ctx.root, wait) => return Ok(()),
            Ok(_) => {
                log.line(&format!(
                    "the LaunchAgent is loaded but no ticker took {} within {}s; started the loop directly",
                    lock_path(&ctx.root).display(),
                    wait.as_secs()
                ));
                println!(
                    "ticker start: the LaunchAgent is loaded but no ticker took {} within {}s; starting one directly (see {})",
                    lock_path(&ctx.root).display(),
                    wait.as_secs(),
                    log_path(&ctx.root).display()
                );
            }
            Err(error) => {
                log.line(&format!("could not install the LaunchAgent ({error:#}); started the loop directly"));
                println!("ticker start: could not install the LaunchAgent ({error:#}); starting the loop directly");
            }
        }
    }
    spawn_directly(&ctx.root)
}

unsafe extern "C" {
    fn setsid() -> i32;
}

/// `ticker run`, detached: null stdio and a new session, so it does not die
/// with the process group of whatever started it (an agent's shell tool).
fn spawn(root: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let binary = crate::paths::binary()?;
    let mut command = Command::new(binary);
    command
        .arg("--root")
        .arg(root)
        .args(["ticker", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Pane variables belong to whoever started us, not to the ticker: every
    // project carries its own recorded socket.
    for key in ["HERDR_SOCKET_PATH", "HERDR_SESSION", "HERDR_PANE_ID", "HERDR_TAB_ID", "HERDR_WORKSPACE_ID", "HC_TERMINAL_ID"] {
        command.env_remove(key);
    }
    // SAFETY: setsid is async-signal-safe and touches no memory.
    unsafe {
        command.pre_exec(|| {
            setsid();
            Ok(())
        });
    }
    command.spawn().context("could not start the ticker")?;
    Ok(())
}

/// Asks the running ticker to exit and waits for the lock to be released: the
/// stop file first (its own path, which writes a log line), then the
/// LaunchAgent is unloaded and its plist removed, so an unconditional
/// `KeepAlive` does not start the loop again.
pub fn stop(ctx: &Ctx) -> Result<()> {
    let root = &ctx.root;
    // The stop file first: the ticker then exits through its own path, which
    // logs it, instead of being killed by `bootout`'s SIGTERM.
    if lock_state(root) != LockState::Free {
        std::fs::write(stop_path(root), b"")?;
        wait_until_free(root, STOP_WAIT);
    }
    // Unload it after the wait: with an unconditional `KeepAlive` the job is
    // what would start the loop again, so it goes last, and removing it is
    // also the SIGTERM for a ticker that ignored the stop file.
    remove_agent(&ctx.env.home, &ctx.root, ctx.runner);
    let free = wait_until_free(root, STOP_WAIT);
    let _ = std::fs::remove_file(stop_path(root));
    if free {
        Ok(())
    } else {
        bail!("the ticker did not exit within {} seconds", 2 * STOP_WAIT.as_secs())
    }
}

/// Whether a ticker holds `root`'s lock right now, waiting up to `limit` for
/// one to appear. The lock is the only thing that says "a loop is running":
/// a loaded LaunchAgent is not the same fact.
pub fn wait_for_ticker(root: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if lock_state(root) != LockState::Free {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wait_until_free(root: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if lock_state(root) == LockState::Free {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// How long `ticker status` waits for a ticker it just loaded to take the lock.
const STATUS_WAIT: Duration = Duration::from_secs(5);

/// `ticker status`, and the one place a person looks when nothing is being
/// announced — so it also repairs a LaunchAgent that is written but not doing
/// its job: the plist missing, out of date, or not loaded (the state the
/// 2026-11-19 accident left behind: a plist on disk, no job in launchd, no
/// process).
pub fn status(ctx: &Ctx) -> Result<()> {
    status_with(ctx, STATUS_WAIT)
}

fn status_with(ctx: &Ctx, wait: Duration) -> Result<()> {
    match lock_state(&ctx.root) {
        LockState::Free => println!("ticker: not running (root {})", ctx.root.display()),
        LockState::Held(info) => {
            println!("ticker: running");
            println!("  version: {}", info.version);
            println!("  pid:     {}", info.pid);
            println!("  root:    {}", info.root);
            println!("  started: {}", info.started);
            for (tool, path) in &info.tools {
                println!("  {tool:<6} {path}");
            }
            if info.version != crate::VERSION {
                println!("  note: this binary is {}; `ticker start` replaces the running one", crate::VERSION);
            }
        }
    }
    let (_, round) = round_line(&ctx.root);
    println!("round:  {round}");
    let agent = status_agent(ctx, wait);
    if !agent.is_empty() {
        println!("{agent}");
    }
    Ok(())
}

/// The LaunchAgent half of `ticker status`: what launchd has, and the repair
/// when it is not "written, loaded, and a ticker running under it". Empty where
/// this machine has no LaunchAgents (not macOS).
///
/// The repair is safe to run on a healthy machine because a ticker that holds
/// the root's lock is never bounced: this command is what a person types when a
/// project has been silent for hours, and it must not be the thing that kills a
/// working loop.
fn status_agent(ctx: &Ctx, wait: Duration) -> String {
    let Ok(binary) = crate::paths::binary() else { return String::new() };
    let Some(state) = agent_state(&ctx.env.home, &binary, &ctx.root, ctx.runner) else { return String::new() };
    let path = state.path.display().to_string();
    let running = lock_state(&ctx.root) != LockState::Free;
    if state.installed && state.current && state.loaded && running {
        return format!("agent:  {path} is loaded, and launchd restarts the ticker after any exit");
    }
    let why = if !state.installed {
        match &state.legacy {
            Some(legacy) => format!("there is no plist for this root's own label yet; it still runs under the old shared one ({})", legacy.display()),
            None => "there is no plist".to_string(),
        }
    } else if !state.current {
        // Naming the program is what made the 2026-11-19 accident legible: a
        // build in a worktree had rewritten the plist to point at itself.
        match &state.program {
            Some(program) => format!("its text is out of date (it runs {program})"),
            None => "its text is out of date".to_string(),
        }
    } else if !state.loaded {
        "launchd has not loaded it, so nothing starts the ticker".to_string()
    } else {
        "launchd has it loaded but nothing is running under it".to_string()
    };
    // `reload_agent` may bounce a job launchd has and nothing runs under; with a
    // ticker holding the lock only the plist text is brought up to date.
    let repair = if running { install_agent(&ctx.env.home, &binary, &ctx.root, ctx.runner) } else { reload_agent(&ctx.env.home, &binary, &ctx.root, ctx.runner) };
    let log = log_path(&ctx.root).display().to_string();
    match repair {
        Ok(report) if report.kept_running && report.wrote_plist => format!(
            "agent:  {path}: {why}; wrote the current text and left the running ticker alone (it takes effect at the ticker's next start)"
        ),
        Ok(report) if report.kept_running => format!(
            "agent:  {path}: {why}; the ticker runs but launchd has no job for it, so nothing restarts it if it exits — `hp ticker start` loads the job once it has stopped"
        ),
        Ok(_) => {
            let alive = wait_for_ticker(&ctx.root, wait);
            match alive {
                true => format!("agent:  {path}: {why}; repaired, and a ticker is running under it"),
                false => format!("agent:  {path}: {why}; loaded the job, but no ticker took the lock within {}s — run `hp ticker start` (see {log})", wait.as_secs()),
            }
        }
        Err(error) => format!("agent:  {path}: {why}; repairing it failed ({error:#}) — see {log}"),
    }
}

/// Where a tool resolves from this process's own `PATH`.
fn which(tool: &str, path_var: &str) -> String {
    if tool.contains('/') {
        return tool.to_string();
    }
    std::env::split_paths(path_var)
        .map(|dir| dir.join(tool))
        .find(|candidate| candidate.is_file())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(not found)".to_string())
}

pub struct Log {
    path: PathBuf,
}

impl Log {
    /// A log at `path`. The ticker's own log is `.ticker.log` under the root;
    /// tests point one at a temporary file to read what was written.
    #[cfg(test)]
    pub fn at(path: PathBuf) -> Log {
        Log { path }
    }

    pub fn line(&self, text: &str) {
        let Ok(mut file) = File::options().create(true).append(true).open(&self.path) else {
            return;
        };
        let _ = writeln!(file, "{} {}", project::now(), text.replace('\n', " "));
        // Size cap: keep the newer half.
        if file.metadata().map(|m| m.len()).unwrap_or(0) > LOG_CAP
            && let Ok(mut reader) = File::open(&self.path)
        {
            let mut tail = Vec::new();
            if reader.seek(SeekFrom::End(-((LOG_CAP / 2) as i64))).is_ok() && reader.read_to_end(&mut tail).is_ok() {
                let start = tail.iter().position(|b| *b == b'\n').map_or(0, |i| i + 1);
                let _ = project::write_atomic(&self.path, &tail[start..]);
            }
        }
    }
}

/// Set by the SIGTERM/SIGINT handler; read by the loop. An atomic store is all
/// a signal handler may do safely.
static TERMINATE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_signal(_: i32) {
    TERMINATE.store(true, std::sync::atomic::Ordering::SeqCst);
}

unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

/// SIGTERM and SIGINT ask the loop to stop instead of killing it: launchd's
/// `bootout` (`ticker stop`, a reinstall after a version change) and a terminal
/// Ctrl-C are the two ways this process is ended from outside, and both used to
/// leave a log that simply stopped. `signal(2)` with a handler that only sets a
/// flag is the whole mechanism — no crate, no unsafe work beyond the call — and
/// the loop logs the exit like every other way out.
fn install_signal_handlers() {
    let handler = on_signal as extern "C" fn(i32);
    // SAFETY: `signal` with a handler that touches nothing but an atomic.
    unsafe {
        signal(SIGTERM, handler as usize);
        signal(SIGINT, handler as usize);
    }
}

/// Whether a signal has asked the loop to stop.
fn terminated() -> bool {
    TERMINATE.load(std::sync::atomic::Ordering::SeqCst)
}

/// The loop, with every exit written to `.ticker.log`.
///
/// A ticker that stops has to say why: a `.ticker.log` that simply ends is what
/// made the 2026-11-17 disappearance invisible (no stop line, no crash line,
/// and no LaunchAgent loaded to start it again). Each way out is logged — the
/// root has no projects, another ticker holds the lock, the stop file, five
/// minutes with no reachable session, an error return — and a panic anywhere in
/// the loop goes through [`install_panic_hook`].
pub fn run(ctx: &Ctx) -> Result<()> {
    let log = Log { path: log_path(&ctx.root) };
    install_panic_hook(log.path.clone());
    // The tests share one process and one process-wide signal table: a test
    // binary must not start ignoring SIGTERM for its parent.
    if !cfg!(test) {
        install_signal_handlers();
    }
    let outcome = run_logged(ctx, &log);
    if let Err(error) = &outcome {
        log.line(&format!("ticker stopped on an error: {error:#}"));
    }
    outcome
}

/// Records a panic in the ticker's log, then runs the previous hook (the
/// default one prints to standard error, which the plist also sends to the
/// same file). A process that dies without a word is the failure this exists
/// to end.
fn install_panic_hook(path: PathBuf) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location().map(|place| format!(" at {}:{}", place.file(), place.line())).unwrap_or_default();
        Log { path: path.clone() }.line(&format!("ticker panicked{location}: {info}"));
        previous(info);
    }));
}

/// The loop. Exits when the root has no projects, when another ticker holds
/// the lock, when the stop file appears, or when no project has had a
/// reachable session for five minutes.
fn run_logged(ctx: &Ctx, log: &Log) -> Result<()> {
    let root = &ctx.root;
    if project::list_slugs(root).is_empty() {
        log.line(&format!("no projects under {}; exiting", root.display()));
        return Ok(());
    }
    let mut lock = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path(root))?;
    if lock.try_lock().is_err() {
        log.line(&format!("another ticker holds the lock ({}); exiting", lock_path(root).display()));
        // Under launchd, exiting means being started again a moment later: two
        // tickers would trade the lock for ever. Unload the job (not the plist,
        // so `doctor` sees it was written but is not loaded) and step aside.
        if let Some(label) = ticker_job_label(root) {
            let uid = unsafe { libc_getuid() };
            log.line("this instance is launchd's job and another ticker has the lock; unloading the job");
            launchctl(ctx.runner, &["bootout", &format!("gui/{uid}/{label}")]);
        }
        return Ok(());
    }
    let path_var = ctx.env.var("PATH").unwrap_or("").to_string();
    let info = Info {
        version: crate::VERSION.to_string(),
        pid: std::process::id(),
        root: root.display().to_string(),
        started: project::now(),
        tools: ["herdr", "git", "gh", "rsync"]
            .iter()
            .map(|tool| {
                let name = if *tool == "herdr" { ctx.env.hc_bin() } else { tool.to_string() };
                (tool.to_string(), which(&name, &path_var))
            })
            .collect(),
    };
    lock.set_len(0)?;
    lock.write_all(serde_json::to_string_pretty(&info)?.as_bytes())?;
    lock.flush()?;

    log.line(&format!("ticker {} started (pid {})", info.version, info.pid));
    let mut last_reachable = Instant::now();
    let mut memory = Memory::new(ctx);
    let mut brief_timings = Timings::from_env();
    loop {
        if stop_path(root).exists() {
            log.line("stop file found; exiting");
            return Ok(());
        }
        if terminated() {
            log.line("asked to stop by a signal (SIGTERM or SIGINT); exiting");
            return Ok(());
        }
        if tick(ctx, log, &mut memory) {
            last_reachable = Instant::now();
        } else if last_reachable.elapsed() > IDLE_EXIT {
            log.line("no project has had a reachable session for five minutes; exiting (the LaunchAgent starts it again)");
            return Ok(());
        }
        // Sleep in short slices so a stop request is honoured promptly.
        let wake = Instant::now() + TICK;
        // Cheap when no brief waits: it reads the thread records only.
        let mut briefs = true;
        let mut next_brief = Instant::now() + BRIEF_POLL;
        while Instant::now() < wake {
            if stop_path(root).exists() || terminated() {
                break;
            }
            if briefs && Instant::now() >= next_brief {
                // Timed like a tick when `HP_TICK_TIMINGS` asks: a brief pass
                // that reads every project's threads is part of what a person
                // waits for after `thread start`.
                brief_timings.stage("brief", "pass");
                briefs = brief_pass(ctx, log);
                brief_timings.stop();
                // No round line: a brief pass is not a round, and its own line
                // already carries its wall time. Printing "timings round" here
                // put a second, much smaller number under the same word as the
                // tick's round, which is the number `doctor` now reports.
                for line in brief_timings.drain(None) {
                    log.line(&line);
                }
                next_brief = Instant::now() + BRIEF_POLL;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

/// One pass over every active project. Cheap work (state, prompts, tokens)
/// comes first for every project, then slow work (copies, launches), so one
/// slow project does not delay the others' sidebar. Returns whether any
/// project's session was reachable. A failure in one project never stops the
/// others.
pub fn tick(ctx: &Ctx, log: &Log, memory: &mut Memory) -> bool {
    tick_with(ctx, log, memory, &mut Timings::from_env())
}

/// [`tick`] with the timings the caller holds, so `HP_TICK_TIMINGS` is read in
/// one place and a test can ask for the lines without touching the process
/// environment.
pub fn tick_with(ctx: &Ctx, log: &Log, memory: &mut Memory, timings: &mut Timings) -> bool {
    let round_at = Instant::now();
    // One view of the session for the whole round: every project reads the same
    // `hc ls` and the same `hc get` per pane instead of one full listing per
    // Space it names (t-0091). Writes inside the round patch or drop it.
    crate::core::view::begin();
    // And one view of the window: `herdr-app status` and `ls`, once for every
    // project in the round instead of twice per coordinator.
    crate::app::begin();
    memory.tick += 1;
    timings.stage(ROUND, "rename");
    // Renames first: a project that moves is ticked under its new slug.
    for line in crate::rename::pending_pass(ctx) {
        log.line(&line);
    }
    let mut reachable = Vec::new();
    let mut unreachable = Vec::new();
    let mut sessions = Sessions::new(ctx);
    for slug in project::list_slugs(&ctx.root) {
        timings.stage(&slug, "load");
        let Ok(project) = Project::load(&ctx.root, &slug) else {
            continue;
        };
        match tick_cheap(ctx, &project, &mut sessions, timings) {
            Ok(Some(seen)) => {
                let held = (!seen.nudge_held.is_empty()).then(|| held_text(&seen.nudge_held));
                match (&held, memory.nudge_held.get(&slug)) {
                    (Some(held), previous) if previous.map(String::as_str) != Some(held.as_str()) => {
                        log.line(&format!("{slug}: nudge held: {held}"));
                        memory.nudge_held.insert(slug.clone(), held.clone());
                    }
                    (None, Some(previous)) => {
                        log.line(&format!("{slug}: nudge no longer held (was: {previous})"));
                        memory.nudge_held.remove(&slug);
                    }
                    _ => {}
                }
                for fixed in &seen.repaired {
                    log.line(&format!("{slug}: lane {}'s coordinator repaired on pane {}: {}", fixed.lane, fixed.pane, fixed.why));
                }
                for stop in &seen.interrupted {
                    log.line(&format!("{slug}: {}", steps::interrupt_note(stop.secs)));
                }
                reachable.push((project, seen))
            }
            Ok(None) => unreachable.push(project),
            Err(error) => log.line(&format!("{slug}: {error:#}")),
        }
    }
    let any_reachable = !reachable.is_empty() || sessions.any_reachable();
    for (project, seen) in &reachable {
        timings.stage(&project.slug, "slow");
        for error in tick_slow(ctx, project, seen, memory, log, timings) {
            log.line(&format!("{}: {error:#}", project.slug));
        }
    }
    // No coordinator: due routines are recorded as skipped, nothing fires.
    timings.stage(ROUND, "headless");
    for project in &unreachable {
        for error in headless_routines(project) {
            log.line(&format!("{}: {error:#}", project.slug));
        }
    }
    timings.stop();
    crate::core::view::end();
    crate::app::end();
    let took = round_at.elapsed();
    // Read before `drain`, which is what empties the record.
    let stages = timings.stage_totals();
    for line in timings.drain(Some(took)) {
        log.line(&line);
    }
    // The standing measurement: no `HP_TICK_TIMINGS`, one small write per round,
    // so `doctor` and `ticker status` can answer "how long is a round" and
    // "which stage took it" without anybody having planned ahead to ask.
    if let Err(error) = record_round(&ctx.root, took, stages) {
        log.line(&format!("the round's own time was not recorded: {error:#}"));
    }
    any_reachable
}

/// Between ticks: sends each thread whose agent the ticker started its brief
/// as soon as the agent is ready and settled, instead of a whole tick later.
/// An agent sitting idle that long looks stalled, and coordinators stepped in
/// with `thread brief`.
/// Returns whether a started thread still waits for its brief.
pub fn brief_pass(ctx: &Ctx, log: &Log) -> bool {
    crate::core::view::begin();
    let waiting = brief_pass_in(ctx, log);
    crate::core::view::end();
    waiting
}

fn brief_pass_in(ctx: &Ctx, log: &Log) -> bool {
    let mut waiting = false;
    let mut lists: std::collections::BTreeMap<String, Option<Vec<Agent>>> = Default::default();
    for slug in project::list_slugs(&ctx.root) {
        let Ok(project) = Project::load(&ctx.root, &slug) else {
            continue;
        };
        let pending: Vec<thread::Thread> = thread::list(&project)
            .into_iter()
            .filter(|t| t.status == thread::Status::Open && t.prompt_pending && !t.brief_stuck && t.launch_attempts > 0)
            .collect();
        let Some(record) = project.coordinator().filter(|r| !pending.is_empty() && !r.socket.is_empty()) else {
            continue;
        };
        let herdr = Core::new(ctx.env.hc_bin(), &record.socket, ctx.runner);
        for t in &pending {
            let Some(agents) = lists.entry(record.socket.clone()).or_insert_with(|| herdr.agent_list().ok()) else {
                continue;
            };
            let Some(agent) = agents.iter().find(|a| thread::agent_matches(t, a)) else {
                waiting = true;
                continue;
            };
            match crate::brief::deliver(&project, &herdr, t, agent, crate::brief::Sender::Ticker) {
                Ok(crate::brief::Outcome::Delivered | crate::brief::Outcome::Stuck) => {}
                Ok(_) => waiting = true,
                Err(error) => log.line(&format!("{slug}: {}: brief: {error:#}", t.id)),
            }
        }
    }
    waiting
}

/// One session's agent and pane lists.
type Lists = (Vec<Agent>, Vec<Pane>);

/// Herdr's agent and pane lists, asked for at most once per session per tick
/// and shared by every project in that session.
pub struct Sessions {
    lists: std::collections::BTreeMap<String, Option<Lists>>,
    /// The sockets a project without a live record is looked for in: the
    /// default session first, then every other project's recorded socket.
    candidates: Option<Vec<String>>,
    known: Vec<String>,
}

impl Sessions {
    pub fn new(ctx: &Ctx) -> Sessions {
        let mut known: Vec<String> = project::list_slugs(&ctx.root)
            .iter()
            .filter_map(|slug| Project::load(&ctx.root, slug).ok()?.coordinator())
            .map(|record| record.socket)
            .filter(|socket| !socket.is_empty())
            .collect();
        known.sort();
        known.dedup();
        Sessions { lists: Default::default(), candidates: None, known }
    }

    /// `None` when the socket is gone or the session does not answer.
    fn get(&mut self, ctx: &Ctx, socket: &str) -> Option<&Lists> {
        self.lists
            .entry(socket.to_string())
            .or_insert_with(|| {
                // A session is a core session name, empty for the default core;
                // the two calls below are what say whether it answers.
                let herdr = Core::new(ctx.env.hc_bin(), socket, ctx.runner);
                Some((herdr.agent_list().ok()?, herdr.pane_list().ok()?))
            })
            .as_ref()
    }

    /// The sockets to look in, the default session first.
    fn candidates(&mut self, ctx: &Ctx) -> Vec<String> {
        if let Some(found) = &self.candidates {
            return found.clone();
        }
        let mut found = Vec::new();
        if let Ok(session) = crate::paths::resolve_session(&Default::default(), ctx.env) {
            found.push(session.socket);
        }
        for socket in &self.known {
            if !found.contains(socket) {
                found.push(socket.clone());
            }
        }
        self.candidates = Some(found.clone());
        found
    }

    fn any_reachable(&self) -> bool {
        self.lists.values().any(Option::is_some)
    }
}

/// A project with no record, or one whose socket is gone: the first agent
/// working in the project folder, in the default session or any session
/// another project uses, becomes its coordinator and is recorded just as
/// `open` records one. Agents in `threads/` or a worktree are threads.
fn discover_record(ctx: &Ctx, project: &Project, sessions: &mut Sessions) -> Result<Option<project::Coordinator>> {
    for socket in sessions.candidates(ctx) {
        let Some((agents, _)) = sessions.get(ctx, &socket) else {
            continue;
        };
        if let Some(found) = coordinator::found(project, &socket, agents) {
            return project.update_coordinator(|c| *c = found).map(Some);
        }
    }
    Ok(None)
}

/// Due routines of a project whose session cannot be reached or that has no
/// coordinator: each is recorded as skipped, with no item.
fn headless_routines(project: &Project) -> Vec<anyhow::Error> {
    let mut state = steps::load_state(project);
    let before = state.clone();
    let mut errors = routine_pass(project, &mut state, false);
    if state != before {
        errors.extend(steps::save_state(project, &state).err());
    }
    errors
}

/// `coordinator`: the project has a live coordinator this tick.
fn routine_pass(project: &Project, state: &mut steps::State, coordinator: bool) -> Vec<anyhow::Error> {
    let zoned = jiff::Zoned::now();
    match project.read_project_md() {
        Ok(_) => steps::routines(project, state, coordinator, None, &zoned),
        Err(error) => {
            let text = std::fs::read(project.project_md()).unwrap_or_default();
            let problem = Some((thread::sha256_hex(&text), format!("{error:#}")));
            steps::routines(project, state, coordinator, problem, &zoned)
        }
    }
}

/// What the cheap pass saw, handed to the slow pass so herdr is asked once.
#[derive(Default)]
pub struct Seen {
    socket: String,
    agents: Vec<Agent>,
    panes: Vec<Pane>,
    /// Group changes of this tick, turned into inbox items after the copies.
    transitions: Vec<Transition>,
    /// Threads seen idle with a failed retry on their pane.
    stuck: Vec<crate::steps::Stuck>,
    /// At least one agent works in the project folder: routines may fire.
    coordinator: bool,
    /// The session answered, the project has at least two recorded local
    /// panes, and every one of them is missing: herdr was restarted.
    session_lost: bool,
    /// lane id -> why that lane had items to send but sent nothing this tick,
    /// for the ticker's log. Every lane is named, so a silent "no coordinator"
    /// verdict on one lane cannot hide behind another lane's delivery (§12
    /// commit 3, the shape of the 2026-10-18 stall).
    nudge_held: std::collections::BTreeMap<String, String>,
    /// lane id -> one line about a coordinator whose recorded terminal is gone
    /// and whose agent could not be found either, for the `coordinator` item.
    lost_coordinators: std::collections::BTreeMap<String, String>,
    /// The lanes the repair pass put right (a record moved to the pane its
    /// agent ran in, or a missing `hp_agent`/`hp_kind` written back).
    repaired: Vec<crate::coordinator::Fixed>,
    /// Every `Ambiguous` pane this tick with the resolver's reason, for the
    /// once-per-state `lane` item of §5.4.
    ambiguous: Vec<(String, String)>,
    /// Coordinator turns this tick stopped for running past
    /// `coordinator_turn_limit`, one log line each.
    interrupted: Vec<steps::Interrupted>,
}

/// The `nudge held` line's tail: every lane that could not deliver, with its
/// reason. One line per project, because a project's state is one state.
fn held_text(held: &std::collections::BTreeMap<String, String>) -> String {
    held.iter().map(|(lane, why)| format!("{lane}: {why}")).collect::<Vec<_>>().join("; ")
}

/// Both passes for one project; `Ok(false)` when its session is unreachable.
#[cfg(test)]
pub fn tick_project(ctx: &Ctx, project: &Project) -> Result<bool> {
    tick_project_with(ctx, project, &mut Memory::new(ctx))
}

#[cfg(test)]
pub fn tick_project_with(ctx: &Ctx, project: &Project, memory: &mut Memory) -> Result<bool> {
    let mut timings = Timings::off();
    match tick_cheap(ctx, project, &mut Sessions::new(ctx), &mut timings)? {
        Some(seen) => match tick_slow(ctx, project, &seen, memory, &Log::at(std::env::temp_dir().join("hp-test-tick.log")), &mut timings).into_iter().next() {
            Some(error) => Err(error),
            None => Ok(true),
        },
        None => match headless_routines(project).into_iter().next() {
            Some(error) => Err(error),
            None => Ok(false),
        },
    }
}

/// State, pending prompts, group and tokens for a set of threads that live in
/// the project's herdr session.
struct Pass {
    transitions: Vec<Transition>,
    /// Threads seen idle with a failed retry on their pane.
    stuck: Vec<crate::steps::Stuck>,
    recorded_panes: usize,
    missing_panes: usize,
    error: Option<anyhow::Error>,
}

fn thread_pass(project: &Project, herdr: &Core, socket: &str, threads: &[thread::Thread], agents: &[Agent], panes: &[Pane], hashes: Option<&std::collections::BTreeMap<String, String>>) -> Result<Pass> {
    let slug = &project.slug;
    let now = jiff::Timestamp::now();
    let mut pass = Pass { transitions: Vec::new(), stuck: Vec::new(), recorded_panes: 0, missing_panes: 0, error: None };
    for t in threads {
        if t.status == thread::Status::Starting {
            if thread::seconds_since(&t.updated, now) >= thread::STARTING_TIMEOUT_SECS {
                thread::update(project, &t.id, |t| {
                    if t.status == thread::Status::Starting
                        && thread::seconds_since(&t.updated, jiff::Timestamp::now()) >= thread::STARTING_TIMEOUT_SECS
                    {
                        t.status = thread::Status::Failed;
                        t.prompt_pending = false;
                        t.error = "still starting after five minutes".into();
                    }
                })?;
            }
            continue;
        }
        let mut live = thread::live_with_report(t, agents, panes, now, &project.root, socket);
        // Herdr's native resume starts our agent again without its name.
        if let Some(agent) = agents.iter().find(|a| thread::needs_rename(t, a))
            && let Err(error) = herdr.agent_rename(&agent.pane_id, &t.agent_name)
        {
            pass.error = pass.error.or(Some(anyhow::anyhow!("{}: rename: {error}", t.id)));
        }
        if !t.pane_id.is_empty() {
            pass.recorded_panes += 1;
            pass.missing_panes += usize::from(!live.pane_exists);
        }
        // A brief about to go to a trust screen Herdr reads as ready would
        // answer it: hold the brief and count the thread as blocked.
        let brief_due = t.prompt_pending && live.agent_state.as_deref().is_some_and(crate::core::ready_state);
        // An unreadable screen holds the brief too, without calling it blocked.
        let screen = brief_due.then(|| herdr.trust_screen(&t.pane_id, &t.agent));
        let hold_brief = !matches!(screen, None | Some(Ok(None)));
        if matches!(screen, Some(Ok(Some(_)))) {
            live.agent_state = Some("blocked".into());
            live.state_secs = if t.last_state == "blocked" { thread::seconds_since(&t.last_state_change, now) } else { 0 };
        }
        let state = live.agent_state.clone().unwrap_or_default();
        if state != t.last_state {
            live.state_secs = 0;
        }
        let mut delivered = false;
        if t.prompt_pending
            && !t.brief_stuck
            && !hold_brief
            && let Some(agent) = agents.iter().find(|a| thread::agent_matches(t, a))
        {
            match crate::brief::deliver(project, herdr, t, agent, crate::brief::Sender::Ticker) {
                Ok(outcome) => delivered = outcome == crate::brief::Outcome::Delivered,
                Err(error) => pass.error = pass.error.or(Some(anyhow::anyhow!("{}: brief: {error:#}", t.id))),
            }
        }

        // A report written this tick counts for the group at once; the copy
        // home follows. Otherwise a finished thread would show as Idle for one
        // tick before it shows as Ready for review.
        let fresh_hash = match hashes {
            Some(hashes) => hashes.get(&t.id).cloned(),
            None => thread::local_report_hash(t),
        };
        let report_hash = fresh_hash.unwrap_or_else(|| t.report_hash.clone());
        let after = thread::Thread { prompt_pending: t.prompt_pending && !delivered, report_hash, ..t.clone() };
        // In the tick that delivers a prompt the agent still reads as idle; it
        // has just been given work, so it is Working, not Idle.
        let group = if delivered { thread::Group::Working } else { thread::group(&after, &live, now) };
        if !t.last_group.is_empty() && group.token() != t.last_group {
            let note = if !live.pane_exists { "pane closed".to_string() } else if state.is_empty() { "no agent".to_string() } else { state.clone() };
            pass.transitions.push(Transition { id: t.id.clone(), to: group, note });
        }
        let line = crate::labels::state_line(group, &after, &live, now);
        let activity = live.self_report.as_ref().map(|r| r.activity.clone()).unwrap_or_default();
        let self_changed = activity != t.activity;
        if delivered || state != t.last_state || group.token() != t.last_group || line != t.state_line || self_changed {
            thread::update(project, &t.id, |t| {
                if delivered {
                    t.prompt_pending = false;
                }
                if state != t.last_state {
                    t.last_state = state.clone();
                    t.last_state_change = project::now();
                }
                t.last_group = group.token().to_string();
                t.state_line = line.clone();
                if self_changed {
                    t.activity = activity.clone();
                }
            })?;
        }
        if live.pane_exists {
            let tokens = panes.iter().find(|p| p.pane_id == t.pane_id).map(|p| &p.tokens);
            crate::labels::report_pane_unless(herdr, &t.pane_id, &crate::labels::thread_display(t), slug, None, tokens);
        }
        // A thread sitting idle with a failed retry on its pane has lost its
        // turn: no report is coming, so it is reported for the coordinator
        // instead.
        if live.pane_exists && state == "idle"
            && let Some(line) = crate::stuck::retry_failure(&herdr.agent_screen(&t.pane_id).unwrap_or_default())
        {
            pass.stuck.push(crate::steps::Stuck { id: t.id.clone(), pane: t.pane_id.clone(), line });
        }
    }
    Ok(pass)
}

/// Launches pending threads whose pane is at a shell prompt. At most one
/// `agent start` per tick (`may_start`), and never a start and a prompt for
/// the same pane in one tick: prompts only go to agents that were already
/// listed before any start.
fn launch_pass(ctx: &Ctx, project: &Project, seen: &Seen, threads: &[thread::Thread], may_start: &mut bool, errors: &mut Vec<anyhow::Error>) {
    let now = jiff::Timestamp::now();
    let herdr = Core::new(ctx.env.hc_bin(), &seen.socket, ctx.runner);
    let (agents, panes) = (&seen.agents, &seen.panes);
    for t in threads {
        if t.status != thread::Status::Open || !t.prompt_pending {
            continue;
        }
        let live = thread::live_state(t, agents, panes, now);
        if live.agent_state.is_some() || !live.pane_exists {
            continue;
        }
        if !*may_start {
            continue;
        }
        let t = match thread::claim_launch(project, &t.id) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        *may_start = false;
        let launched = (|| -> Result<()> {
            let safety = project.safety(&ctx.config_dir)?;
            let config = crate::profiles::load(&ctx.config_dir)?;
            let (settings, _) = project.read_project_md()?;
            let legacy = crate::profiles::legacy_agent(&config, &settings, crate::profiles::Role::Thread);
            let (kind, mut args) = if t.profile.is_empty() {
                // A thread started before profiles: the built-in of its kind
                // plus its stored model flag, which passes the same model-only
                // check as before. The refused ones are dropped for good.
                let (model, refused) = crate::agents::split_model_args(&t.agent, &t.agent_args);
                if !refused.is_empty() {
                    thread::update(project, &t.id, |t| t.agent_args = model.clone())?;
                    let summary = format!(
                        "{}: launched without agent arguments that are not a model flag: {}. Launch flags belong in a profile, which only the user sets (`hp profile list`)",
                        t.id,
                        refused.join(" "),
                    );
                    inbox::write_in(project, &inbox::lane_of_thread(project, &t.id), "thread-state", &t.id, "launched without some flags", &summary, "")?;
                }
                let builtin = config.get(&t.agent).filter(|p| p.builtin).map(|p| crate::profiles::launch_args(&p, &safety.thread_agent_args, &legacy)).unwrap_or_default();
                (t.agent.clone(), [builtin, model].concat())
            } else {
                // The profile is looked up and checked against the allow-list
                // again: one the user removed or disallowed since does not launch.
                let profile = match crate::profiles::resolve(&config, &safety, &settings, crate::profiles::Role::Thread, Some(&t.profile), &project.slug) {
                    Ok(profile) => profile,
                    Err(error) => {
                        let message = format!("{error:#}");
                        thread::update(project, &t.id, |t| {
                            t.status = thread::Status::Failed;
                            t.error = message.clone();
                        })?;
                        inbox::write_in(project, &inbox::lane_of_thread(project, &t.id), "thread-state", &t.id, "not launched", &format!("{}: not launched: {message}", t.id), "")?;
                        return Ok(());
                    }
                };
                if profile.agent() != t.agent {
                    let agent = profile.agent().to_string();
                    thread::update(project, &t.id, |t| t.agent = agent)?;
                }
                (profile.agent().to_string(), crate::profiles::launch_args(&profile, &safety.thread_agent_args, &legacy))
            };
            args = crate::profiles::expand_home(&args, &ctx.env.home);
            let args = safety.launch_args(&kind, &args);
            herdr.agent_start(&t.agent_name, &kind, &t.pane_id, &args)?;
            Ok(())
        })();
        errors.extend(launched.err().map(|e| e.context(format!("{}: launch", t.id))));
    }
}

pub(crate) fn open_threads(project: &Project) -> Vec<thread::Thread> {
    thread::list(project)
        .into_iter()
        .filter(|t| matches!(t.status, thread::Status::Open | thread::Status::Starting))
        .collect()
}

/// The tick's heartbeat, written for every active project before anything can
/// return early. `hp context` runs in another process, so the persisted state
/// is the only place the two meet: the time goes there and nowhere else.
fn tick_cheap(ctx: &Ctx, project: &Project, sessions: &mut Sessions, timings: &mut Timings) -> Result<Option<Seen>> {
    timings.stage(&project.slug, "state");
    let mut state = steps::load_state(project);
    let before = state.clone();
    state.tick_at = project::now();
    let result = tick_cheap_body(ctx, project, sessions, &mut state, timings);
    timings.stage(&project.slug, "save_state");
    let saved = if state == before { Ok(()) } else { steps::save_state(project, &state) };
    match result {
        Err(error) => Err(error),
        Ok(seen) => saved.map(|()| seen),
    }
}

/// Returns `Ok(None)` when the project's session cannot be reached, or it has
/// no record and no agent works in its folder: then no state is read, so
/// nothing is ever reported as gone.
fn tick_cheap_body(ctx: &Ctx, project: &Project, sessions: &mut Sessions, state: &mut steps::State, timings: &mut Timings) -> Result<Option<Seen>> {
    let recorded = project.coordinator();
    let record = match recorded {
        Some(record) => record,
        None => match discover_record(ctx, project, sessions)? {
            Some(record) => record,
            None => return Ok(None),
        },
    };
    timings.stage(&project.slug, "lists");
    let Some((mut agents, panes)) = sessions.get(ctx, &record.socket).cloned() else {
        return Ok(None);
    };
    let herdr = Core::new(ctx.env.hc_bin(), &record.socket, ctx.runner);
    let slug = &project.slug;
    let mut first_error = None;

    // Before anything is read from the records: a coordinator whose terminal
    // core has reaped while its agent still runs is moved to that pane, and one
    // that is gone outright is named. Without this the tick sees no coordinator
    // at all and holds every notification with the generic "not idle long
    // enough" (2026-11-18, t-0053).
    timings.stage(slug, "repair");
    let repair = coordinator::repair(project, &herdr, &agents, &project::now())?;
    if let Some(refreshed) = &repair.agents {
        // The list was read before the repair wrote a label: take core's word
        // for it now, so `discover` sees the repaired coordinator on this tick
        // and not the next one.
        agents = refreshed.clone();
    }
    // The record the repair rewrote is the one to read from here on: `discover`
    // asks it who is a coordinator, and the answer before the repair was "the
    // pane that is gone".
    let record = if repair.fixed.is_empty() { record } else { project.coordinator().unwrap_or(record) };

    // The coordinators: every agent in the project folder. Nothing is
    // launched or primed here; `open` starts them and AGENTS.md primes them.
    let previous = coordinator::live(project);
    let now_text = project::now();
    timings.stage(slug, "discover");
    let coordinators = coordinator::discover(project, &record, &previous, &agents, &now_text);
    if coordinators != previous {
        coordinator::save_live(project, &coordinators)?;
    }
    // §4.1 rule 2's hint: every open lane whose coordinator this tick resolved
    // gets its pane, native session and `last_active` refreshed, under the
    // project lock and as a read-modify-write so another lane's concurrent
    // update is never dropped.
    crate::lanes::refresh_instances(project, &coordinators, &now_text)?;
    let ambiguous = crate::lanes::ambiguous_panes(project, &record, &agents);
    // The primary pane's native session id, for a later resume by `open`.
    if let Some(primary) = coordinators.iter().find(|c| c.pane_id == record.pane_id)
        && !primary.agent_session.is_empty()
        && (primary.agent_session != record.agent_session || primary.agent != record.agent)
    {
        let (session_id, kind) = (primary.agent_session.clone(), primary.agent.clone());
        project.update_coordinator(|c| {
            c.agent_session = session_id;
            c.agent = kind;
        })?;
    }
    for c in &coordinators {
        // The lane claim, refreshed every tick from the registry, so a pane
        // whose shell drifted keeps saying which lane it is (§5.1, risk (b)).
        // A pane the resolver could not place keeps no claim at all: rule 4
        // never assigns an `Ambiguous` pane a lane.
        let lane = (!c.lane.is_empty()).then(|| c.lane.clone());
        timings.stage(slug, "coordinator_pane");
        let tokens = agents.iter().find(|a| a.pane_id == c.pane_id).map(|a| &a.tokens);
        crate::labels::report_pane_unless(&herdr, &c.pane_id, &crate::labels::coordinator_display(project, lane.as_deref()), slug, lane.as_deref(), tokens);
        // The names that pane's Space and tab must show, repaired here rather
        // than on demand: the ticker already visits every coordinator, so one
        // that was never named — a coordinator started in a terminal the app
        // made carries no `hp_ws` (t-0051) — is put right on the next tick.
        // `keep_names` prints a line only when a name did not land.
        timings.stage(slug, "keep_names");
        crate::coordinator::keep_names(&herdr, project, &c.pane_id);
    }

    // The coordinator's own turn, timed here and nowhere else: a coordinator
    // cannot see its own turn end, and this is the only process that looks at
    // the pane between turns. `hp context` prints it and the audit measures it
    // ([`crate::audit::COORDINATOR_LONG_TURN_SECS`]); one tick's resolution is
    // better than the turn's 19-minute blindness (2026-11-20).
    let working: Vec<(String, bool)> = coordinators.iter().map(|c| (c.pane_id.clone(), c.agent_status == "working")).collect();
    let live_panes: Vec<String> = panes.iter().map(|p| p.pane_id.clone()).collect();
    steps::track_coordinator_turns(state, &working, &live_panes, jiff::Timestamp::now());

    // A turn past the project's limit is stopped here, on the same measurement
    // the audit warns about at ten minutes (`PROJECT.md`'s
    // `coordinator_turn_limit`, twenty minutes by default). The warning alone
    // cannot reach a coordinator that is inside the turn it is about — the nudge
    // waits for an empty input box in a quiet pane — so a runaway turn used to
    // need a person to press Escape (2026-11-21, rexa: 41 minutes of ssh retries
    // in one turn).
    let mut interrupted = Vec::new();
    timings.stage(slug, "turns");
    if let Ok((settings, _)) = project.read_project_md() {
        let stops = steps::interrupt_long_turns(project, state, &settings, &herdr, &coordinators, jiff::Timestamp::now());
        interrupted = stops.stopped;
        for error in stops.errors {
            first_error.get_or_insert(error);
        }
    }

    timings.stage(slug, "thread_list");
    let open = open_threads(project);
    timings.stage(slug, "threads");
    let pass = thread_pass(project, &herdr, &record.socket, &open, &agents, &panes, None)?;
    // Progress records of panes that are gone are dropped; a new agent in a
    // reused pane id is told apart by its terminal id.
    let live_ids: Vec<String> = panes.iter().map(|p| p.pane_id.clone()).collect();
    crate::progress::prune(&ctx.root, &record.socket, &live_ids);
    first_error = first_error.or(pass.error);
    let coordinator_recorded = usize::from(!record.pane_id.is_empty());
    let coordinator_missing = usize::from(coordinator_recorded == 1 && coordinators.is_empty() && !panes.iter().any(|p| coordinator::pane_matches(&record, p)));
    let recorded_panes = pass.recorded_panes + coordinator_recorded;
    let missing_panes = pass.missing_panes + coordinator_missing;

    // Nudge about inbox items `context` has not shown yet, once
    // per lane: each lane's coordinator hears only its own lane's items, and
    // one lane's announcement is never spent on another's (§4.4, §12 commit
    // 3). Without a registry there is exactly one lane, so this is the single
    // nudge run it always was. What the decision was — and when it started
    // being that — is recorded per lane, because the ticker's log is not what
    // a coordinator reads: `hp context` is (see `steps::held_note`).
    let mut nudge_held: std::collections::BTreeMap<String, String> = Default::default();
    timings.stage(slug, "nudge");
    if let Ok((settings, _)) = project.read_project_md() {
        let now = jiff::Timestamp::now();
        let targets = coordinator::nudge_target(project, &coordinators, now);
        for lane in crate::lanes::routes(project) {
            // A lane with no idle coordinator gets no target, which is a held
            // reason rather than a delivery — an `Ambiguous` pane never has a
            // lane here, so it is never nudged.
            let target = targets.get(&lane).copied();
            match steps::nudge(project, state, &settings, &herdr, &lane, target, now) {
                Ok(steps::NudgeOutcome::Delivered) | Ok(steps::NudgeOutcome::Quiet) => state.released(&lane),
                Ok(steps::NudgeOutcome::Held(why)) => {
                    // "No coordinator is idle enough" is the wrong reason when
                    // the coordinator's terminal is gone: nothing will ever
                    // become idle. Name it, so the digest says what to do.
                    let why = if why == steps::NO_COORDINATOR && repair.lost.contains_key(&lane) { steps::NO_COORDINATOR_GONE } else { why };
                    nudge_held.insert(lane.clone(), why.to_string());
                    state.hold(&lane, why, now);
                }
                Err(error) => {
                    first_error = first_error.or(Some(error.context(format!("nudge {lane}"))));
                }
            }
        }
    }

    match first_error {
        Some(error) => Err(error),
        None => Ok(Some(Seen {
            socket: record.socket,
            agents,
            panes,
            transitions: pass.transitions,
            stuck: pass.stuck,
            coordinator: !coordinators.is_empty(),
            session_lost: recorded_panes >= 2 && missing_panes == recorded_panes,
            nudge_held,
            interrupted,
            lost_coordinators: repair.lost,
            repaired: repair.fixed,
            ambiguous,
        })),
    }
}

/// Copies and launches, then inbox items, pull requests, routines,
/// auto-resolve and housekeeping.
fn tick_slow(ctx: &Ctx, project: &Project, seen: &Seen, memory: &mut Memory, log: &Log, timings: &mut Timings) -> Vec<anyhow::Error> {
    let slug = &project.slug;
    timings.stage(slug, "slow_load");
    let mut errors = Vec::new();
    let mut copy_notes: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    let herdr = Core::new(ctx.env.hc_bin(), &seen.socket, ctx.runner);
    let now = jiff::Timestamp::now();
    let mut may_start = true;
    let transitions = &seen.transitions;

    // Local threads: copy home when the report changed, then launches.
    timings.stage(slug, "slow_thread_list");
    let local = open_threads(project);
    timings.stage(slug, "copy");
    // One-time backfill for worktrees placed before `skills::prepare` wrote the
    // shared `.agents` link into `info/exclude`: without it a direct
    // `herdr worktree remove` reads the link as untracked work and refuses
    // (`dirty_worktree_requires_force`). Recorded, so each thread pays for
    // this check once, not every tick.
    for t in local.iter().filter(|t| t.status == thread::Status::Open && t.kind == thread::Kind::Worktree && !t.worktree_path.is_empty() && !t.agents_excluded) {
        if crate::skills::exclude_shared_link_if_present(Path::new(&t.repo), Path::new(&t.worktree_path)).is_ok()
            && let Err(error) = thread::update(project, &t.id, |t| t.agents_excluded = true)
        {
            errors.push(error.context(format!("{}: .agents exclude backfill", t.id)));
        }
    }
    for t in local.iter().filter(|t| t.status == thread::Status::Open) {
        if let Some(hash) = thread::local_report_hash(t)
            && hash != t.report_hash
        {
            let copied = thread::copy_home_local(project, t, true, ctx.runner);
            match copied.outcome {
                thread::CopyOutcome::Failed(error) => errors.push(anyhow::anyhow!("{}: copy failed: {error}", t.id)),
                outcome => {
                    if let thread::CopyOutcome::Partial(notes) = outcome {
                        copy_notes.insert(t.id.clone(), notes);
                    }
                    let updated = thread::update(project, &t.id, |t| {
                        t.report_hash = hash.clone();
                        t.last_report_change = project::now();
                    });
                    errors.extend(updated.err());
                }
            }
        }
    }
    timings.stage(slug, "launch");
    launch_pass(ctx, project, seen, &local, &mut may_start, &mut errors);

    timings.stage(slug, "slow_state");
    let mut state = steps::load_state(project);
    let before = state.clone();

    timings.stage(slug, "items");
    errors.extend(steps::write_thread_items(project, &mut state, transitions, seen.session_lost, &copy_notes).err());
    errors.extend(steps::write_stuck_items(project, &mut state, &seen.stuck).err());
    errors.extend(steps::write_idle_notes(project, &mut state, now).err());
    // The registry audit, on its own clock (`steps::AUDIT_EVERY_SECS`) instead
    // of on the sweep routine's: that prompt is suppressed while a project is
    // quiet, which is exactly when a resolved thread that never stopped running
    // goes unnoticed. It runs before the routines so a new finding is already
    // in the inbox when this tick decides whether a sweep has news. The lists
    // are the ones this tick already made.
    timings.stage(slug, "audit");
    match steps::audit_items(project, &mut state, &herdr, &seen.socket, now, &seen.agents, &seen.panes) {
        Ok(written) if !written.is_empty() => {
            let what: Vec<String> = written.iter().map(|f| format!("{} {}", f.subject(), f.kind)).collect();
            log.line(&format!("{}: audit wrote {} item(s): {}", project.slug, written.len(), what.join(", ")));
        }
        Ok(_) => {}
        Err(error) => errors.push(error.context("audit")),
    }
    // §7: an open lane with live threads and no live coordinator gets one
    // `lane` item, addressed to the default lane, deduplicated on its thread
    // count. A project without a registry has no lane that can be orphaned.
    errors.extend(steps::write_lane_items(project, &mut state, &crate::lanes::orphan_lanes(project, &seen.agents)).err());
    // §5.4: every `Ambiguous` pane gets one `lane` item, so a project whose
    // inspector is not open still reports the refusal somewhere visible.
    errors.extend(steps::write_ambiguous_lane_items(project, &mut state, &seen.ambiguous).err());
    // A coordinator whose recorded terminal is gone and whose agent could not
    // be found anywhere: one `coordinator` item per lane, so a project nobody is
    // watching still says that no prompt can reach it. Nothing restarts it.
    errors.extend(steps::write_coordinator_lost_items(project, &mut state, &seen.lost_coordinators).err());
    timings.stage(slug, "pr");
    errors.extend(steps::pull_requests(ctx, project, &mut state, memory, now));
    timings.stage(slug, "resolve");
    errors.extend(steps::resolve_merged(ctx, project, &mut state, now));
    timings.stage(slug, "routines");
    errors.extend(routine_pass(project, &mut state, seen.coordinator));
    if let Ok((settings, _)) = project.read_project_md() {
        // First the close a reader is likelier to be surprised by (a thread
        // that produced nothing, resolved after `auto_resolve_nopr`), then the
        // week-long idle rule.
        let nopr = steps::auto_resolve_nopr(ctx, project, &settings, &mut state, now);
        if !nopr.resolved.is_empty() {
            log.line(&format!(
                "{}: resolved {} automatically (no pull request, no commit of its own, its report unchanged for {}): `auto_resolve_nopr`",
                project.slug,
                nopr.resolved.join(", "),
                settings.auto_resolve_nopr
            ));
        }
        errors.extend(nopr.errors);
        for line in &nopr.branches {
            log.line(&format!("{}: {line}", project.slug));
        }
        errors.extend(steps::auto_resolve(ctx, project, &settings, memory, now));
    }
    // Backstop for inbox items whose thread is long resolved: they can no
    // longer be acted on, and unhandled ones are re-announced forever.
    timings.stage(slug, "housekeeping");
    match steps::archive_items_of_resolved_threads(project, now) {
        Ok(archived) if !archived.is_empty() => {
            let mut per_thread: std::collections::BTreeMap<&str, usize> = Default::default();
            for (_, subject) in &archived {
                *per_thread.entry(subject.as_str()).or_default() += 1;
            }
            let what: Vec<String> = per_thread.iter().map(|(id, n)| if *n > 1 { format!("{id} x{n}") } else { id.to_string() }).collect();
            log.line(&format!("{}: archived {} item(s) about resolved threads: {}", project.slug, archived.len(), what.join(", ")));
        }
        Ok(_) => {}
        Err(error) => errors.push(error.context("archiving items of resolved threads")),
    }
    // Orphan home Spaces and the thread Space once empty. The sweep decides
    // whether to close a Space from the session's listing, so it is given one no
    // older than a round: the held view is dropped when it predates this.
    crate::core::view::refresh_if_older(VIEW_FRESH);
    errors.extend(crate::spaces::close_empty(ctx, project, &herdr));
    // The home Space's label and the thread Space's follow the project's name
    // without an `open`.
    crate::spaces::sync_home_label(project, &herdr);
    crate::spaces::sync_threads_label(project, &herdr);
    inbox::prune_done(project, steps::DONE_RETENTION_DAYS);
    if state != before {
        errors.extend(steps::save_state(project, &state).err());
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Env;
    use crate::runner::fake::{FakeRunner, fail, ok};

    fn held(version: &str) -> LockState {
        LockState::Held(Info {
            version: version.into(),
            ..Info::default()
        })
    }

    /// `HP_TICK_TIMINGS`: one line per project and one for the round when it is
    /// on, nothing at all when it is off — the ticker's log stays byte for byte
    /// what it was.
    #[test]
    fn timings_say_where_a_round_went_and_nothing_when_off() {
        let mut on = Timings::on();
        on.stage("demo", "lists");
        std::thread::sleep(std::time::Duration::from_millis(2));
        on.stage("demo", "threads");
        on.stage("demo", "housekeeping");
        on.stage(ROUND, "headless");
        on.stop();
        assert!(on.total("demo") >= std::time::Duration::from_millis(2), "the stages add up");
        let lines = on.drain(Some(std::time::Duration::from_millis(42)));
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("timings demo: "), "{}", lines[0]);
        for stage in ["lists ", "threads ", "housekeeping "] {
            assert!(lines[0].contains(stage), "{} is in {}", stage, lines[0]);
        }
        assert_eq!(lines[1], "timings round: 42ms");
        assert!(lines[0].contains("lists 2") || lines[0].contains("lists 3"), "the millisecond count is real: {}", lines[0]);

        let mut off = Timings::off();
        off.stage("demo", "lists");
        off.stop();
        assert!(off.drain(Some(std::time::Duration::from_millis(42))).is_empty(), "switched off writes nothing");
        // The switch is the environment, and only a real value turns it on.
        assert!(!timings_from_env() || std::env::var(TIMINGS_VAR).is_ok());
    }

    #[test]
    fn start_decisions() {
        assert_eq!(decide_start(&LockState::Free, "v1", false), StartAction::Spawn);
        assert_eq!(decide_start(&LockState::Free, "v1", true), StartAction::Spawn);
        assert_eq!(decide_start(&held("v1"), "v1", false), StartAction::Nothing);
        assert_eq!(decide_start(&held("v0"), "v1", false), StartAction::StopThenSpawn);
        // A stop in progress: finish it, then spawn.
        assert_eq!(decide_start(&held("v1"), "v1", true), StartAction::StopThenSpawn);
    }

    #[test]
    fn start_and_run_create_nothing_without_projects() {
        let home = tempfile::tempdir().unwrap();
        let missing = home.path().join("root");
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        let ctx = Ctx { env: &env, root: missing.clone(), config_dir: home.path().join("cfg"), runner: &runner, detached_ticker: true };
        start(&ctx).unwrap();
        assert!(!missing.exists());
        run(&ctx).unwrap();
        assert!(!missing.exists());

        std::fs::create_dir(&missing).unwrap();
        start(&ctx).unwrap();
        run(&ctx).unwrap();
        // The run logs why it exits and takes no lock; nothing else is created.
        let mut entries: Vec<String> = std::fs::read_dir(&missing).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        entries.sort();
        assert_eq!(entries, [".ticker.log"]);
        let log = std::fs::read_to_string(missing.join(".ticker.log")).unwrap();
        assert!(log.contains("no projects under"), "{log}");
    }

    #[test]
    fn the_launch_agent_is_there_to_start_the_ticker_after_any_exit() {
        let plist = agent_plist(Path::new("/opt/hp/hp"), Path::new("/Users/me/.herdr-projects"), Path::new("/Users/me/.herdr-projects/.ticker.log"));
        // A clean exit is restarted too: `SuccessfulExit` was why the episode
        // of 2026-11-17 stayed dead.
        assert!(plist.contains("<key>KeepAlive</key><true/>"), "{plist}");
        assert!(!plist.contains("SuccessfulExit"), "{plist}");
        assert!(plist.contains("<key>RunAtLoad</key><true/>"), "{plist}");
        assert!(plist.contains("<string>/opt/hp/hp</string>"), "{plist}");
        assert!(plist.contains("<string>--root</string>") && plist.contains("<string>ticker</string>") && plist.contains("<string>run</string>"), "{plist}");
        assert!(plist.contains("<key>StandardOutPath</key><string>/Users/me/.herdr-projects/.ticker.log</string>"), "{plist}");
        // Not `Background`: launchd throttles that policy, and every process
        // the loop starts inherits it. Measured 2026-11-23 on one binary and
        // one root, a round was 9–25 s under `Background` and 0.9–1.5 s under
        // `Standard` (t-0095) — this line is the whole difference.
        assert!(plist.contains("<key>ProcessType</key><string>Standard</string>"), "{plist}");
        assert!(!plist.contains("Background"), "{plist}");
    }

    #[test]
    fn installing_the_launch_agent_rewrites_only_what_is_out_of_date() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        let runner = FakeRunner::new();
        runner.on("launchctl print", ok(""));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));
        let binary = Path::new("/opt/hp/hp");
        let plist = plist_in(home.path(), &root);

        install_agent(home.path(), binary, &root, &runner).unwrap();
        assert!(std::fs::read_to_string(&plist).unwrap().contains("<key>KeepAlive</key><true/>"));
        assert_eq!(runner.count("launchctl bootstrap"), 1);

        // Already this text and already loaded: nothing is rewritten or bounced.
        install_agent(home.path(), binary, &root, &runner).unwrap();
        assert_eq!(runner.count("launchctl bootout"), 1);
        assert_eq!(runner.count("launchctl bootstrap"), 1);

        // The old shape, as the accident left it: rewritten and reloaded.
        std::fs::write(&plist, "<plist><dict><key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict></dict></plist>").unwrap();
        install_agent(home.path(), binary, &root, &runner).unwrap();
        assert!(std::fs::read_to_string(&plist).unwrap().contains("<key>KeepAlive</key><true/>"));
        assert_eq!(runner.count("launchctl bootout"), 2);
        assert_eq!(runner.count("launchctl bootstrap"), 2);
    }

    /// A ticker process: it holds the root's lock, which is the only thing that
    /// says a loop is running. Held in the test's own thread, so the probe in
    /// [`load_agent`] cannot miss it.
    fn hold_lock(root: &Path) -> File {
        let file = File::options().create(true).truncate(false).read(true).write(true).open(lock_path(root)).unwrap();
        file.lock().unwrap();
        file
    }

    fn plist_in(home: &Path, root: &Path) -> PathBuf {
        agent_path(home, root)
    }

    fn write_plist(home: &Path, root: &Path, text: &str) {
        let path = plist_in(home, root);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
    }

    /// The 2026-11-19 accident, part one: the plist was rewritten at 18:21 and
    /// the ticker it was keeping alive died of SIGTERM in the same second. A
    /// rewrite is not a reason to bounce a loop that is running.
    #[test]
    fn a_running_ticker_is_not_bounced_to_refresh_the_plist() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        std::fs::create_dir(&root).unwrap();
        write_plist(home.path(), &root, "<plist><dict><key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict></dict></plist>");
        let runner = FakeRunner::new();
        runner.on("launchctl print", ok(""));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));
        let lock = hold_lock(&root);

        let report = install_agent(home.path(), Path::new("/opt/hp/hp"), &root, &runner).unwrap();
        assert!(report.wrote_plist, "the out of date plist is still rewritten");
        assert!(report.kept_running, "{report:?}");
        assert_eq!(runner.count("launchctl bootout"), 0, "a running ticker must not be unloaded to refresh a plist");
        assert_eq!(runner.count("launchctl bootstrap"), 0);
        assert!(std::fs::read_to_string(plist_in(home.path(), &root)).unwrap().contains("<key>KeepAlive</key><true/>"));
        let log = std::fs::read_to_string(root.join(".ticker.log")).unwrap_or_default();
        assert!(log.contains("left it running"), "{log}");
        drop(lock);
    }

    /// `launchctl print` failing is not proof that no job is loaded (launchd
    /// answers an I/O error too), so a job nobody has loaded is loaded with
    /// `bootstrap` alone: `bootout` first is what killed the ticker.
    #[test]
    fn loading_a_job_launchd_does_not_have_unloads_nothing_first() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let runner = FakeRunner::new();
        runner.on("launchctl print", fail(113, "Could not find service"));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));

        let report = install_agent(home.path(), Path::new("/opt/hp/hp"), &root, &runner).unwrap();
        assert!(report.bootstrapped, "{report:?}");
        assert!(!report.bounced, "{report:?}");
        assert_eq!(runner.count("launchctl bootout"), 0, "nothing was loaded, so there is nothing to unload");
        assert_eq!(runner.count("launchctl bootstrap"), 1);
    }

    /// Part two: a `bootstrap` that fails leaves no job and no process. The
    /// reason used to reach only the stdout of the command that happened to be
    /// running; the plist launchd refused is put back so the previous state is
    /// the one on disk.
    #[test]
    fn a_refused_bootstrap_is_logged_and_the_old_plist_put_back() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let old = "<plist><dict><key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict></dict></plist>";
        write_plist(home.path(), &root, old);
        let runner = FakeRunner::new();
        runner.on("launchctl print", ok(""));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", fail(5, "Bootstrap failed: 5: Input/output error"));

        let error = install_agent(home.path(), Path::new("/opt/hp/hp"), &root, &runner).unwrap_err();
        assert!(format!("{error:#}").contains("Input/output error"), "{error:#}");
        let log = std::fs::read_to_string(root.join(".ticker.log")).unwrap_or_default();
        assert!(log.contains("launchctl bootstrap failed: Bootstrap failed: 5: Input/output error"), "{log}");
        assert_eq!(std::fs::read_to_string(plist_in(home.path(), &root)).unwrap(), old, "the plist launchd refused is not left behind");
    }

    /// The repair the accident asks for: `hp ticker status` is the command a
    /// person runs when nothing is announced, so it is also the one that puts a
    /// written-but-unloaded plist back to work.
    #[test]
    fn ticker_status_loads_a_written_plist_launchd_does_not_have() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let binary = crate::paths::binary().unwrap();
        write_plist(home.path(), &root, &agent_plist(&binary, &root, &log_path(&root)));
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        runner.on("launchctl print", fail(113, "Could not find service"));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: home.path().join("cfg"), runner: &runner, detached_ticker: true };

        let line = status_agent(&ctx, Duration::from_millis(100));
        assert_eq!(runner.count("launchctl bootstrap"), 1, "the job nobody loaded is loaded: {line:?}");
        assert_eq!(runner.count("launchctl bootout"), 0, "nothing held the lock, so nothing needed unloading");
        assert!(line.contains("hp ticker start"), "and it says what is still missing: {line:?}");
    }

    /// The same command on a healthy machine says so and touches no job.
    #[test]
    fn ticker_status_leaves_a_healthy_agent_alone() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let binary = crate::paths::binary().unwrap();
        write_plist(home.path(), &root, &agent_plist(&binary, &root, &log_path(&root)));
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        runner.on("launchctl print", ok(""));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: home.path().join("cfg"), runner: &runner, detached_ticker: true };
        let lock = hold_lock(&root);

        let line = status_agent(&ctx, Duration::from_millis(100));
        assert!(line.contains("is loaded"), "{line:?}");
        assert_eq!(runner.count("launchctl bootout"), 0);
        assert_eq!(runner.count("launchctl bootstrap"), 0);
        drop(lock);
    }

    /// The plist an accident leaves behind names the binary it points at: a
    /// build in a worktree rewriting the global plist is what a person needs to
    /// see, not just "out of date".
    #[test]
    fn an_out_of_date_plist_says_which_program_it_runs() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let elsewhere = Path::new("/Users/me/.herdr/worktrees/x/target/release/hp");
        write_plist(home.path(), &root, &agent_plist(elsewhere, &root, &log_path(&root)));
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        runner.on("launchctl print", ok(""));
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: home.path().join("cfg"), runner: &runner, detached_ticker: true };
        let lock = hold_lock(&root);

        let line = status_agent(&ctx, Duration::from_millis(100));
        assert!(line.contains("it runs /Users/me/.herdr/worktrees/x/target/release/hp"), "{line}");
        assert_eq!(runner.count("launchctl bootout"), 0);
        drop(lock);
    }

    /// Two `hp` commands at once used to interleave their unload and load, and
    /// the loser's `bootstrap` was the one that failed. The pair is serialized
    /// on a lock under the root, so the second call sees the first one's result.
    #[test]
    fn the_unload_and_load_pair_is_serialized_across_processes() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let guard = File::options().create(true).truncate(false).read(true).write(true).open(agent_lock_path(&root)).unwrap();
        guard.lock().unwrap();
        let home_path = home.path().to_path_buf();
        let root_path = root.clone();
        let (done, wait) = std::sync::mpsc::channel();
        let spawned = std::thread::spawn(move || {
            let runner = FakeRunner::new();
            runner.on("launchctl print", fail(113, "Could not find service"));
            runner.on("launchctl bootstrap", ok(""));
            let result = install_agent(&home_path, Path::new("/opt/hp/hp"), &root_path, &runner).map(|report| report.bootstrapped);
            done.send(result).unwrap();
        });
        // While the other holder is inside, nothing is written and no launchctl
        // runs: the pair cannot be split by a second process.
        assert!(wait.recv_timeout(Duration::from_millis(300)).is_err(), "install did not wait for the other process");
        assert!(!plist_in(home.path(), &root).exists());
        drop(guard);
        assert!(wait.recv_timeout(Duration::from_secs(10)).unwrap().unwrap());
        assert!(plist_in(home.path(), &root).exists());
        spawned.join().unwrap();
    }

    #[test]
    fn a_version_change_leaves_a_launch_agent_behind() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        project::create(&root, "demo", vec![]).unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        runner.on("launchctl print", fail(113, "Could not find service"));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: home.path().join("cfg"), runner: &runner, detached_ticker: true };

        // A ticker of another version holds the lock and lets go when it sees
        // the stop file: the stop-then-spawn path, which used to spawn a plain
        // process and leave no LaunchAgent at all (the 2026-11-17 shape).
        let holder = File::options().create(true).truncate(false).read(true).write(true).open(lock_path(&root)).unwrap();
        holder.lock().unwrap();
        let watched = root.clone();
        let releaser = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            while Instant::now() < deadline && !stop_path(&watched).exists() {
                std::thread::sleep(Duration::from_millis(20));
            }
            drop(holder);
        });
        let started = std::cell::Cell::new(0);
        start_with(&ctx, Duration::from_millis(100), &|_| {
            started.set(started.get() + 1);
            Ok(())
        })
        .unwrap();
        releaser.join().unwrap();

        let plist = plist_in(home.path(), &root);
        let text = std::fs::read_to_string(&plist).unwrap_or_default();
        assert!(text.contains("<key>KeepAlive</key><true/>"), "no LaunchAgent written: {text}");
        assert_eq!(runner.count("launchctl bootstrap"), 1);
        // Nothing took the lock (the fake launchctl starts no process), so the
        // fallback ran: start never reports success with no process.
        assert_eq!(started.get(), 1);
    }

    /// A stand-in for a ticker process: it takes the root's lock a moment
    /// later, then holds it long enough to be seen.
    fn pretend_ticker(root: &Path) -> std::thread::JoinHandle<()> {
        let path = lock_path(root);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let file = File::options().create(true).truncate(false).read(true).write(true).open(&path).unwrap();
            file.lock().unwrap();
            std::thread::sleep(Duration::from_secs(3));
            drop(file);
        })
    }

    #[test]
    fn a_loaded_agent_that_starts_nothing_falls_back_to_a_plain_process() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        project::create(&root, "demo", vec![]).unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        runner.on("launchctl print", ok(""));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: home.path().join("cfg"), runner: &runner, detached_ticker: true };
        let started = std::cell::Cell::new(0);
        // `launchctl` says the job is loaded, but no loop took the lock: the
        // agent alone is not "the ticker is running".
        launch_with(&ctx, Duration::from_millis(100), &|_| {
            started.set(started.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(runner.count("launchctl bootstrap"), 1, "the LaunchAgent was written and loaded");
        assert_eq!(started.get(), 1, "nothing took the lock, so a process is started directly");
    }

    /// The acceptance check for the 2026-11-19 accident, in one test: a command
    /// that ensures the ticker runs (`hp thread start` goes through here) does
    /// nothing at all to a ticker that is running — no unload, no load, no spawn,
    /// and nothing new in `.ticker.log`.
    #[test]
    fn a_healthy_ticker_is_not_touched_by_a_command_that_ensures_it() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        project::create(&root, "demo", vec![]).unwrap();
        let binary = crate::paths::binary().unwrap();
        write_plist(home.path(), &root, &agent_plist(&binary, &root, &log_path(&root)));
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        runner.on("launchctl print", ok(""));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: home.path().join("cfg"), runner: &runner, detached_ticker: true };
        let lock = hold_lock(&root);

        launch_with(&ctx, Duration::from_secs(5), &|_| panic!("a healthy ticker was replaced by a plain process")).unwrap();
        assert_eq!(runner.count("launchctl bootout"), 0, "the healthy job was unloaded");
        assert_eq!(runner.count("launchctl bootstrap"), 0);
        assert!(!root.join(".ticker.log").exists(), "a command that found everything right logged something");
        drop(lock);
    }

    #[test]
    fn a_loaded_agent_whose_ticker_took_the_lock_is_not_started_again() {
        if !cfg!(target_os = "macos") {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        project::create(&root, "demo", vec![]).unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        runner.on("launchctl print", ok(""));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: home.path().join("cfg"), runner: &runner, detached_ticker: true };
        let ticker = pretend_ticker(&root);
        let started = std::cell::Cell::new(0);
        launch_with(&ctx, Duration::from_secs(5), &|_| {
            started.set(started.get() + 1);
            Ok(())
        })
        .unwrap();
        assert_eq!(started.get(), 0);
        ticker.join().unwrap();
    }

    #[test]
    fn the_stop_file_path_is_logged() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        project::create(&root, "demo", vec![]).unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: home.path().join("cfg"), runner: &runner, detached_ticker: false };
        std::fs::write(stop_path(&root), b"").unwrap();
        let log = Log { path: root.join(".ticker.log") };
        run_logged(&ctx, &log).unwrap();
        let text = std::fs::read_to_string(&log.path).unwrap();
        assert!(text.contains("stop file found; exiting"), "{text}");
    }

    #[test]
    fn a_panic_is_written_to_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        // The installed hook chains to whatever was there: a panic still prints
        // to standard error as before, and lands in the ticker's log.
        install_panic_hook(path.clone());
        assert!(std::panic::catch_unwind(|| panic!("boom in the ticker")).is_err());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("ticker panicked"), "{text}");
        assert!(text.contains("boom in the ticker"), "{text}");
    }

    #[test]
    fn a_second_ticker_says_why_it_steps_aside() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        project::create(&root, "demo", vec![]).unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: home.path().join("cfg"), runner: &runner, detached_ticker: false };
        let held = File::options().create(true).truncate(false).read(true).write(true).open(lock_path(&root)).unwrap();
        held.lock().unwrap();
        run(&ctx).unwrap();
        let log = std::fs::read_to_string(root.join(".ticker.log")).unwrap();
        assert!(log.contains("another ticker holds the lock"), "{log}");
    }

    #[test]
    fn lock_probe_sees_a_holder_and_its_version() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(lock_state(root.path()), LockState::Free);
        let mut file = File::options().create(true).write(true).truncate(false).open(lock_path(root.path())).unwrap();
        file.lock().unwrap();
        file.write_all(br#"{"version":"v9","pid":1}"#).unwrap();
        match lock_state(root.path()) {
            LockState::Held(info) => assert_eq!(info.version, "v9"),
            LockState::Free => panic!("lock should be held"),
        }
        drop(file);
        // Another test may fork a child at this instant; until that child execs,
        // it shares the locked descriptor. Real callers poll too (`ticker stop`).
        let deadline = Instant::now() + Duration::from_secs(2);
        while lock_state(root.path()) != LockState::Free && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(lock_state(root.path()), LockState::Free);
    }

    #[test]
    fn stop_with_a_free_lock_removes_a_stale_stop_file() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(stop_path(&root), b"").unwrap();
        // A temporary HOME and a fake runner: the real machine's LaunchAgent is
        // never unloaded by a test.
        let env = Env::for_test(home.path(), &[]);
        let runner = FakeRunner::new();
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: home.path().join("cfg"), runner: &runner, detached_ticker: false };
        stop(&ctx).unwrap();
        assert!(!stop_path(&root).exists());
    }

    const NO_AGENTS: &str = r#"{"result":{"agents":[]}}"#;
    const PANE: &str = r#"{"result":{"panes":[{"pane_id":"w1:p1","tab_id":"w1:t1","workspace_id":"w1","cwd":"CWD"}]}}"#;

    struct Fixture {
        _home: tempfile::TempDir,
        env: Env,
        root: PathBuf,
        project: Project,
    }

    fn fixture() -> Fixture {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        let project = project::create(&root, "demo", vec![]).unwrap();
        let socket = home.path().join("herdr.sock");
        std::fs::write(&socket, b"").unwrap();
        let cwd = project.dir().to_string_lossy().into_owned();
        project
            .update_coordinator(|c| {
                c.socket = socket.to_string_lossy().into_owned();
                c.workspace_id = "w1".into();
                c.tab_id = "w1:t1".into();
                c.pane_id = "w1:p1".into();
                c.agent_name = "hpc-demo".into();
                c.cwd = cwd;
                c.agent = "claude".into();
            })
            .unwrap();
        let env = Env::for_test(home.path(), &[]);
        Fixture { _home: home, env, root, project }
    }

    fn with_cwd(json: &str, fixture: &Fixture) -> String {
        json.replace("CWD", &fixture.project.dir().to_string_lossy())
    }

    #[test]
    fn a_stale_ticker_snapshot_cannot_launch_across_a_restart_barrier() {
        let f = fixture();
        let runner = FakeRunner::new();
        let worktree = f._home.path().join("worktree");
        std::fs::create_dir(&worktree).unwrap();
        let pending = thread::allocate(&f.project, |t| {
            t.status = thread::Status::Open;
            t.prompt_pending = true;
            t.kind = thread::Kind::Worktree;
            t.repo = f._home.path().join("repo").to_string_lossy().into_owned();
            t.worktree_path = worktree.to_string_lossy().into_owned();
            t.cwd = worktree.to_string_lossy().into_owned();
            t.pane_id = "w2:p1".into();
            t.agent = "claude".into();
            t.agent_name = "hp-demo-t-0001".into();
        }).unwrap();
        let stale_snapshot = thread::load(&f.project, &pending.id).unwrap();
        thread::begin_restart(&f.project, &pending.id, |_| {}).unwrap();

        let ctx = Ctx { env: &f.env, root: f.root.clone(), config_dir: f.root.join("cfg"), runner: &runner, detached_ticker: false };
        let pane = Pane { pane_id: "w2:p1".into(), workspace_id: "w2".into(), tab_id: "w2:t1".into(), cwd: worktree.to_string_lossy().into_owned(), ..Pane::default() };
        let seen = Seen { socket: f.project.coordinator().unwrap().socket, agents: Vec::new(), panes: vec![pane], ..Seen::default() };
        let mut may_start = true;
        let mut errors = Vec::new();
        launch_pass(&ctx, &f.project, &seen, &[stale_snapshot], &mut may_start, &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(runner.count("agent start"), 0);
        let current = thread::load(&f.project, &pending.id).unwrap();
        assert_eq!(current.status, thread::Status::Starting);
        assert!(!current.prompt_pending);
        assert_eq!(current.launch_attempts, 0);

        let claimed = thread::allocate(&f.project, |t| {
            t.status = thread::Status::Open;
            t.prompt_pending = true;
            t.launch_attempts = 0;
            t.agent = "claude".into();
        }).unwrap();
        assert!(thread::claim_launch(&f.project, &claimed.id).unwrap().is_some());
        let error = thread::begin_restart(&f.project, &claimed.id, |_| {}).unwrap_err().to_string();
        assert!(error.contains("pending ticker launch"), "{error}");
        let current = thread::load(&f.project, &claimed.id).unwrap();
        assert_eq!(current.status, thread::Status::Open);
        assert!(current.prompt_pending);
    }

    /// The 2026-11-18 incident, end to end: a coordinator whose terminal core
    /// reaped, found again by its name, and the nudge that then goes out. Before
    /// the repair pass this tick could only hold, with a reason that reads as
    /// "the coordinator is busy".
    #[test]
    fn a_coordinator_whose_terminal_was_reaped_is_found_again_and_nudged() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        let project = project::create(&root, "demo", vec![]).unwrap();
        let core = crate::fake_hc::FakeCore::new();
        let dir = project.canonical_dir().to_string_lossy().into_owned();
        let labels = [("hp_agent", "hpc-demo"), ("hp_kind", "pi")];
        core.terminal("hp-demo-1", &dir, &labels);
        core.finish("hp-demo-1", "killed");
        core.terminal("hp-demo-2", "/Users/someone", &labels);
        core.set_status("hp-demo-2", "idle");
        // Pi's editor between its two rules: an empty box, so the nudge has
        // nothing to fear from a draft.
        let rule = "─".repeat(70);
        core.set_screen("hp-demo-2", &format!("{rule}\n\n{rule}\n"));
        project
            .update_coordinator(|c| {
                c.pane_id = "hp-demo-1".into();
                c.agent_name = "hpc-demo".into();
                c.cwd = dir;
            })
            .unwrap();
        inbox::write(&project, "thread-state", "t-0001", "finished", "t-0001 finished", "").unwrap();

        let env = Env::for_test(home.path(), &[]);
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: home.path().join("cfg"), runner: &core, detached_ticker: false };
        let _ = tick_project(&ctx, &project);

        assert_eq!(project.coordinator().unwrap().pane_id, "hp-demo-2", "the record followed the agent");
        // Both waits the nudge asks for, already satisfied: an idle coordinator
        // whose box has been empty for a while.
        let long_ago = "2025-01-01T00:00:00Z";
        let path = project.state_dir().join("coordinators.json");
        let mut panes: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for pane in panes.as_array_mut().unwrap() {
            pane["pair_since"] = long_ago.into();
        }
        std::fs::write(&path, panes.to_string()).unwrap();
        let path = project.state_dir().join("ticker.json");
        let mut state: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        state["box_pane"] = "hp-demo-2".into();
        state["box_empty_since"] = long_ago.into();
        std::fs::write(&path, state.to_string()).unwrap();

        let _ = tick_project(&ctx, &project);
        let calls = core.calls.borrow();
        assert_eq!(calls.iter().filter(|call| call.contains("send hp-demo-2")).count(), 1, "{calls:?}");
        assert!(calls.iter().any(|call| call.contains("[hp inbox] t-0001 finished")), "{calls:?}");
    }

    /// The other half: nothing to repair means an item and a named hold, not
    /// silence. `no coordinator is idle long enough` would send a reader
    /// looking for a busy coordinator that does not exist.
    /// The 2026-11-21 incident end to end: a coordinator 41 minutes into one
    /// turn, a ticker that stops it exactly once, the log line a person can act
    /// on, and the `thread-audit` item the warning path still writes — the
    /// warning could never reach a coordinator inside the turn it is about.
    /// A thread's pane, however long it works, is never the ticker's to stop.
    #[test]
    fn a_coordinator_turn_past_the_limit_is_interrupted_once() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        let project = project::create(&root, "rexa", vec![]).unwrap();
        let core = crate::fake_hc::FakeCore::new();
        let dir = project.canonical_dir().to_string_lossy().into_owned();
        let pane = "hp-rexa-coordinator";
        core.terminal(pane, &dir, &[("hp_agent", "hpc-rexa"), ("hp_kind", "pi"), ("hp_project", "rexa")]);
        core.set_status(pane, "working");
        core.set_turn(pane, 7);
        // A thread's pane, working since long before: the audit may look at it,
        // the ticker must not stop it.
        let thread_pane = "hp-rexa-t-0001";
        core.terminal(thread_pane, &format!("{dir}/threads/t-0001"), &[("hp_agent", "hp-rexa-t-0001"), ("hp_kind", "pi")]);
        core.set_status(thread_pane, "working");
        core.set_turn(thread_pane, 3);
        project
            .update_coordinator(|c| {
                c.pane_id = pane.into();
                c.agent_name = "hpc-rexa".into();
                c.cwd = dir.clone();
            })
            .unwrap();

        let now = jiff::Timestamp::now();
        let ago = |secs: i64| now.checked_sub(jiff::SignedDuration::from_secs(secs)).unwrap().to_string();
        // The turn the ticker measured, and an audit that has run before (a
        // first sight only records the clock, so no item would be written).
        let mut state = steps::State::default();
        for (id, since) in [(pane, ago(41 * 60)), (thread_pane, ago(30 * 60))] {
            state.coordinator_turns.insert(id.into(), steps::Turn { since, longest: 0, day: String::new() });
        }
        state.last_audit = ago(31 * 60);
        steps::save_state(&project, &state).unwrap();
        crate::coordinator::save_live(
            &project,
            &[
                crate::coordinator::LivePane {
                    pane_id: pane.into(),
                    agent_status: "working".into(),
                    state_change_seq: 7,
                    pair_since: ago(41 * 60),
                    ..Default::default()
                },
            ],
        )
        .unwrap();

        let env = Env::for_test(home.path(), &[]);
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: home.path().join("cfg"), runner: &core, detached_ticker: false };
        tick_project(&ctx, &project).unwrap();
        let interrupts = || core.calls.borrow().iter().filter(|call| call.contains("--json interrupt")).cloned().collect::<Vec<_>>();
        assert_eq!(interrupts().len(), 1, "the coordinator, once: {:?}", core.calls.borrow());
        assert!(interrupts()[0].contains(&format!("interrupt {pane}")), "{:?}", interrupts());
        let state = steps::load_state(&project);
        assert_eq!(state.interrupted_turns.get(pane), Some(&7), "the stopped turn is remembered");
        assert!(!state.interrupted_turns.contains_key(thread_pane), "a thread's pane is not the ticker's turn");
        let items = inbox::unhandled(&project);
        let long: Vec<&inbox::Item> = items.iter().filter(|item| item.kind == steps::AUDIT_KIND && item.event == "its coordinator's turn has run long").collect();
        assert_eq!(long.len(), 1, "the warning is still written: {items:?}");

        // The same turn, still saying `working` (a harness that ignored the
        // stop key): the hand that stopped it stays its hand.
        core.set_status(pane, "working");
        core.set_turn(pane, 7);
        tick_project(&ctx, &project).unwrap();
        assert_eq!(interrupts().len(), 1, "once per turn, not once per tick: {:?}", core.calls.borrow());
    }

    /// A whole tick, off and on: off adds nothing to the log (every line is the
    /// line it was), on says what each stage cost.
    #[test]
    fn a_tick_says_nothing_about_timings_unless_asked() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        let project = project::create(&root, "demo", vec![]).unwrap();
        let core = crate::fake_hc::FakeCore::new();
        let dir = project.canonical_dir().to_string_lossy().into_owned();
        let term = core.terminal("hp-demo-1", &dir, &[("hp_agent", "hpc-demo"), ("hp_kind", "pi"), ("hp_lane", "default")]);
        core.set_status(&term.name, "idle");
        project
            .update_coordinator(|c| {
                c.pane_id = term.name.clone();
                c.agent_name = "hpc-demo".into();
                c.cwd = dir.clone();
            })
            .unwrap();
        let env = Env::for_test(home.path(), &[]);
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: home.path().join("cfg"), runner: &core, detached_ticker: false };

        let quiet = home.path().join("quiet.log");
        let mut memory = Memory::new(&ctx);
        tick_with(&ctx, &Log::at(quiet.clone()), &mut memory, &mut Timings::off());
        let quiet = std::fs::read_to_string(&quiet).unwrap_or_default();
        assert!(!quiet.contains("timings"), "nothing about timings when off: {quiet}");

        let loud = home.path().join("loud.log");
        let mut memory = Memory::new(&ctx);
        tick_with(&ctx, &Log::at(loud.clone()), &mut memory, &mut Timings::on());
        let loud = std::fs::read_to_string(&loud).unwrap();
        assert!(loud.contains("timings demo: "), "a line per project: {loud}");
        assert!(loud.contains("(load "), "with its stages: {loud}");
        assert!(loud.lines().any(|line| line.contains("timings round: ")), "and one for the round: {loud}");

        // The same round, in the file `doctor` and `ticker status` read, and
        // without `HP_TICK_TIMINGS` having been asked for it: this round only
        // went through `Timings::on` because a test wanted the lines, and the
        // standing measurement is not the switch's business.
        let rounds = load_rounds(&root);
        let printed: u64 = loud
            .lines()
            .find_map(|line| line.split_once("timings round: ").map(|(_, rest)| rest))
            .and_then(|rest| rest.strip_suffix("ms"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(rounds.round_ms, printed, "the file says what the round printed: {rounds:?}");
        assert_eq!(rounds.round_ms_recent.last(), Some(&rounds.round_ms), "{rounds:?}");
        assert!(rounds.round_ms_recent.len() >= 2, "every round is kept, not only timed ones: {rounds:?}");
        // And where the round went, without `HP_TICK_TIMINGS` having to be in
        // the deployed ticker's environment (t-0095).
        assert!(!rounds.round_stages.is_empty(), "the last round's stages are recorded: {rounds:?}");
        assert!(rounds.round_stages.iter().any(|stage| stage.stage == "load"), "{rounds:?}");
        assert!(
            rounds.round_stages.windows(2).all(|pair| pair[0].ms >= pair[1].ms),
            "most expensive first: {rounds:?}"
        );
    }

    /// The standing measurement, with no ticker and no environment variable:
    /// an old file reads as "no round yet", every round is kept once, and only
    /// the last ten stay.
    #[test]
    fn every_round_is_remembered_and_only_the_last_ten() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        std::fs::create_dir(&root).unwrap();
        assert_eq!(load_rounds(&root), Rounds::default(), "no file, no round");
        assert_eq!(round_line(&root), (RoundHealth::NoData, "no round recorded yet".to_string()));

        // A file written before these fields existed — or a project's own
        // `.state/ticker.json` — still reads: it simply has no round in it.
        std::fs::create_dir_all(root.join(".state")).unwrap();
        std::fs::write(rounds_path(&root), "{\n  \"tick_at\": \"20261101T000000Z\"\n}\n").unwrap();
        assert_eq!(load_rounds(&root), Rounds::default());
        assert_eq!(round_line(&root).1, "no round recorded yet");

        for ms in 1..=12u64 {
            record_round(&root, Duration::from_millis(ms * 100), Vec::new()).unwrap();
        }
        let rounds = load_rounds(&root);
        assert_eq!(rounds.round_ms, 1200, "the last round");
        assert_eq!(rounds.round_ms_recent, (3..=12).map(|ms| ms * 100).collect::<Vec<u64>>(), "the oldest two dropped");
        assert_eq!(rounds.round_ms_recent.len(), ROUNDS_KEPT);
        assert!(rounds.round_stages.is_empty(), "no stages named, none recorded");
    }

    /// Where a round went, stage by stage, summed over every project that ran
    /// it — the standing answer to "the deployed ticker is slow", which cannot
    /// be asked for a `HP_TICK_TIMINGS=1` (its environment is the plist's).
    #[test]
    fn a_round_records_which_stages_took_the_time() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let stages = vec![
            Stage { stage: "housekeeping".into(), ms: 3120 },
            Stage { stage: "pr".into(), ms: 2950 },
            Stage { stage: "thread_list".into(), ms: 240 },
            Stage { stage: "load".into(), ms: 0 },
        ];
        record_round(&root, Duration::from_millis(8000), stages.clone()).unwrap();
        assert_eq!(load_rounds(&root).round_stages, stages, "only the last round's stages are kept");
        record_round(&root, Duration::from_millis(700), Vec::new()).unwrap();
        assert!(load_rounds(&root).round_stages.is_empty(), "the next round replaces them, it does not grow");
    }

    /// Stage totals as the round sees them: one number per stage name, whatever
    /// project it ran in, most expensive first.
    #[test]
    fn stage_totals_add_the_projects_of_one_stage_together() {
        let mut timings = Timings::off();
        timings.stage("alpha", "lists");
        std::thread::sleep(Duration::from_millis(5));
        timings.stage("alpha", "housekeeping");
        std::thread::sleep(Duration::from_millis(5));
        timings.stage("beta", "lists");
        std::thread::sleep(Duration::from_millis(5));
        timings.stop();
        let rest = timings.total("beta");
        let totals = timings.stage_totals();
        assert_eq!(totals.len(), 2, "two stage names, not four spans: {totals:?}");
        let lists = totals.iter().find(|stage| stage.stage == "lists").expect("lists is there");
        assert!(lists.ms >= 10, "both projects' `lists` add up: {totals:?}");
        assert!(totals.iter().any(|stage| stage.stage == "housekeeping"), "{totals:?}");
        assert!(totals.windows(2).all(|pair| pair[0].ms >= pair[1].ms), "most expensive first: {totals:?}");
        assert!(!timings.log, "recording is not logging");
        assert!(rest > Duration::ZERO);
    }

    /// What `doctor` and `ticker status` print, and what counts as slow: the
    /// last round or the median of the last ten, against the loop's own tick.
    #[test]
    fn a_round_over_the_tick_is_a_warning_with_the_way_to_the_stages() {
        let normal = Rounds { round_ms: 93, round_ms_recent: vec![110; 10], ..Rounds::default() };
        assert_eq!(rounds_line(&normal), (RoundHealth::Fine, "last 93ms, median of 10 110ms (TICK 15000ms)".to_string()));
        // An even count is the mean of the two middles, so one slow round
        // cannot claim the median by itself.
        assert!(rounds_line(&Rounds { round_ms: 10, round_ms_recent: vec![1, 2, 3, 4], ..Rounds::default() }).1.contains("median of 4 2ms"));

        // An old file has no stages: the line can only point at the switch.
        let slow = Rounds { round_ms: 21840, round_ms_recent: vec![19300; 10], ..Rounds::default() };
        let (health, text) = rounds_line(&slow);
        assert_eq!(health, RoundHealth::Slow, "over the tick is a warning: {text}");
        assert_eq!(
            text,
            format!("last 21840ms, median of 10 19300ms (TICK 15000ms) — over the 15000ms TICK; set {TIMINGS_VAR}=1 for the per-stage breakdown")
        );

        // With them, the line says where the time went — the deployed ticker's
        // environment cannot be given the switch (t-0095).
        let with_stages = Rounds {
            round_ms: 21840,
            round_ms_recent: vec![19300; 10],
            round_stages: vec![
                Stage { stage: "housekeeping".into(), ms: 12000 },
                Stage { stage: "lists".into(), ms: 6100 },
                Stage { stage: "pr".into(), ms: 2950 },
                Stage { stage: "thread_list".into(), ms: 240 },
                Stage { stage: "load".into(), ms: 0 },
            ],
        };
        assert_eq!(
            rounds_line(&with_stages),
            (
                RoundHealth::Slow,
                "last 21840ms, median of 10 19300ms (TICK 15000ms) — over the 15000ms TICK; most of it: housekeeping 12000ms, lists 6100ms, pr 2950ms"
                    .to_string()
            )
        );
        // A fast round says nothing about stages: the detail belongs to the slow
        // line, so a healthy ticker prints exactly what it printed before.
        let fast_with_stages = Rounds { round_stages: with_stages.round_stages.clone(), ..normal.clone() };
        assert_eq!(rounds_line(&fast_with_stages).1, rounds_line(&normal).1);

        // Either half over the tick means the loop cannot keep up: a slow last
        // round, and a median that stays slow while the last round was quick.
        assert_eq!(rounds_line(&Rounds { round_ms: 16000, round_ms_recent: vec![16000, 100, 100, 100], ..Rounds::default() }).0, RoundHealth::Slow);
        assert_eq!(rounds_line(&Rounds { round_ms: 900, round_ms_recent: vec![100, 21000, 21000, 21000], ..Rounds::default() }).0, RoundHealth::Slow);
        assert_eq!(rounds_line(&Rounds { round_ms: 15000, round_ms_recent: vec![15000], ..Rounds::default() }).0, RoundHealth::Fine, "exactly the tick still keeps up");
    }

    #[test]
    fn a_coordinator_that_is_gone_outright_leaves_one_item_and_a_named_hold() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        let project = project::create(&root, "demo", vec![]).unwrap();
        let core = crate::fake_hc::FakeCore::new();
        let dir = project.canonical_dir().to_string_lossy().into_owned();
        core.terminal("hp-demo-1", &dir, &[("hp_agent", "hpc-demo")]);
        core.finish("hp-demo-1", "killed");
        project
            .update_coordinator(|c| {
                c.pane_id = "hp-demo-1".into();
                c.agent_name = "hpc-demo".into();
                c.cwd = dir;
            })
            .unwrap();
        inbox::write(&project, "thread-state", "t-0001", "finished", "t-0001 finished", "").unwrap();

        let env = Env::for_test(home.path(), &[]);
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: home.path().join("cfg"), runner: &core, detached_ticker: false };
        let _ = tick_project(&ctx, &project);
        let _ = tick_project(&ctx, &project);

        let items = inbox::unhandled(&project);
        let lost: Vec<&crate::inbox::Item> = items.iter().filter(|item| item.kind == "coordinator").collect();
        assert_eq!(lost.len(), 1, "one item per loss, not one per tick: {items:?}");
        assert_eq!((lost[0].subject.as_str(), lost[0].event.as_str()), ("demo", "coordinator terminal gone"));
        assert!(lost[0].summary.contains("hp-demo-1"), "{}", lost[0].summary);
        assert!(lost[0].summary.contains("finished"), "{}", lost[0].summary);
        assert!(lost[0].summary.contains("hp open demo"), "{}", lost[0].summary);
        let state = steps::load_state(&project);
        let (reason, _) = state.held("").expect("the hold is recorded");
        assert_eq!(reason, steps::NO_COORDINATOR_GONE);
    }

    #[test]
    fn a_shell_prompt_pane_is_not_launched_by_the_ticker() {
        let f = fixture();
        let runner = FakeRunner::new();
        runner.on("agent list", ok(NO_AGENTS));
        runner.on("pane list", ok(&with_cwd(PANE, &f)));
        let ctx = Ctx { env: &f.env, root: f.root.clone(), config_dir: f.root.join("cfg"), runner: &runner, detached_ticker: false };
        for _ in 0..3 {
            let _ = tick_project(&ctx, &f.project);
        }
        assert_eq!(runner.count("agent start"), 0);
        assert_eq!(runner.count("agent prompt"), 0);
        assert!(coordinator::live(&f.project).is_empty());
    }

    #[test]
    fn unreachable_session_reads_no_state() {
        let f = fixture();
        let runner = FakeRunner::new();
        runner.on("agent list", fail(1, "connection refused"));
        let ctx = Ctx { env: &f.env, root: f.root.clone(), config_dir: f.root.join("cfg"), runner: &runner, detached_ticker: false };
        assert!(!tick_project(&ctx, &f.project).unwrap());

        // A socket file that is gone is not even called; only the default
        // session is looked in for an agent in the project folder.
        let gone = f.project.coordinator().unwrap().socket;
        std::fs::remove_file(&gone).unwrap();
        let runner = FakeRunner::new();
        let ctx = Ctx { env: &f.env, root: f.root.clone(), config_dir: f.root.join("cfg"), runner: &runner, detached_ticker: false };
        assert!(!tick_project(&ctx, &f.project).unwrap());
        assert!(runner.calls.borrow().iter().all(|c| !c.env.iter().any(|(_, v)| *v == gone)));
        assert_eq!(runner.count("agent list"), 0);
    }

    /// Two roots, two jobs: the label is derived from the canonical root, so a
    /// scratch root (`scripts/dev-hp`) can neither unload nor rewrite the real
    /// root's LaunchAgent (t-0083). Before this, every root shared one label, so
    /// `dev-hp open` booted the real ticker out and pointed its plist at the
    /// scratch root.
    #[test]
    fn each_root_gets_its_own_launch_agent_label() {
        let home = tempfile::tempdir().unwrap();
        let mut roots = Vec::new();
        for name in ["real", "scratch"] {
            let dir = home.path().join(name);
            std::fs::create_dir(&dir).unwrap();
            roots.push(dir);
        }
        let (real, scratch) = (&roots[0], &roots[1]);
        let label = agent_label(real);
        assert!(label.starts_with("dev.hp.ticker."), "{label}");
        assert_eq!(label, agent_label(real), "one root, one label, every time");
        assert_eq!(label, agent_label(&real.join(".")), "another spelling of the same folder is the same root");
        assert_ne!(label, agent_label(scratch), "two roots must never share a job");
        assert_eq!(
            agent_path(home.path(), real),
            home.path().join(format!("Library/LaunchAgents/{label}.plist")),
            "the plist file follows the label"
        );
        assert_ne!(agent_path(home.path(), real), agent_path(home.path(), scratch), "and so does the file");
    }

    /// The two halves of a plist are one root: the label it is loaded under, and
    /// the `--root` its program ticks.
    #[test]
    fn a_plist_names_its_own_label_and_its_own_root() {
        let root = Path::new("/Users/me/.herdr-projects");
        let plist = agent_plist(Path::new("/opt/hp/hp"), root, &log_path(root));
        assert!(plist.contains(&format!("<key>Label</key><string>{}</string>", agent_label(root))), "{plist}");
        assert_eq!(plist_root(&plist).as_deref(), Some("/Users/me/.herdr-projects"), "{plist}");
        assert_eq!(plist_program(&plist).as_deref(), Some("/opt/hp/hp"), "{plist}");
    }

    /// A plist as the shared label wrote it: the same program and root, under
    /// `dev.hp.ticker`.
    fn legacy_plist(binary: &Path, root: &Path) -> String {
        let ours = format!("<key>Label</key><string>{}</string>", agent_label(root));
        let theirs = format!("<key>Label</key><string>{LEGACY_AGENT_LABEL}</string>");
        agent_plist(binary, root, &log_path(root)).replace(&ours, &theirs)
    }

    fn legacy_file(home: &Path) -> PathBuf {
        agent_path_in(home, LEGACY_AGENT_LABEL)
    }

    fn write_legacy(home: &Path, text: &str) -> PathBuf {
        let path = legacy_file(home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    /// How often this root's job was unloaded from under `label`. Whole lines,
    /// because `dev.hp.ticker` is a prefix of every derived label.
    fn unloaded(runner: &FakeRunner, label: &str) -> usize {
        let line = format!("launchctl bootout gui/{}/{label}", unsafe { libc_getuid() });
        runner.calls.borrow().iter().filter(|cmd| cmd.display() == line).count()
    }

    #[test]
    fn the_shared_job_is_replaced_when_its_plist_is_this_roots() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let legacy = write_legacy(home.path(), &legacy_plist(Path::new("/opt/hp/hp"), &root));
        let runner = FakeRunner::new();
        runner.on("launchctl print", fail(113, "Could not find service"));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));

        install_agent(home.path(), Path::new("/opt/hp/hp"), &root, &runner).unwrap();

        assert!(!legacy.exists(), "this root's shared plist is gone");
        assert_eq!(unloaded(&runner, LEGACY_AGENT_LABEL), 1, "and the job it named is unloaded");
        assert!(agent_path(home.path(), &root).exists(), "the derived plist is the one left behind");
        let log = std::fs::read_to_string(root.join(".ticker.log")).unwrap_or_default();
        assert!(log.contains("moved this root off the shared LaunchAgent"), "{log}");
    }

    /// The accident itself: the shared plist points at another root. Touching it
    /// is exactly what t-0083 exists to stop.
    #[test]
    fn another_roots_shared_job_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        std::fs::create_dir(&root).unwrap();
        let other = Path::new("/Users/someone/.herdr-projects");
        let written = legacy_plist(Path::new("/opt/hp/hp"), other);
        let legacy = write_legacy(home.path(), &written);
        let runner = FakeRunner::new();
        runner.on("launchctl print", fail(113, "Could not find service"));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));

        install_agent(home.path(), Path::new("/opt/hp/hp"), &root, &runner).unwrap();

        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), written, "another root's plist is not rewritten");
        assert_eq!(unloaded(&runner, LEGACY_AGENT_LABEL), 0, "and its job is not unloaded");
        assert!(agent_path(home.path(), &root).exists(), "this root gets its own plist anyway");
    }

    /// A ticker started by the shared job is still running: `bootout` would be
    /// its SIGTERM, so the job stays loaded and only its plist is moved aside —
    /// a reboot then loads the new one alone, and `ticker stop` ends the old job
    /// with that file as its proof.
    #[test]
    fn a_running_shared_job_is_moved_aside_and_stopped_with_the_next_stop() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        std::fs::create_dir(&root).unwrap();
        write_legacy(home.path(), &legacy_plist(Path::new("/opt/hp/hp"), &root));
        let [_, aside] = legacy_paths(home.path());
        let runner = FakeRunner::new();
        runner.on("launchctl print", fail(113, "Could not find service"));
        runner.on("launchctl bootout", ok(""));
        runner.on("launchctl bootstrap", ok(""));
        let lock = hold_lock(&root);

        install_agent(home.path(), Path::new("/opt/hp/hp"), &root, &runner).unwrap();
        assert_eq!(unloaded(&runner, LEGACY_AGENT_LABEL), 0, "a running ticker is not unloaded to change its label");
        assert!(!legacy_file(home.path()).exists(), "the plist is not left where a reboot would load it");
        assert!(aside.exists(), "it is kept as the proof of which root the loaded job is");

        remove_agent(home.path(), &root, &runner);
        assert_eq!(unloaded(&runner, LEGACY_AGENT_LABEL), 1, "`ticker stop` unloads the old job");
        assert!(!aside.exists(), "and drops its file");
        drop(lock);
    }

    #[test]
    fn log_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let log = Log { path: dir.path().join("log") };
        let long = "x".repeat(10_000);
        for _ in 0..150 {
            log.line(&long);
        }
        let size = std::fs::metadata(&log.path).unwrap().len();
        assert!(size <= LOG_CAP, "{size}");
        assert!(size > LOG_CAP / 4);
    }
}
