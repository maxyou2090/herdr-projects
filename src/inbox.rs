//! Inbox items: events the ticker leaves for the coordinator.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::project::{self, Project};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Item {
    pub id: String,
    pub kind: String,
    pub subject: String,
    pub created: String,
    pub summary: String,
    /// What happened, in a few words this binary chose ("merged", "blocked on
    /// a prompt"). The nudge line is built from subjects and events only, never
    /// from summaries, which may quote text from reports or GitHub.
    pub event: String,
    /// Empty except for `routine` items.
    #[serde(skip)]
    pub body: String,
    /// The lane this item is addressed to: its **id** (`docs/lanes.md` §4.3).
    /// Empty on every item written before lanes, and an empty lane reads as the
    /// default lane's (§10). `inbox::write_in` is the writer for items whose
    /// thread has a lane; project-level events stay empty.
    pub lane: String,
}

fn inbox_dir(project: &Project) -> PathBuf {
    project.dir().join("inbox")
}

fn parse(text: &str) -> Option<Item> {
    let rest = text.strip_prefix("+++\n")?;
    let (front, body) = rest.split_once("\n+++\n").or_else(|| Some((rest.strip_suffix("\n+++")?, "")))?;
    let mut item: Item = toml::from_str(front).ok()?;
    item.body = body.trim_matches('\n').to_string();
    Some(item)
}

/// File-name-safe form of a subject (a thread id, routine name, machine label).
pub fn safe_subject(subject: &str) -> String {
    let cleaned: String = subject
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' })
        .take(40)
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() { "item".to_string() } else { cleaned }
}

/// Writes one item to the default lane. The id is
/// `<UTC timestamp>-<kind>-<subject>-<n>`, where `<n>` is a counter allocated
/// under the project lock, so two events in one tick never share a name.
/// `event` is a fixed phrase from this binary. `body` is empty except for
/// `routine` items.
pub fn write(project: &Project, kind: &str, subject: &str, event: &str, summary: &str, body: &str) -> Result<String> {
    write_in(project, "", kind, subject, event, summary, body)
}

/// The same, addressed to one lane. `lane` is a lane **id**; empty is the
/// default lane, which is what every project-level event (`session`, `space`,
/// `outage`, `routine`, `rename`, `config-error`) writes.
pub fn write_in(project: &Project, lane: &str, kind: &str, subject: &str, event: &str, summary: &str, body: &str) -> Result<String> {
    let _lock = project.lock()?;
    let counter_path = project.state_dir().join("inbox-counter.json");
    let n: u64 = project::read_json::<u64>(&counter_path).unwrap_or(0) + 1;
    project::write_json(&counter_path, &n)?;
    let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ").to_string();
    let id = format!("{stamp}-{kind}-{}-{n}", safe_subject(subject));
    let item = Item {
        id: id.clone(),
        kind: kind.to_string(),
        subject: subject.to_string(),
        created: project::now(),
        // One line, no control characters: summaries are printed in the digest.
        summary: summary.chars().map(|c| if c.is_control() { ' ' } else { c }).collect(),
        event: event.to_string(),
        body: String::new(),
        lane: lane.to_string(),
    };
    let mut text = format!("+++\n{}+++\n", toml::to_string(&item)?);
    if !body.is_empty() {
        text.push('\n');
        text.push_str(body.trim_end());
        text.push('\n');
    }
    project::write_atomic(&inbox_dir(project).join(format!("{id}.md")), text.as_bytes())?;
    Ok(id)
}

/// The lane of the thread an item is about, from its record: the id to pass
/// to [`write_in`]. An absent record or an empty `lane` field is the default
/// lane, which is exactly what the empty string means.
pub fn lane_of_thread(project: &Project, id: &str) -> String {
    crate::thread::load(project, id).map(|thread| thread.lane).unwrap_or_default()
}

/// Deletes handled items older than `days`.
pub fn prune_done(project: &Project, days: u64) {
    let Ok(entries) = std::fs::read_dir(inbox_dir(project).join("done")) else {
        return;
    };
    let limit = std::time::Duration::from_secs(days * 24 * 3600);
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > limit);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Unhandled items, oldest first (ids start with a UTC timestamp).
pub fn unhandled(project: &Project) -> Vec<Item> {
    let Ok(entries) = std::fs::read_dir(inbox_dir(project)) else {
        return Vec::new();
    };
    let mut items: Vec<Item> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".md"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|text| parse(&text))
        .collect();
    items.sort_by(|a, b| a.id.cmp(&b.id));
    items
}

/// The unhandled items addressed to one lane. An empty item lane is the
/// default lane (§10). Without a registry nothing is filtered at all — that is
/// the zero-change promise for projects that never used lanes.
pub fn unhandled_in(project: &Project, lane: &str) -> Vec<Item> {
    let items = unhandled(project);
    if !crate::lanes::split_in_use(project) {
        return items;
    }
    let default_lane = crate::lanes::default_lane_id(project);
    let lane = if lane.is_empty() { default_lane.clone() } else { lane.to_string() };
    items
        .into_iter()
        .filter(|item| if item.lane.is_empty() { lane == default_lane } else { item.lane == lane })
        .collect()
}

/// The seen set of the default lane. Without a registry this is the old
/// single `.state/inbox-seen.json`; with one it is that lane's shard.
pub fn seen(project: &Project) -> BTreeSet<String> {
    seen_in(project, &crate::lanes::default_lane_id(project))
}

/// The seen set of one lane. With a `lanes.json` every lane gets its own
/// `.state/inbox-seen/<lane>.json`; without one the old single file is used
/// and no directory is ever created (§4.3, §10).
pub fn seen_in(project: &Project, lane: &str) -> BTreeSet<String> {
    if !crate::lanes::split_in_use(project) {
        return project::read_json(&project.state_dir().join("inbox-seen.json")).unwrap_or_default();
    }
    let lane = if lane.is_empty() { crate::lanes::default_lane_id(project) } else { lane.to_string() };
    project::read_json(&seen_path(project, &lane)).unwrap_or_default()
}

fn seen_path(project: &Project, lane: &str) -> PathBuf {
    project.state_dir().join("inbox-seen").join(format!("{}.json", safe_subject(lane)))
}

/// Records that `context` showed these items, so they are nudged once only.
/// The default lane's set.
pub fn mark_seen(project: &Project, ids: &[String]) -> Result<()> {
    mark_seen_in(project, &crate::lanes::default_lane_id(project), ids)
}

/// Records that one lane's `context` showed these items. The live set used to
/// prune old ids is that same lane's [`unhandled_in`], never the project-wide
/// one: pruning against another lane's live set would delete its seen records.
pub fn mark_seen_in(project: &Project, lane: &str, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let _lock = project.lock()?;
    let mut all = seen_in(project, lane);
    all.extend(ids.iter().cloned());
    // Ids of items that no longer exist are dropped so the file stays small,
    // within this lane only.
    let live: BTreeSet<String> = unhandled_in(project, lane).into_iter().map(|i| i.id).collect();
    all.retain(|id| live.contains(id));
    if !crate::lanes::split_in_use(project) {
        return project::write_json(&project.state_dir().join("inbox-seen.json"), &all);
    }
    let lane = if lane.is_empty() { crate::lanes::default_lane_id(project) } else { lane.to_string() };
    let dir = project.state_dir().join("inbox-seen");
    std::fs::create_dir_all(&dir)?;
    project::write_json(&seen_path(project, &lane), &all)
}

/// An item id is also a file name, so it is checked before any path is built.
fn validate_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !ok || id.contains("..") {
        bail!("`{id}` is not an inbox item id");
    }
    Ok(())
}

/// Moves items to `inbox/done/`. Returns how many moved.
pub fn done(project: &Project, ids: &[String], all: bool) -> Result<usize> {
    let ids: Vec<String> = if all {
        unhandled(project).into_iter().map(|i| i.id).collect()
    } else {
        ids.to_vec()
    };
    for id in &ids {
        validate_id(id)?;
    }
    let _lock = project.lock()?;
    let dir = inbox_dir(project);
    let mut moved = 0;
    for id in &ids {
        let from = dir.join(format!("{id}.md"));
        if !from.is_file() {
            eprintln!("no unhandled item `{id}`");
            continue;
        }
        std::fs::rename(&from, dir.join("done").join(format!("{id}.md")))?;
        moved += 1;
    }
    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_item(project: &Project, id: &str, body: &str) {
        let text = format!(
            "+++\nid = \"{id}\"\nkind = \"routine\"\nsubject = \"r\"\ncreated = \"2026-09-17T00:00:00Z\"\nsummary = \"s\"\n+++\n{body}"
        );
        std::fs::write(inbox_dir(project).join(format!("{id}.md")), text).unwrap();
    }

    #[test]
    fn lists_marks_seen_and_moves_to_done() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        write_item(&project, "20260917T000002Z-routine-r-2", "\nbody text\n");
        write_item(&project, "20260917T000001Z-routine-r-1", "");
        let items = unhandled(&project);
        assert_eq!(items.len(), 2);
        assert!(items[0].id.ends_with("-1"));
        assert_eq!(items[1].body, "body text");

        mark_seen(&project, &[items[0].id.clone()]).unwrap();
        assert_eq!(seen(&project).len(), 1);

        assert_eq!(done(&project, &[items[0].id.clone()], false).unwrap(), 1);
        assert_eq!(unhandled(&project).len(), 1);
        assert!(inbox_dir(&project).join("done").join(format!("{}.md", items[0].id)).is_file());
        assert_eq!(done(&project, &[], true).unwrap(), 1);
        assert!(unhandled(&project).is_empty());
    }

    #[test]
    fn two_events_in_one_tick_get_two_items() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        let a = write(&project, "thread-state", "t-0001", "idle", "first", "").unwrap();
        let b = write(&project, "thread-state", "t-0001", "idle", "second\nline", "").unwrap();
        assert_ne!(a, b);
        assert!(a.ends_with("-thread-state-t-0001-1"), "{a}");
        assert!(b.ends_with("-thread-state-t-0001-2"), "{b}");
        let items = unhandled(&project);
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].summary, "second line");
        assert!(items.iter().all(|i| i.body.is_empty()));
        // A written item can be marked done by its id.
        assert_eq!(done(&project, &[a], false).unwrap(), 1);
    }

    #[test]
    fn routine_items_carry_a_body_and_subjects_are_made_file_safe() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        let id = write(&project, "outage", "Elias MacBook/../x", "unreachable", "down", "").unwrap();
        assert!(id.contains("-outage-elias-macbook----x-"), "{id}");
        write(&project, "routine", "nightly", "due", "due", "Check the build.\n\n```\nout\n```").unwrap();
        let routine = unhandled(&project).into_iter().find(|i| i.kind == "routine").unwrap();
        assert!(routine.body.starts_with("Check the build."));
        assert!(routine.body.ends_with("```"));
    }

    fn two_lane_project() -> (tempfile::TempDir, Project) {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        crate::lanes::Registry::ensure_written(&project).unwrap();
        let mut registry = crate::lanes::Registry::load(&project).unwrap();
        registry.create("books", "收藏");
        registry.save(&project).unwrap();
        (root, project)
    }

    #[test]
    fn write_in_addresses_one_lane_and_lane_of_thread_reads_the_record() {
        let (_root, project) = two_lane_project();
        crate::thread::allocate(&project, |t| {
            t.title = "Task".into();
            t.lane = "c-2".into();
        })
        .unwrap();
        let id = write_in(&project, "c-2", "thread-state", "t-0001", "idle", "s", "").unwrap();
        let item = unhandled(&project).into_iter().find(|i| i.id == id).unwrap();
        assert_eq!(item.lane, "c-2");
        assert_eq!(lane_of_thread(&project, "t-0001"), "c-2");
        assert_eq!(lane_of_thread(&project, "t-9999"), "", "an absent record is the default lane");
        // A project-level event stays empty, i.e. the default lane.
        let id = write(&project, "session", "session", "restarted", "s", "").unwrap();
        assert_eq!(unhandled(&project).into_iter().find(|i| i.id == id).unwrap().lane, "");
    }

    #[test]
    fn seen_state_is_one_file_without_a_registry_and_one_file_per_lane_with_one() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        write_item(&project, "20260917T000001Z-routine-r-1", "");
        mark_seen(&project, &["20260917T000001Z-routine-r-1".to_string()]).unwrap();
        assert!(project.state_dir().join("inbox-seen.json").is_file());
        assert!(!project.state_dir().join("inbox-seen").exists(), "no registry, no directory");

        // A registry switches to per-lane shards; the old file is neither read
        // nor written any more, and reading a lane writes no directory yet.
        let (_root, project) = two_lane_project();
        assert!(seen_in(&project, "c-1").is_empty());
        assert!(!project.state_dir().join("inbox-seen").exists());
        let a = write_in(&project, "c-1", "thread-state", "t-0001", "idle", "a", "").unwrap();
        mark_seen(&project, std::slice::from_ref(&a)).unwrap();
        assert!(project.state_dir().join("inbox-seen/c-1.json").is_file());
        assert!(seen(&project).contains(&a));
        assert!(!project.state_dir().join("inbox-seen.json").exists(), "the old file is not written");
        assert!(!project.state_dir().join("inbox-seen/c-2.json").exists(), "only the lane that read");
    }

    #[test]
    fn marking_one_lanes_items_seen_leaves_another_lanes_items_and_seen_alone() {
        let (_root, project) = two_lane_project();
        let a = write_in(&project, "c-1", "thread-state", "t-0001", "idle", "a", "").unwrap();
        let b = write_in(&project, "c-2", "thread-state", "t-0002", "idle", "b", "").unwrap();
        assert_eq!(unhandled_in(&project, "c-1").len(), 1);
        assert_eq!(unhandled_in(&project, "c-2").len(), 1);
        // Lane A marks its item seen: B's item is untouched, and B's seen set
        // is not created.
        mark_seen_in(&project, "c-1", std::slice::from_ref(&a)).unwrap();
        assert!(seen_in(&project, "c-1").contains(&a));
        assert!(!seen_in(&project, "c-2").contains(&a));
        assert!(!project.state_dir().join("inbox-seen/c-2.json").exists());
        assert_eq!(unhandled_in(&project, "c-2").len(), 1, "B's item is still unhandled");
        // B marks its own: A's file still exists and still has A's id. Pruning
        // against B's live set must not delete it.
        mark_seen_in(&project, "c-2", std::slice::from_ref(&b)).unwrap();
        assert!(seen_in(&project, "c-1").contains(&a), "A's seen set survives B's mark_seen");
        assert!(seen_in(&project, "c-2").contains(&b));
        // Marking A again after B also keeps both.
        mark_seen_in(&project, "c-1", std::slice::from_ref(&a)).unwrap();
        assert!(seen_in(&project, "c-1").contains(&a));
        assert!(seen_in(&project, "c-2").contains(&b));
    }

    #[test]
    fn hostile_ids_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let project = project::create(root.path(), "demo", "", vec![]).unwrap();
        for bad in ["../PROJECT", "a/b", "", ".hidden", "x..y"] {
            assert!(done(&project, &[bad.to_string()], false).is_err(), "{bad}");
        }
    }
}
