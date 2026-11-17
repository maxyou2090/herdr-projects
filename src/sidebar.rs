//! How a project's threads read: the words of a group, a row's one-line
//! display, and the labels that carry them. Herdr 1 drew these into its
//! sidebar and tab bar; herdr-core draws nothing, so the same strings are what
//! `overview`, `list` and the digest print.

use crate::herdr::Herdr;
use crate::project::{self, Project};
use crate::thread::{Group, Live, Thread};

/// Tokens this binary wrote in Herdr 1's layouts; cleared on every report.
pub const OLD_TOKENS: [&str; 4] = ["project", "thread", "review", "rank"];
/// Minutes without a self-report after which a working row says "Nm quiet".
pub const QUIET_SECS: i64 = 300;
/// The token every report writes naming the project a pane belongs to. It is
/// what tells one project's lanes from another's (`lanes::resolve`).
pub const PROJECT_TOKEN: &str = "hp_project";

/// The short word of a group, as the digest prints it.
pub fn word(group: Group) -> &'static str {
    match group {
        Group::WaitingOnYou => "needs you",
        Group::ReadyForReview => "review",
        Group::Working => "working",
        Group::Landing => "landing",
        Group::Idle => "idle",
        Group::Resolved => "resolved",
    }
}

/// Groups that need the user.
pub fn needs_you(group: Group) -> bool {
    matches!(group, Group::WaitingOnYou | Group::ReadyForReview)
}

fn pr_number(url: &str) -> Option<&str> {
    url.rsplit('/').next().filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// Line 3 of a thread row: the group word and one fact.
pub fn state_line(group: Group, thread: &Thread, live: &Live, now: jiff::Timestamp) -> String {
    let activity = live.self_report.as_ref().map(|r| r.activity.trim()).filter(|a| !a.is_empty());
    let pr = pr_number(&thread.pr).map(|n| format!("PR #{n}"));
    let fact = match group {
        Group::WaitingOnYou if !live.pane_exists => Some("pane closed".to_string()),
        Group::WaitingOnYou if thread.status == crate::thread::Status::Failed => Some("failed".to_string()),
        Group::WaitingOnYou if live.agent_state.as_deref() == Some("blocked") => Some("blocked".to_string()),
        Group::WaitingOnYou => activity.map(str::to_string),
        Group::ReadyForReview | Group::Landing => pr.or_else(|| Some("report".into())),
        Group::Working => match (&live.self_report, activity) {
            (_, Some(activity)) if live.report_age_secs < QUIET_SECS => Some(activity.to_string()),
            _ => {
                let since = match &live.self_report {
                    Some(record) => live.report_age_secs.max(now.as_second() - record.reported_at),
                    None => crate::thread::seconds_since(&thread.last_state_change, now),
                };
                (since >= QUIET_SECS && !thread.prompt_pending).then(|| format!("{}m quiet", since / 60))
            }
        },
        Group::Idle | Group::Resolved => None,
    };
    match fact {
        Some(fact) => format!("{} · {fact}", word(group)),
        None => word(group).to_string(),
    }
}

/// Row 1 of a thread: its id and title.
pub fn thread_display(thread: &Thread) -> String {
    format!("{} · {}", thread.id, thread.title)
}

/// A coordinator's row name: the project's name, plus ` · <lane label>` when
/// the project has a lane registry (`docs/lanes.md` §9). The gate is the
/// registry, not the lane count: a project that never wrote `lanes.json` reads
/// as before, and a project with lanes shows them even for one lane — including
/// the default — so two coordinators are never two identical rows.
pub fn coordinator_display(project: &Project, lane: Option<&str>) -> String {
    let name = project.read_project_md().map(|(s, _)| project::display_name(&s.name, &project.slug)).unwrap_or_else(|_| project.slug.clone());
    match crate::lanes::display_label(project, lane) {
        Some(label) => format!("{name} · {label}"),
        None => name,
    }
}

/// Reports one pane: its project, its display name and — for a coordinator
/// whose lane the registry names — its lane claim (`docs/lanes.md` §5.1).
/// `lane` is the id to write as `hp_lane`, or `None` for no claim: a thread
/// pane never claims a lane, and a project with no lane registry reports none.
pub fn report_pane(herdr: &Herdr, pane: &str, display: &str, slug: &str, _group: Group, lane: Option<&str>) {
    if pane.is_empty() {
        return;
    }
    let mut pairs: Vec<(String, String)> = vec![
        (PROJECT_TOKEN.into(), slug.to_string()),
        ("hp_display".into(), display.to_string()),
    ];
    if let Some(lane) = lane.filter(|lane| !lane.is_empty()) {
        pairs.push((crate::lanes::TOKEN.to_string(), lane.to_string()));
    }
    let _ = herdr.set_labels(pane, &pairs);
    let _ = herdr.clear_labels(pane, &OLD_TOKENS);
}

/// Clears every label this binary set on a pane: its project, display name and
/// lane claim.
pub fn clear_pane(herdr: &Herdr, pane: &str) {
    if pane.is_empty() {
        return;
    }
    let mut keys: Vec<&str> = vec![PROJECT_TOKEN, "hp_display", "hp_rank", crate::lanes::TOKEN];
    keys.extend(OLD_TOKENS);
    let _ = herdr.clear_labels(pane, &keys);
}

/// A Space is a group of terminals by label; closing its terminals is what
/// removes it, so there is nothing left to clear.
pub fn clear_workspace(_herdr: &Herdr, _workspace: &str) {}
