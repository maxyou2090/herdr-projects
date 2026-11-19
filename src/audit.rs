//! `hp thread audit`: cross-check the thread registry against the herdr
//! session and report the mismatches that no other view shows.
//!
//! Read only: it lists agents and panes and reads thread records. It never
//! closes a pane, never restarts a thread and never writes an item — the point
//! is that ten projects can run it in a sweep without any of them changing.
//!
//! The same core runs from the ticker (`steps::audit_items`) every
//! [`crate::steps::AUDIT_EVERY_SECS`], because a *quiet* project never runs the
//! sweep that used to be this check's only caller: the routine prompt is
//! suppressed when nothing needs a person, and a zombie thread that is already
//! `resolved` needs nobody until someone notices it (the 2026-11-13 incident).
//! The command and the ticker differ only in what they do with a finding:
//! print it, or write one inbox item per finding.


use anyhow::{Result, bail};
use serde::Serialize;

use crate::core::{Agent, Core, Pane};
use crate::paths::Ctx;
use crate::project::Project;
use crate::thread::{self, Status, Thread};
use crate::threads;

/// A record that says `resolved` while herdr still lists its agent (or a pane
/// at its id).
pub const RESOLVED_BUT_RUNNING: &str = "resolved-but-running";
/// An open record with a recorded pane that is gone, for a thread whose own
/// restart cannot bring it back (an adopted thread above all).
pub const REGISTERED_PANE_MISSING: &str = "registered-pane-missing";
/// An agent that has said `working` for a long time while neither its state
/// nor its report moved.
pub const WORKING_BUT_STALE: &str = "working-but-stale";
/// The project's coordinator record names a terminal core has finished, and no
/// live agent of the project took its place.
pub const COORDINATOR_TERMINAL_FINISHED: &str = "coordinator-terminal-finished";
/// The recorded coordinator terminal is alive and its program reports a status,
/// but it carries no `hp_agent`/`hp_kind`, so core's agent list cannot see it.
pub const COORDINATOR_LABELS_MISSING: &str = "coordinator-labels-missing";

/// How long an agent may sit in `working` with nothing written before the
/// audit calls it stale. Three aged-reminder windows
/// ([`crate::steps::RE_NUDGE_EVERY_SECS`]): silence is measured in the unit
/// the ticker already uses for "this has sat too long". One window is too
/// short — a single long step (a test run, a migration) easily passes 30
/// minutes without writing a report, and a false alarm on every slow step
/// would make the audit useless in a sweep. The record's `last_state_change`
/// and `last_report_change` are written by every state change, every report
/// copy and every progress update ([`newest_activity`] says why those two and
/// not the file time), so 90 minutes with neither, while the agent claims to
/// work and reports no progress of its own, is the shape of the 2026-11-13
/// incident (22.5 h and 9.8M tokens in a loop).
pub const WORKING_STALE_SECS: i64 = 3 * crate::steps::RE_NUDGE_EVERY_SECS;

/// What one run found.
#[derive(Debug, Default)]
pub struct Findings {
    /// Every mismatch, in thread-id order.
    pub found: Vec<Finding>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    /// One of [`RESOLVED_BUT_RUNNING`], [`REGISTERED_PANE_MISSING`],
    /// [`WORKING_BUT_STALE`], [`COORDINATOR_TERMINAL_FINISHED`]: stable, so a
    /// sweep can grep for it.
    pub kind: &'static str,
    /// The thread this is about, empty for a finding about the project's
    /// coordinator.
    pub thread: String,
    /// The lane id, for a finding about a coordinator; empty for a thread's.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub lane: String,
    /// One line: what was seen, and what it means for the reader.
    pub detail: String,
}

impl Finding {
    /// What the finding is about, as one safe subject: the thread id, or
    /// `coordinator-<lane>` for the project's own. It is the dedup key of the
    /// once-per-finding item, so the two kinds never shadow each other.
    pub fn subject(&self) -> String {
        if !self.thread.is_empty() {
            return self.thread.clone();
        }
        let lane = if self.lane.is_empty() { "default" } else { self.lane.as_str() };
        format!("coordinator-{lane}")
    }

    /// A few words for the nudge line and the notification: this binary's own
    /// phrase, never text from a report.
    pub fn event(&self) -> &'static str {
        match self.kind {
            RESOLVED_BUT_RUNNING => "still running after it was resolved",
            REGISTERED_PANE_MISSING => "pane gone and it cannot restart itself",
            COORDINATOR_TERMINAL_FINISHED => "its coordinator's terminal is gone",
            COORDINATOR_LABELS_MISSING => "its coordinator's pane carries no `hp_agent` label",
            _ => "working but nothing written",
        }
    }
}

/// Every mismatch in one project, in thread-id order, from this session's
/// lists. `socket` is the herdr session they belong to, for the agent's own
/// progress record; `hc` is that same session, for the one question the lists
/// cannot answer — what became of the coordinator's recorded terminal
/// (`hc ls` hides finished terminals, see [`crate::core::TerminalFate`]).
pub fn findings_with(project: &Project, socket: &str, now: jiff::Timestamp, agents: &[Agent], panes: &[Pane], hc: &Core) -> Findings {
    let mut found = Vec::new();
    for t in thread::list(project) {
        found.extend(thread_findings(&t, agents, panes, now, &project.root, socket, hc));
    }
    found.extend(coordinator_findings(project, agents, hc));
    Findings { found }
}

/// The project's own coordinator, when its record names a terminal that is gone
/// and nothing took its place: the 2026-11-18 incident, where every
/// notification was held as "no coordinator is idle long enough" while the
/// coordinator was in fact unreachable, and this audit said `nothing unusual`
/// because it only looked at threads.
fn coordinator_findings(project: &Project, agents: &[Agent], hc: &Core) -> Vec<Finding> {
    let Some(record) = project.coordinator() else {
        return Vec::new();
    };
    // No pane recorded is not a lost terminal. A project whose `lanes.json` has
    // a default lane and no `coordinator.json` answers with that lane's view,
    // and a lane with no coordinator instance keeps `pane_id` empty on purpose
    // (`docs/lanes.md` §10, `hp lane list` calls it "has no live coordinator").
    // little-world, ovis and web-tpl printed `the record still names pane , but
    // the session no longer knows its terminal` — an empty pane name and a
    // "lost" verdict for a coordinator that was never started (2026-11-19,
    // t-0055 §8c). `coordinator::repair` and `doctor` already skip exactly these
    // records (`!record.pane_id.is_empty()`), so the audit asks the same
    // question rather than phrasing the same answer differently.
    if record.pane_id.is_empty() {
        return Vec::new();
    }
    let lane = crate::lanes::default_lane_id(project);
    match crate::coordinator::fate(hc, project, &record, agents, &lane) {
        crate::coordinator::Fate::Lost { detail } => vec![Finding {
            kind: COORDINATOR_TERMINAL_FINISHED,
            thread: String::new(),
            lane,
            detail: format!(
                "the record still names pane {}, but {detail}; inbox items are held (not lost) until a coordinator runs again, and `open {}` starts one",
                record.pane_id, project.slug
            ),
        }],
        // The 2026-11-18 shape on rexa and law: the pane was healthy all along
        // and only the two labels hp reads an agent from were missing, so every
        // list said "no coordinator" and every nudge held.
        crate::coordinator::Fate::Unlabeled { .. } => vec![Finding {
            kind: COORDINATOR_LABELS_MISSING,
            thread: String::new(),
            lane,
            detail: format!(
                "pane {} is alive and reports a status but carries no `hp_agent`/`hp_kind`, so core's agent list cannot see the coordinator; the ticker writes both labels on its next tick, and `open {}` does it now",
                record.pane_id, project.slug
            ),
        }],
        _ => Vec::new(),
    }
}

/// The three checks for one thread, against this session's lists. `hc` answers
/// the one question the lists cannot — whether a terminal still runs a program
/// at all — and is asked only for a resolved thread whose pane is open while no
/// agent of it reports a state.
fn thread_findings(t: &Thread, agents: &[Agent], panes: &[Pane], now: jiff::Timestamp, root: &std::path::Path, socket: &str, hc: &Core) -> Vec<Finding> {
    let mut found = Vec::new();
    let live = thread::live_with_report(t, agents, panes, now, root, socket);
    match t.status {
        Status::Resolved => {
            // "Still running" means a program that *reports* a state, never the
            // `hp_agent` label: that label is written at launch and nothing
            // clears it when the program exits, so a terminal core keeps as a
            // shell still carries it while `hc get` answers `status: null`.
            // Reading the label alone printed `still listed in pane X ()` — the
            // empty brackets were the whole tell — for three my-net threads
            // whose processes and worktrees were long gone (2026-11-19, t-0055).
            // `coordinator::fate` asks the same question through
            // `TerminalFate::reported`, and so does `Agent::reporting` here.
            let running = agents.iter().find(|a| thread::agent_matches(t, a) && a.reporting());
            if let Some(agent) = running {
                found.push(Finding {
                    kind: RESOLVED_BUT_RUNNING,
                    thread: t.id.clone(),
                    lane: String::new(),
                    detail: format!(
                        "resolved, but its agent is still listed in pane {} ({}): nothing stopped it",
                        t.pane_id, agent.agent_status
                    ),
                });
            } else if live.pane_exists && !hc.pane_idle_shell(&t.pane_id) {
                // No agent of this thread in the list, but a program in the pane
                // still reports: its labels were lost, or it is not ours. The
                // list cannot say which, so this is reported and not acted on.
                // A pane holding a bare shell answers the same question with
                // "idle shell" and is not reported: that is the ordinary
                // leftover of a resolved thread.
                found.push(Finding {
                    kind: RESOLVED_BUT_RUNNING,
                    thread: t.id.clone(),
                    lane: String::new(),
                    detail: format!(
                        "resolved, but pane {} is still open on its working directory and a program still reports a status there: nothing stopped it",
                        t.pane_id
                    ),
                });
            }
        }
        Status::Open | Status::Starting => {
            if !t.pane_id.is_empty() && !live.pane_exists {
                // The question is not "is the pane gone" (a closed tab is
                // ordinary, and `thread restart` reopens one) but "can this
                // thread ever come back". `restart_plan` is the one place
                // that knows, so the audit asks it rather than guessing.
                // Its branch check is skipped (`false`): finding out costs
                // a git call per thread and only separates two ways of
                // saying a human must look.
                if let Err(refusal) = threads::restart_plan(t, &live, false, now) {
                    found.push(Finding {
                        kind: REGISTERED_PANE_MISSING,
                        thread: t.id.clone(),
                        lane: String::new(),
                        detail: format!("its pane {} is gone and it cannot restart itself: {refusal}", t.pane_id),
                    });
                }
            }
            // An agent that reports its own progress is working, however
            // long its turn runs: only its silence is a finding.
            if live.agent_state.as_deref() == Some("working")
                && !live.self_working()
                && let Some(newest) = newest_activity(t)
            {
                let silent = now.as_second() - newest.as_second();
                if silent >= WORKING_STALE_SECS {
                    found.push(Finding {
                        kind: WORKING_BUT_STALE,
                        thread: t.id.clone(),
                        lane: String::new(),
                        detail: format!("working, but nothing was written for {} (since {newest}): no report, no record change", duration_text(silent)),
                    });
                }
            }
        }
        Status::Failed => {}
    }
    found
}

/// Every mismatch in one project, from the session this process can reach.
/// `Err` when the project's session is unreachable: the command must say so
/// rather than print a clean project it never looked at.
pub fn findings(ctx: &Ctx, project: &Project) -> Result<Vec<Finding>> {
    let view = threads::session_view(ctx, project).ok_or_else(|| {
        anyhow::anyhow!(
            "the session of `{}` is not reachable; run `open {}` first (auditing compares its record with core's agents and terminals)",
            project.slug,
            project.slug
        )
    })?;
    Ok(findings_with(project, &view.socket, jiff::Timestamp::now(), &view.agents, &view.panes, &view.herdr).found)
}

/// The newest moment the agent is known to have done anything: the record's
/// `last_state_change` and `last_report_change`, floored by `created`.
///
/// Deliberately **not** the record's `updated` or its file time: the ticker
/// rewrites both every minute while a thread is quiet (`state_line` counts
/// `working · 12m quiet`, and that changing line is written back), so they say
/// when the ticker last looked, not when the agent last did anything. The
/// agent's own progress record is the other sign of life, and
/// [`crate::thread::Live::self_working`] already covers it.
fn newest_activity(t: &Thread) -> Option<jiff::Timestamp> {
    let stamp = |text: &str| text.parse::<jiff::Timestamp>().ok();
    let written = [stamp(&t.last_state_change), stamp(&t.last_report_change)].into_iter().flatten().max();
    // A record with neither (written before those fields existed) is measured
    // from its creation: it cannot claim more activity than it has.
    written.or_else(|| stamp(&t.created))
}

/// `3d`, `22h 30m`, `45m`, `30s`: long enough to read, short enough for one
/// line. Days are there for `hp overview --needs-you`, where a thread can wait
/// on a person for days.
pub(crate) fn duration_text(secs: i64) -> String {
    let (days, hours, minutes) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    if days > 0 {
        match hours {
            0 => format!("{days}d"),
            _ => format!("{days}d {hours}h"),
        }
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{secs}s")
    }
}

/// The machine-readable shape: what `--json` prints. `unusual` is the one
/// thing a sweep has to read.
pub fn json_value(project: &Project, findings: &[Finding]) -> serde_json::Value {
    serde_json::json!({
        "project": project.slug,
        "unusual": !findings.is_empty(),
        "findings": findings,
    })
}

/// `hp thread audit [slug] [--json]`. Findings are reported, not fixed, and
/// both a clean and an unusual project exit 0: a sweep calls this for ten
/// projects and must not read "3 findings" as "the command failed".
pub fn run(ctx: &Ctx, slug: Option<&str>, json: bool) -> Result<()> {
    let slug = match slug {
        Some(slug) => {
            crate::project::validate_slug(slug)?;
            slug.to_string()
        }
        // The pane's own project, else nothing: a picker would block a sweep.
        None => match crate::overview::resolve_slug_quiet(ctx) {
            Some(slug) => slug,
            None => bail!("no project resolves from this terminal; pass one: `thread audit <slug>`"),
        },
    };
    let project = Project::load(&ctx.root, &slug)?;
    let found = findings(ctx, &project)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&json_value(&project, &found))?);
        return Ok(());
    }
    if found.is_empty() {
        println!("{}: nothing unusual", project.slug);
        return Ok(());
    }
    println!("{}: {} finding(s)", project.slug, found.len());
    for finding in found {
        println!("  {}  {}  {}", finding.kind, finding.subject(), finding.detail);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Core;
    use crate::fake_hc::FakeCore;
    use crate::lanes::{Lane, Registry};
    use crate::project::{self, Project};
    use crate::thread::{self, Thread};

    /// A record and the core session that answers `hc`: what [`findings_with`]
    /// reads. `HC_SESSION` stays unset, which is the default core.
    struct World {
        home: tempfile::TempDir,
        fake: FakeCore,
    }

    impl World {
        fn new() -> World {
            World { home: tempfile::tempdir().unwrap(), fake: FakeCore::new() }
        }

        fn project(&self, slug: &str) -> Project {
            project::create(&self.home.path().join("root"), slug, vec![]).unwrap()
        }

        fn herdr(&self) -> Core<'_> {
            Core::new("hc", "", &self.fake)
        }
    }

    const PANE: &str = "hp-google-ai-pro-gemini-7e46d1";

    /// One resolved thread whose recorded pane core still lists. `labelled`
    /// writes the `hp_agent`/`hp_kind` labels `agent start` leaves behind, and
    /// `status` is what a program in that pane reports over OSC 7501 — `None`
    /// for a terminal kept as a shell after its program exited (2026-11-19,
    /// my-net t-0017/t-0018/t-0019).
    fn resolved_in_a_recorded_pane(world: &World, slug: &str, status: Option<&str>, labelled: bool) -> (Project, Thread) {
        let project = world.project(slug);
        let dir = project.canonical_dir().to_string_lossy().into_owned();
        let thread = thread::allocate(&project, |t| {
            t.title = "resolved and gone".into();
            t.status = Status::Resolved;
            t.resolved_reason = "merged".into();
            t.pane_id = PANE.into();
            t.cwd = dir.clone();
            t.agent = "pi".into();
            t.agent_name = format!("hp-{slug}-t-0017");
        })
        .unwrap();
        let labels: &[(&str, &str)] = if labelled { &[("hp_agent", thread.agent_name.as_str()), ("hp_kind", "pi")] } else { &[] };
        world.fake.terminal(PANE, &dir, labels);
        if let Some(status) = status {
            world.fake.set_status(PANE, status);
        }
        (project, thread)
    }

    fn findings(project: &Project, herdr: &Core) -> Vec<Finding> {
        let agents = herdr.agent_list().unwrap();
        let panes = herdr.pane_list().unwrap();
        findings_with(project, "", jiff::Timestamp::now(), &agents, &panes, herdr).found
    }

    /// The accident: `hc get` answers `state: live`, `status: null`, and the
    /// labels `hp_agent`/`hp_kind` are still on it, because nothing clears them
    /// when the program exits. Nothing runs there, so nothing is "still
    /// running" — the old check printed `still listed in pane X ()`, and the
    /// empty brackets were the whole tell.
    #[test]
    fn a_terminal_kept_after_its_program_exited_is_not_an_agent_still_running() {
        let world = World::new();
        let (project, t) = resolved_in_a_recorded_pane(&world, "my-net", None, true);
        let herdr = world.herdr();
        // The shape itself: core's agent list has the terminal (the label) and
        // says nothing about any state.
        let agents = herdr.agent_list().unwrap();
        assert_eq!(agents.len(), 1, "the label alone lists it as an agent: {agents:#?}");
        assert_eq!(agents[0].pane_id, t.pane_id);
        assert!(!agents[0].reporting(), "no program reported a status");
        assert!(agents[0].agent_status.is_empty());

        let found = findings(&project, &herdr);
        assert!(
            found.iter().all(|f| f.kind != RESOLVED_BUT_RUNNING),
            "{} is resolved and no program runs in its pane: \n{:#?}",
            t.id,
            found
        );
    }

    /// The guard this change must not remove (2026-11-18, t-0053/t-0054): the
    /// program is really there, reports `done`, and outlived the thread.
    #[test]
    fn a_resolved_thread_whose_program_still_reports_is_still_a_finding() {
        let world = World::new();
        let (project, t) = resolved_in_a_recorded_pane(&world, "my-net", Some("done"), true);
        let herdr = world.herdr();
        let found = findings(&project, &herdr);
        let hit: Vec<&Finding> = found.iter().filter(|f| f.kind == RESOLVED_BUT_RUNNING).collect();
        assert_eq!(hit.len(), 1, "a program that reports `done` is still running: \n{found:#?}");
        assert_eq!(hit[0].thread, t.id);
        assert!(hit[0].detail.contains("(done)"), "{}", hit[0].detail);
    }

    /// The pane branch is not dead code: a program can report a status while the
    /// `hp_agent` label is gone (the 2026-11-18 rexa/law shape, on a resolved
    /// thread). The agent list cannot see it; `hc get` can.
    #[test]
    fn a_resolved_pane_whose_program_still_reports_without_labels_is_a_finding() {
        let world = World::new();
        let (project, t) = resolved_in_a_recorded_pane(&world, "my-net", Some("done"), false);
        let herdr = world.herdr();
        assert!(herdr.agent_list().unwrap().is_empty(), "without `hp_agent` core lists no agent");
        let found = findings(&project, &herdr);
        let hit: Vec<&Finding> = found.iter().filter(|f| f.kind == RESOLVED_BUT_RUNNING).collect();
        assert_eq!(hit.len(), 1, "a program that reports is still running, labels or not: \n{found:#?}");
        assert_eq!(hit[0].thread, t.id);
        assert!(hit[0].detail.contains("still open on its working directory"), "{}", hit[0].detail);
    }

    /// little-world, ovis and web-tpl on 2026-11-19: a `lanes.json` whose default
    /// lane was never opened (empty `pane_id`) and **no** `coordinator.json` at
    /// all. `Project::coordinator()` answers with that lane's view, so the audit
    /// printed `the record still names pane , but the session no longer knows
    /// its terminal` — an empty pane name and a "lost" verdict for a project
    /// whose coordinator was never started. Nothing was lost: there is nothing
    /// to lose.
    #[test]
    fn a_lane_with_no_pane_recorded_is_not_a_lost_coordinator() {
        let world = World::new();
        let project = world.project("little-world");
        let lane = Lane {
            id: "c-1".into(),
            name: "default".into(),
            label: "Little World".into(),
            status: crate::lanes::Status::Open,
            ..Lane::default()
        };
        Registry { lanes: vec![lane], default: "c-1".into(), next: 1 }.save(&project).unwrap();
        assert!(!project.coordinator_path().exists(), "the real shape has no coordinator.json at all");
        assert_eq!(project.coordinator().map(|r| r.pane_id).unwrap_or_default(), "");

        let found = findings(&project, &world.herdr());
        assert!(found.is_empty(), "no coordinator was ever opened in this project: \n{found:#?}");
    }

    /// The guard this keeps (2026-11-18, t-0053): the record names a pane and
    /// core does not know that terminal any more.
    #[test]
    fn a_record_whose_terminal_is_gone_is_still_a_lost_coordinator() {
        let world = World::new();
        let project = world.project("law");
        project
            .update_coordinator(|c| {
                c.pane_id = "hp-law-coordinator".into();
                c.agent_name = "hpc-law".into();
            })
            .unwrap();

        let found = findings(&project, &world.herdr());
        assert_eq!(found.len(), 1, "a recorded terminal core cannot see is a finding: \n{found:#?}");
        assert_eq!(found[0].kind, COORDINATOR_TERMINAL_FINISHED);
        assert!(found[0].detail.contains("hp-law-coordinator"), "{}", found[0].detail);
    }

    #[test]
    fn durations_stay_readable() {
        assert_eq!(duration_text(30), "30s");
        assert_eq!(duration_text(45 * 60), "45m");
        assert_eq!(duration_text(22 * 3600 + 30 * 60), "22h 30m");
        assert_eq!(duration_text(WORKING_STALE_SECS), "1h 30m");
        assert_eq!(duration_text(3 * 86400), "3d");
        assert_eq!(duration_text(3 * 86400 + 4 * 3600 + 59 * 60), "3d 4h");
    }
}
