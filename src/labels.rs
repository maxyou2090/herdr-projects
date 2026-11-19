//! How a project's threads read: the words of a group, a row's one-line
//! display, and the labels (`hc label`) that carry them. Herdr 1 drew these
//! into its sidebar and tab bar; herdr-core draws nothing, so the same strings
//! are what `overview`, `list` and the digest print, and the rest are pane
//! labels. This module was `sidebar.rs` until t-0039: nothing here draws a
//! sidebar any more, only labels.

use crate::core::Core;
use crate::project::Project;
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

/// A thread's tab name: the id alone. The app shows a row as `<tab> <Space>`,
/// so the title belongs to the Space ([`thread_space_label`]) and lives there
/// once; the tab says which thread it is, without repeating the title.
pub fn thread_tab_label(id: &str) -> String {
    id.to_string()
}

/// A thread's Space name: its title, tightened for a Chinese title
/// ([`tight_cjk`]) and otherwise unchanged by the tab/display rework. A restart
/// recognises a thread's own Space by this label (`thread::recorded_own_space`,
/// `threads::place_tab`, `spaces::empty`), so it is the title on purpose.
pub fn thread_space_label(title: &str) -> String {
    tight_cjk(title)
}

/// Whether `label` names a thread's Space: the tightened title, or the title as
/// written — a Space made before the tightening rule keeps the old label, so a
/// restart must still recognise it (and renames it through `Core::name_space`).
pub fn thread_space_matches(label: &str, title: &str) -> bool {
    label == tight_cjk(title) || label == title
}

/// The Han ideographs: what [`tight_cjk`] reads as Chinese. Kana, Hangul and CJK
/// punctuation are deliberately outside, so only an ideograph beside a space
/// makes that space wrong.
fn is_han(c: char) -> bool {
    matches!(c as u32, 0x3007 | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F | 0x30000..=0x323AF)
}

/// Drops a space that touches a Chinese character, so a title reads
/// `今天1月abc课程` instead of `今天 1 月 abc 课程`; a space between two Latin
/// words stays, and nothing else changes (no space is added, punctuation and
/// case are kept). Both sides are checked, so `tab 用 id` tightens to
/// `tab用id`, and a run of spaces is dropped as a whole.
pub fn tight_cjk(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != ' ' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i] == ' ' {
            i += 1;
        }
        let before = start.checked_sub(1).map(|j| chars[j]).filter(|c| is_han(*c));
        let after = chars.get(i).copied().filter(|c| is_han(*c));
        if before.is_none() && after.is_none() {
            out.extend(std::iter::repeat_n(' ', i - start));
        }
    }
    out
}

/// A coordinator's tab name, in the Space the project owns. That Space is
/// called by the project's name; the tab is the one word that says what runs
/// there.
pub fn coordinator_tab_label() -> &'static str {
    "coordinator"
}

/// Row 1 of a thread, as `hp_display` carries it: its id and its title, one
/// space between (no `·`: a person reads `t-0047 线程命名tab用id`). The title is
/// tightened like the Space (no space beside a Chinese character) so the two
/// names read as one.
pub fn thread_display(thread: &Thread) -> String {
    format!("{} {}", thread.id, tight_cjk(&thread.title))
}

/// A coordinator's row name: the project's name, plus ` · <lane label>` when
/// the project has a lane registry (`docs/lanes.md` §9). The gate is the
/// registry, not the lane count: a project that never wrote `lanes.json` reads
/// as before, and a project with lanes shows them even for one lane — including
/// the default — so two coordinators are never two identical rows.
pub fn coordinator_display(project: &Project, lane: Option<&str>) -> String {
    let name = project.display_name();
    match crate::lanes::display_label(project, lane) {
        Some(label) => format!("{name} · {label}"),
        None => name,
    }
}

/// Reports one pane: its project, its display name and — for a coordinator
/// whose lane the registry names — its lane claim (`docs/lanes.md` §5.1).
/// `lane` is the id to write as `hp_lane`, or `None` for no claim: a thread
/// pane never claims a lane, and a project with no lane registry reports none.
pub fn report_pane(herdr: &Core, pane: &str, display: &str, slug: &str, _group: Group, lane: Option<&str>) {
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
pub fn clear_pane(herdr: &Core, pane: &str) {
    if pane.is_empty() {
        return;
    }
    let mut keys: Vec<&str> = vec![PROJECT_TOKEN, "hp_display", "hp_rank", crate::lanes::TOKEN];
    keys.extend(OLD_TOKENS);
    let _ = herdr.clear_labels(pane, &keys);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_threads_three_names_say_the_id_once_and_the_title_once() {
        let thread = Thread { id: "t-0047".into(), title: "线程命名 tab 用 id".into(), ..Thread::default() };
        assert_eq!(thread_tab_label(&thread.id), "t-0047", "the tab is the id alone");
        assert_eq!(thread_space_label(&thread.title), "线程命名tab用id", "the Space keeps the title, with no space beside Chinese");
        assert_eq!(thread_display(&thread), "t-0047 线程命名tab用id", "the display is id, one space, the tightened title");
        assert!(!thread_display(&thread).contains('·'), "the separator is a space, never `·`");
    }

    #[test]
    fn a_space_beside_a_chinese_character_goes_and_other_spaces_stay() {
        assert_eq!(tight_cjk("今天 1 月 abc 课程"), "今天1月abc课程");
        assert_eq!(thread_space_label("今天 1 月 abc 课程"), "今天1月abc课程", "a thread's Space label is the tightened title");
        let thread = Thread { id: "t-0052".into(), title: "今天 1 月 abc 课程".into(), ..Thread::default() };
        assert_eq!(thread_display(&thread), "t-0052 今天1月abc课程", "and so is hp_display");
        assert_eq!(tight_cjk("hello world"), "hello world", "two Latin words keep their space");
        assert_eq!(tight_cjk("tab 用 id"), "tab用id", "either side is enough to drop the space");
        assert_eq!(tight_cjk("修复：abc，def。"), "修复：abc，def。", "Chinese punctuation is kept as it is");
        assert_eq!(tight_cjk("a  中文"), "a中文", "a run of spaces is dropped as a whole");
        assert_eq!(tight_cjk(""), "", "an empty title is not an error");
        assert_eq!(tight_cjk("   "), "   ", "a title of spaces only is left alone and does not panic");
        assert_eq!(tight_cjk(" 中文 "), "中文", "every space beside Chinese goes, before or after");
        assert_eq!(tight_cjk("テスト サンプル"), "テスト サンプル", "kana is not Han, so its space stays");
        assert!(thread_space_matches("今天1月", "今天 1 月"), "a Space made before the rule still matches");
        assert!(thread_space_matches("今天 1 月", "今天 1 月"), "the title as written matches too");
        assert!(!thread_space_matches("别的标题", "今天 1 月"), "another title does not");
    }

    #[test]
    fn a_coordinators_row_still_reads_project_dot_lane() {
        // The thread rework left the coordinator's names alone: its row is the
        // project's display name, joined to a lane label with ` · `.
        let root = tempfile::tempdir().unwrap();
        let project = crate::project::create(&root.path().join("root"), "demo", vec![]).unwrap();
        assert_eq!(coordinator_display(&project, None), "Demo", "a project with no lane registry reads as before");
    }
}
