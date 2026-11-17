//! Routines: scheduled prompts. A routine file that still names a `command`
//! keeps its prompt; the command is never run. Command routines were removed
//! (t-0039): the approval chain behind them was never exercised, so the ticker
//! writes a `config-error` item about the ignored line instead of running it.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::project::{self, Project};
use crate::thread::sha256_hex;

#[derive(Debug, Clone, PartialEq)]
pub enum Schedule {
    /// `every <N>m|h|d`, in seconds.
    Every(i64),
    /// `daily HH:MM`, local time.
    Daily(i8, i8),
}

pub fn parse_schedule(text: &str) -> Result<Schedule> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("every ") {
        let rest = rest.trim();
        let (digits, unit) = rest.split_at(rest.len().saturating_sub(1));
        let n: i64 = digits.parse().ok().filter(|n| *n > 0).with_context(|| format!("bad schedule `{text}`"))?;
        let seconds = match unit {
            "m" => 60,
            "h" => 3600,
            "d" => 86_400,
            _ => bail!("bad schedule `{text}`: use `every <N>m`, `<N>h` or `<N>d`"),
        };
        return Ok(Schedule::Every(n * seconds));
    }
    if let Some(rest) = text.strip_prefix("daily ") {
        let (h, m) = rest.trim().split_once(':').with_context(|| format!("bad schedule `{text}`"))?;
        let (h, m): (i8, i8) = (h.parse().ok().context("bad hour")?, m.parse().ok().context("bad minute")?);
        if !(0..24).contains(&h) || !(0..60).contains(&m) {
            bail!("bad schedule `{text}`: the time must be 00:00 to 23:59");
        }
        return Ok(Schedule::Daily(h, m));
    }
    bail!("bad schedule `{text}`: use `every <N>m|h|d` or `daily HH:MM`")
}

/// Whether a routine is due, comparing with its stored last run. The time zone
/// is read on each use, so `daily HH:MM` stays right across a daylight-saving
/// change in a long-running ticker.
pub fn is_due(schedule: &Schedule, last_run: jiff::Timestamp, now: &jiff::Zoned) -> bool {
    match schedule {
        Schedule::Every(seconds) => now.timestamp().as_second() - last_run.as_second() >= *seconds,
        Schedule::Daily(hour, minute) => {
            let Ok(today) = now.with().hour(*hour).minute(*minute).second(0).subsec_nanosecond(0).build() else {
                return false;
            };
            // The most recent occurrence of HH:MM at or before now.
            let latest = if today.timestamp() <= now.timestamp() {
                today
            } else {
                match today.yesterday() {
                    Ok(yesterday) => yesterday,
                    Err(_) => return false,
                }
            };
            last_run < latest.timestamp()
        }
    }
}

/// When a routine is next due: the interval after its last run, or the first
/// `HH:MM` after it. With no last run yet, the ticker's first look only
/// records one, so the next run is a schedule from now.
pub fn next_run(schedule: &Schedule, last_run: Option<jiff::Timestamp>, now: &jiff::Zoned) -> Option<jiff::Timestamp> {
    let last = last_run.unwrap_or(now.timestamp());
    match schedule {
        Schedule::Every(seconds) => last.checked_add(jiff::SignedDuration::from_secs(*seconds)).ok(),
        Schedule::Daily(hour, minute) => {
            let at = last.to_zoned(now.time_zone().clone()).with().hour(*hour).minute(*minute).second(0).subsec_nanosecond(0).build().ok()?;
            let at = if at.timestamp() > last { at } else { at.tomorrow().ok()? };
            Some(at.timestamp())
        }
    }
}

/// `last <time> · next <time>` for a scheduled routine, in local time.
pub fn when_text(routine: &Routine, state: Option<&State>, now: &jiff::Zoned) -> String {
    let Trigger::Schedule(schedule) = &routine.trigger else {
        return String::new();
    };
    let local = |t: jiff::Timestamp| t.to_zoned(now.time_zone().clone()).strftime("%Y-%m-%d %H:%M").to_string();
    let last = state.and_then(|s| s.last_run.parse::<jiff::Timestamp>().ok());
    let next = match next_run(schedule, last, now) {
        _ if !routine.enabled => "- (disabled)".to_string(),
        Some(t) if t <= now.timestamp() => "due now".to_string(),
        Some(t) => local(t),
        None => "-".to_string(),
    };
    let skipped = state.map(|s| s.skipped).filter(|n| *n > 0).map(|n| format!(" · {n} run(s) skipped while its item was unhandled")).unwrap_or_default();
    let alone = state.map(|s| s.no_coordinator).filter(|n| *n > 0).map(|n| format!(" · skipped: no coordinator ({n} run(s))")).unwrap_or_default();
    let quiet = state.map(|s| s.quiet).filter(|n| *n > 0).map(|n| format!(" · {n} quiet run(s): nothing needed a person since its last item")).unwrap_or_default();
    format!("last {} · next {next}{skipped}{alone}{quiet}", last.map(local).unwrap_or_else(|| "never".into()))
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(default)]
struct Front {
    schedule: String,
    command: String,
    enabled: bool,
    /// `"pr"`: fired by the ticker's pull request poll instead of a schedule.
    on: String,
    events: Vec<String>,
}

impl Default for Front {
    fn default() -> Self {
        Front { schedule: String::new(), command: String::new(), enabled: true, on: String::new(), events: Vec::new() }
    }
}

/// The pull request events a `pr` routine can fire on.
pub const PR_EVENTS: [&str; 4] = ["opened", "checks-failed", "review", "merged"];

#[derive(Debug, Clone, PartialEq)]
pub enum Trigger {
    Schedule(Schedule),
    /// `on = "pr"`, with the events it fires on (all of them when none are named).
    Pr(Vec<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Routine {
    pub name: String,
    pub trigger: Trigger,
    /// The schedule as written, or `on pr: <events>`.
    pub schedule_text: String,
    /// The `command` line as written. It is never run; a non-empty value only
    /// makes the ticker report that command routines are gone.
    pub command: String,
    pub enabled: bool,
    pub prompt: String,
}

/// A routine file that could not be used, keyed by its content hash so an
/// unfixed file is reported once.
#[derive(Debug, Clone, PartialEq)]
pub struct Broken {
    pub file: String,
    pub hash: String,
    pub error: String,
}

pub fn parse(name: &str, text: &str) -> Result<Routine> {
    project::validate_slug(name).context("a routine's file name must follow the slug rule")?;
    let rest = text.strip_prefix("+++\n").context("a routine must start with a `+++` line")?;
    let (front, body) = rest.split_once("\n+++\n").or_else(|| rest.strip_suffix("\n+++").map(|f| (f, ""))).context("no closing `+++` line")?;
    let front: Front = toml::from_str(front).context("front matter does not parse")?;
    let (trigger, schedule_text) = match front.on.as_str() {
        "" => (Trigger::Schedule(parse_schedule(&front.schedule)?), front.schedule.trim().to_string()),
        "pr" => {
            for event in &front.events {
                if !PR_EVENTS.contains(&event.as_str()) {
                    bail!("unknown pr event `{event}`; use {}", PR_EVENTS.join(", "));
                }
            }
            let events = if front.events.is_empty() { PR_EVENTS.iter().map(|e| e.to_string()).collect() } else { front.events.clone() };
            let text = format!("on pr: {}", events.join(", "));
            (Trigger::Pr(events), text)
        }
        other => bail!("`on = \"{other}\"` is not a trigger; use `on = \"pr\"` or a `schedule`"),
    };
    Ok(Routine {
        name: name.to_string(),
        trigger,
        schedule_text,
        command: front.command.trim().to_string(),
        enabled: front.enabled,
        prompt: body.trim().to_string(),
    })
}

pub fn load_all(project: &Project) -> (Vec<Routine>, Vec<Broken>) {
    let mut routines = Vec::new();
    let mut broken = Vec::new();
    let Ok(entries) = std::fs::read_dir(project.dir().join("routines")) else {
        return (routines, broken);
    };
    let mut files: Vec<String> = entries.flatten().filter_map(|e| e.file_name().into_string().ok()).filter(|n| n.ends_with(".md") && !n.starts_with('.')).collect();
    files.sort();
    for file in files {
        let path = project.dir().join("routines").join(&file);
        if !std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        match parse(file.trim_end_matches(".md"), &text) {
            Ok(routine) => routines.push(routine),
            Err(error) => broken.push(Broken { file: format!("routines/{file}"), hash: sha256_hex(text.as_bytes()), error: format!("{error:#}") }),
        }
    }
    (routines, broken)
}

// ----------------------------------------------------------------- journal

/// The append-only record of routine changes, in the project's `.state/`.
/// Nothing else reads it: it is not part of `context`, of `doctor`'s pass/fail
/// checks or of routine parsing, so a project's behaviour never depends on it.
pub fn journal_path(project: &Project) -> std::path::PathBuf {
    project.state_dir().join("routine-journal.log")
}

/// Where a `routine toggle` came from: a core terminal, or a plain one (the
/// ticker, a script). A file that changed on its own can be traced to the kind
/// of caller that changed it; the terminal address names the pane, which a pane
/// id cannot (core renames panes).
pub fn journal_source(env: &crate::paths::Env) -> String {
    match env.this_terminal() {
        Some(terminal) => format!("core-terminal:{terminal}"),
        None => "terminal".to_string(),
    }
}

/// One line for a journal entry: time, event, routine, resulting state,
/// source, binary and root. `binary` and `root` are what tell the installed
/// copy from a throwaway build at a scratch root.
pub fn journal_line(project: &Project, action: &str, name: &str, state: &str, source: &str) -> String {
    let binary = crate::paths::binary().map(|p| p.display().to_string()).unwrap_or_else(|_| "unknown".into());
    format!("{}  {action}  {name}  {state}  source={source}  binary={binary}  root={}\n", project::now(), project.root.display())
}

/// Appends one line; the caller holds the project lock. The file is tiny and
/// never rotated, so it is read whole and rewritten atomically.
pub fn journal_locked(project: &Project, action: &str, name: &str, state: &str, source: &str) -> Result<()> {
    let path = journal_path(project);
    let mut text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("could not read {}", path.display())),
    };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&journal_line(project, action, name, state, source));
    project::write_atomic(&path, text.as_bytes())
}

/// The append for changes that must land even if the record cannot be written:
/// a command that already changed a file stays changed, and the failure is
/// said on stderr instead of turning into an error.
pub fn journal_locked_or_warn(project: &Project, action: &str, name: &str, state: &str, source: &str) {
    if let Err(error) = journal_locked(project, action, name, state, source) {
        eprintln!("warning: could not record the routine change in {}: {error:#}", journal_path(project).display());
    }
}

/// The journal's line count, for `doctor`'s "you may clean this up" hint.
pub fn journal_lines(project: &Project) -> Option<usize> {
    std::fs::read_to_string(journal_path(project)).ok().map(|text| text.lines().count())
}

pub fn print_list(project: &Project) {
    let (routines, broken) = load_all(project);
    let states = crate::steps::load_state(project).routines;
    let now = jiff::Zoned::now();
    if routines.is_empty() && broken.is_empty() {
        println!("no routines");
    }
    for r in &routines {
        // A leftover `command` line is reported, never run (t-0039).
        let kind = if r.command.is_empty() { "prompt only".to_string() } else { "prompt only (its `command` is ignored: command routines were removed)".to_string() };
        let when = when_text(r, states.get(&r.name), &now);
        println!("{}\t{}\t{}\t{kind}\t{when}", r.name, r.schedule_text, if r.enabled { "enabled" } else { "disabled" });
    }
    for b in &broken {
        println!("{}\tconfig-error: {}", b.file, b.error);
    }
}

/// Per-routine ticker state, stored in `.state/ticker.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct State {
    pub last_run: String,
    pub output_hash: String,
    /// Ids of the unhandled items a prompt-only routine saw at its last fire
    /// (`steps::prompt_items`). A due run fires on an id that is not in here,
    /// so an item the coordinator archived afterwards is not news a second
    /// time: handling an item must not be what calls the sweep back (t-0029).
    /// Written by the prompt-only branch only; an older file reads as empty.
    pub seen_items: BTreeSet<String>,
    /// Due runs of a prompt-only routine that wrote nothing because nothing
    /// that needs a person had changed since the item it last fired on.
    pub quiet: u32,
    /// Runs of a prompt-only routine that wrote nothing because its last item
    /// was still unhandled.
    pub skipped: u32,
    /// Due runs that did nothing because the project had no live
    /// coordinator; back to 0 once one runs.
    pub no_coordinator: u32,
}

pub type States = BTreeMap<String, State>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_run_follows_the_schedule_and_says_when_it_is_due() {
        let now: jiff::Zoned = "2026-09-24T10:00:00+00:00[UTC]".parse().unwrap();
        let at = |t: &str| t.parse::<jiff::Timestamp>().unwrap();
        assert_eq!(next_run(&Schedule::Every(300), Some(at("2026-09-24T09:58:00Z")), &now), Some(at("2026-09-24T10:03:00Z")));
        assert_eq!(next_run(&Schedule::Every(300), None, &now), Some(at("2026-09-24T10:05:00Z")));
        assert_eq!(next_run(&Schedule::Daily(9, 0), Some(at("2026-09-24T09:00:00Z")), &now), Some(at("2026-09-25T09:00:00Z")));
        assert_eq!(next_run(&Schedule::Daily(9, 0), Some(at("2026-09-23T08:00:00Z")), &now), Some(at("2026-09-23T09:00:00Z")));
        let r = parse("autopilot", "+++\nschedule = \"every 5m\"\n+++\nGo.\n").unwrap();
        let state = State { last_run: "2026-09-24T09:00:00Z".into(), skipped: 2, ..State::default() };
        assert_eq!(when_text(&r, Some(&state), &now), "last 2026-09-24 09:00 · next due now · 2 run(s) skipped while its item was unhandled");
        assert_eq!(when_text(&r, None, &now), "last never · next 2026-09-24 10:05");
        let alone = State { last_run: "2026-09-24T09:58:00Z".into(), no_coordinator: 3, ..State::default() };
        assert_eq!(when_text(&r, Some(&alone), &now), "last 2026-09-24 09:58 · next 2026-09-24 10:03 · skipped: no coordinator (3 run(s))");
    }

    fn zoned(text: &str) -> jiff::Zoned {
        text.parse().unwrap()
    }

    #[test]
    fn schedule_parsing() {
        assert_eq!(parse_schedule("every 5m").unwrap(), Schedule::Every(300));
        assert_eq!(parse_schedule(" every 2h ").unwrap(), Schedule::Every(7200));
        assert_eq!(parse_schedule("every 1d").unwrap(), Schedule::Every(86_400));
        assert_eq!(parse_schedule("daily 07:30").unwrap(), Schedule::Daily(7, 30));
        for bad in ["", "every", "every 0m", "every -1h", "every 5x", "every m", "daily 24:00", "daily 7", "daily 07:60", "hourly", "* * * * *"] {
            assert!(parse_schedule(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn every_is_due_after_its_interval() {
        let now = zoned("2026-09-17T12:00:00+02:00[Europe/Stockholm]");
        let schedule = Schedule::Every(3600);
        assert!(!is_due(&schedule, "2026-09-17T09:30:00Z".parse().unwrap(), &now));
        assert!(is_due(&schedule, "2026-09-17T09:00:00Z".parse().unwrap(), &now));
    }

    #[test]
    fn daily_is_due_once_per_local_day() {
        let schedule = Schedule::Daily(7, 30);
        let before = zoned("2026-09-17T07:29:00+02:00[Europe/Stockholm]");
        let after = zoned("2026-09-17T07:31:00+02:00[Europe/Stockholm]");
        let ran_yesterday: jiff::Timestamp = "2026-09-16T05:30:10Z".parse().unwrap();
        assert!(!is_due(&schedule, ran_yesterday, &before));
        assert!(is_due(&schedule, ran_yesterday, &after));
        let ran_today: jiff::Timestamp = "2026-09-17T05:30:10Z".parse().unwrap();
        assert!(!is_due(&schedule, ran_today, &after));
        // A ticker that was down over 07:30 still runs it once when it is back.
        let late = zoned("2026-09-17T23:00:00+02:00[Europe/Stockholm]");
        assert!(is_due(&schedule, ran_yesterday, &late));
    }

    #[test]
    fn daily_follows_local_time_across_a_daylight_saving_change() {
        // Stockholm leaves summer time on 2026-10-25: 07:30 local moves from
        // 05:30Z to 06:30Z.
        let schedule = Schedule::Daily(7, 30);
        let ran: jiff::Timestamp = "2026-10-24T05:30:05Z".parse().unwrap();
        assert!(!is_due(&schedule, ran, &zoned("2026-10-25T06:45:00+01:00[Europe/Stockholm]")));
        assert!(is_due(&schedule, ran, &zoned("2026-10-25T07:30:00+01:00[Europe/Stockholm]")));
    }

    #[test]
    fn routine_parsing_and_name_validation() {
        let pr = parse("follow", "+++\non = \"pr\"\nevents = [\"checks-failed\", \"review\"]\n+++\nFix it.\n").unwrap();
        assert_eq!(pr.trigger, Trigger::Pr(vec!["checks-failed".into(), "review".into()]));
        assert_eq!(pr.schedule_text, "on pr: checks-failed, review");
        assert!(matches!(parse("all", "+++\non = \"pr\"\n+++\nx").unwrap().trigger, Trigger::Pr(e) if e.len() == 4));
        assert!(parse("bad", "+++\non = \"pr\"\nevents = [\"pushed\"]\n+++\n").is_err());
        // A leftover `command` on a `pr` routine parses too; it is ignored.
        assert!(parse("ok", "+++\non = \"pr\"\ncommand = \"x\"\n+++\n").unwrap().command == "x");
        assert!(parse("bad", "+++\non = \"slack\"\n+++\n").is_err());
        // A leftover `command` line still parses: its prompt runs and the
        // ticker reports the ignored line (t-0039).
        let r = parse("nightly", "+++\nschedule = \"daily 02:00\"\ncommand = \"./check.sh\"\n+++\n\nLook at the output.\n").unwrap();
        assert_eq!((r.name.as_str(), r.command.as_str(), r.enabled, r.prompt.as_str()), ("nightly", "./check.sh", true, "Look at the output."));
        let r = parse("p", "+++\nschedule = \"every 1h\"\nenabled = false\n+++\nPrompt").unwrap();
        assert!(r.command.is_empty() && !r.enabled);
        assert!(parse("Bad_Name", "+++\nschedule = \"every 1h\"\n+++\n").is_err());
        assert!(parse("../x", "+++\nschedule = \"every 1h\"\n+++\n").is_err());
        assert!(parse("ok", "+++\nschedule = \"sometimes\"\n+++\n").is_err());
        assert!(parse("ok", "no front matter").is_err());
    }

    #[test]
    fn broken_files_are_reported_with_their_hash() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        std::fs::write(project.dir().join("routines/good.md"), "+++\nschedule = \"every 1h\"\n+++\nP").unwrap();
        std::fs::write(project.dir().join("routines/Bad Name.md"), "+++\nschedule = \"every 1h\"\n+++\nP").unwrap();
        std::fs::write(project.dir().join("routines/broken.md"), "+++\nschedule = \n+++\n").unwrap();
        let (routines, broken) = load_all(&project);
        // Theirs plus the two default routines.
        assert_eq!(routines.len(), 3);
        assert_eq!(broken.len(), 2);
        assert!(broken.iter().all(|b| b.hash.len() == 64));
    }
}
