//! `hp thread audit`: cross-check the thread registry against the herdr
//! session and report the mismatches that no other view shows.
//!
//! Read only: it lists agents and panes and reads thread records. It never
//! closes a pane, never restarts a thread and never writes an item — the point
//! is that ten projects can run it in a sweep without any of them changing.

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::herdr::{Agent, Pane};
use crate::paths::Ctx;
use crate::project::Project;
use crate::thread::{self, Status, Thread};
use crate::threads::{self, SessionView};

/// A record that says `resolved` while herdr still lists its agent (or a pane
/// at its id).
pub const RESOLVED_BUT_RUNNING: &str = "resolved-but-running";
/// An open record with a recorded pane that is gone, for a thread whose own
/// restart cannot bring it back (an adopted thread above all).
pub const REGISTERED_PANE_MISSING: &str = "registered-pane-missing";
/// An agent that has said `working` for a long time while neither its state
/// nor its report moved.
pub const WORKING_BUT_STALE: &str = "working-but-stale";

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

/// One machine's agents and panes, or `None` when that machine cannot be
/// reached this run. Keyed by machine name (`""` is this session).
type MachineLists = BTreeMap<String, Option<(Vec<Agent>, Vec<Pane>)>>;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Finding {
    /// One of [`RESOLVED_BUT_RUNNING`], [`REGISTERED_PANE_MISSING`],
    /// [`WORKING_BUT_STALE`]: stable, so a sweep can grep for it.
    pub kind: &'static str,
    pub thread: String,
    /// One line: what was seen, and what it means for the reader.
    pub detail: String,
}

/// Every mismatch in one project, in thread-id order.
pub fn findings(ctx: &Ctx, project: &Project) -> Result<Vec<Finding>> {
    let view = threads::session_view(ctx, project).ok_or_else(|| {
        anyhow::anyhow!(
            "the herdr session of `{}` is not reachable; run `open {}` first (auditing compares its record with herdr's agents and panes)",
            project.slug,
            project.slug
        )
    })?;
    let now = jiff::Timestamp::now();
    // One pair of lists per machine: a remote thread costs one ssh call, not
    // one per thread.
    let mut machines: MachineLists = BTreeMap::new();
    let mut found = Vec::new();
    for t in thread::list(project) {
        let Some((agents, panes)) = machine_lists(&view, &t, &mut machines) else {
            continue;
        };
        let live = thread::live_with_report(&t, agents, panes, now, &project.root, &view.socket);
        match t.status {
            Status::Resolved => {
                // The 2026-11-13 report: resolved for a day, still looping.
                if let Some(state) = &live.agent_state {
                    found.push(Finding {
                        kind: RESOLVED_BUT_RUNNING,
                        thread: t.id.clone(),
                        detail: format!("resolved, but its agent is still listed in pane {} ({state}): nothing stopped it", t.pane_id),
                    });
                } else if live.pane_exists {
                    found.push(Finding {
                        kind: RESOLVED_BUT_RUNNING,
                        thread: t.id.clone(),
                        detail: format!("resolved, but pane {} is still open on its working directory", t.pane_id),
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
                    if let Err(refusal) = threads::restart_plan(&t, &live, false, now) {
                        found.push(Finding {
                            kind: REGISTERED_PANE_MISSING,
                            thread: t.id.clone(),
                            detail: format!("its pane {} is gone and it cannot restart itself: {refusal}", t.pane_id),
                        });
                    }
                }
                // An agent that reports its own progress is working, however
                // long its turn runs: only its silence is a finding.
                if live.agent_state.as_deref() == Some("working")
                    && !live.self_working()
                    && let Some(newest) = newest_activity(&t)
                {
                    let silent = now.as_second() - newest.as_second();
                    if silent >= WORKING_STALE_SECS {
                        found.push(Finding {
                            kind: WORKING_BUT_STALE,
                            thread: t.id.clone(),
                            detail: format!(
                                "working, but nothing was written for {} (since {newest}): no report, no record change",
                                duration_text(silent)
                            ),
                        });
                    }
                }
            }
            Status::Failed => {}
        }
    }
    Ok(found)
}

/// One machine's agents and panes, listed once per call to [`findings`]. A
/// machine that cannot be reached yields `None` (with one warning on stderr)
/// rather than an error: an unreachable laptop must not make a whole project's
/// audit fail, and its threads are simply not audited.
fn machine_lists<'a>(view: &SessionView, t: &Thread, cache: &'a mut MachineLists) -> Option<&'a (Vec<Agent>, Vec<Pane>)> {
    if !cache.contains_key(&t.machine) {
        let lists = threads::lists_for(view, t).ok();
        if lists.is_none() {
            let where_ = if t.machine.is_empty() { "this session" } else { t.machine.as_str() };
            eprintln!("warning: {where_} is unreachable; its threads are not audited");
        }
        cache.insert(t.machine.clone(), lists);
    }
    cache.get(&t.machine).and_then(Option::as_ref)
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

/// `22h 30m`, `45m`, `30s`: long enough to read, short enough for one line.
fn duration_text(secs: i64) -> String {
    let (hours, minutes) = (secs / 3600, secs % 3600 / 60);
    if hours > 0 {
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
            None => bail!("no project resolves from this herdr workspace; pass one: `thread audit <slug>`"),
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
        println!("  {}  {}  {}", finding.kind, finding.thread, finding.detail);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_stay_readable() {
        assert_eq!(duration_text(30), "30s");
        assert_eq!(duration_text(45 * 60), "45m");
        assert_eq!(duration_text(22 * 3600 + 30 * 60), "22h 30m");
        assert_eq!(duration_text(WORKING_STALE_SECS), "1h 30m");
    }
}
