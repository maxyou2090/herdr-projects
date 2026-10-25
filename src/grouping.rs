//! Groups Herdr's sidebar by project with nothing but order and one head row
//! per project, the same in the agents and the Spaces list. The head is the
//! project's own row: its home Space and its coordinator show the project
//! name in bold, the rows after them are Herdr's own. Every row a card shows
//! is that item's own content, so focus and selection light only the item.
//!
//! The bold comes from a config rule keyed on an invisible mark in the name:
//! [`HOME_MARK`] ends every home Space label, [`HEAD_MARK`] every coordinator
//! display name. A mark in the name, not a list of names in the config,
//! because a client draws another machine's rows with its own config.
//!
//! Agents sort by `$hp_group` in the agent view; Spaces are moved into one
//! block per project with `workspace.move`. Only Spaces of this root's own
//! projects are ever moved, and a block that already stands somewhere is
//! left alone: another root's ticker sharing the session settles beside it
//! instead of the two trading the head of the list every tick. Anything in
//! no project comes last.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::herdr::{Agent, CALL_TIMEOUT, Herdr, Workspace};

/// Ends every home Space label; the Space rows make a label that contains it
/// the project's bold head.
pub const HOME_MARK: char = '\u{2800}';
/// Ends every coordinator's display name; the agent rows make a name that
/// contains it the project's bold head. Zero-width, so it takes no cell.
pub const HEAD_MARK: char = '\u{200B}';
/// The group key of whatever belongs to no project: sorts after every slug.
const OTHER: &str = "~";

/// Every token this module writes.
pub fn tokens() -> Vec<String> {
    ["hp_sub", "hp_group"].map(String::from).to_vec()
}

/// Tokens of earlier layouts: the 0.2.17/0.2.18 headings and the 0.2.19
/// rails. Cleared with a pane's or Space's own tokens, and otherwise left to
/// expire (they had a TTL and have no rows any more).
pub fn legacy() -> Vec<String> {
    let mut all: Vec<String> = ["hp_top", "hp_other", "hp_note", "hp_tail", "hp", "hp_state", "hp_activity", "hp_gap", "hp_home"].map(String::from).to_vec();
    for slot in ["top", "con", "sub", "end"] {
        for rail in ["n", "w", "i", "o"] {
            all.push(format!("hp_{slot}_{rail}"));
        }
    }
    all
}

/// One agent of a project: its pane, sort key and sub-line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaneRow {
    pub pane: String,
    pub key: String,
    /// `review · report`, `~40% · Writing tests`, or empty.
    pub sub: String,
}

/// One project's part of a session, as its tick saw it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectPart {
    /// The project's agents in panel order.
    pub panes: Vec<PaneRow>,
    /// The home Space.
    pub home: String,
    /// The project's Spaces, home first, then each thread's repository Space
    /// and its own.
    pub spaces: Vec<String>,
}

/// Every project's part in one Herdr session, keyed by slug.
pub type Parts = BTreeMap<String, ProjectPart>;

/// `!` sorts before every slug character, so keys sort as their slugs do.
pub fn coordinator_key(slug: &str, pane: &str) -> String {
    format!("{slug}!0!{pane}")
}

pub fn thread_key(slug: &str, rank: u8, id: &str) -> String {
    format!("{slug}!1!{rank}!{id}")
}

/// Tokens for one card: `Some` sets, `None` clears.
pub type Tokens = Vec<(String, Option<String>)>;

#[derive(Debug, Default, PartialEq)]
pub struct Plan {
    pub agents: Vec<(String, Tokens)>,
}

/// The agents' half of one session's layout: projects by slug, each in its
/// own key order, then every other agent in Herdr's order. The Spaces' half
/// is [`owned`] and [`moves`], which read the live list each time.
pub fn plan(parts: &Parts, agents: &[Agent]) -> Plan {
    let mut out = Plan::default();
    if parts.is_empty() {
        return out;
    }
    let mut owner: HashMap<&str, &PaneRow> = HashMap::new();
    for part in parts.values() {
        for row in &part.panes {
            owner.entry(row.pane.as_str()).or_insert(row);
        }
    }
    let mut rows: Vec<(String, &str, &str)> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| match owner.get(a.pane_id.as_str()) {
            Some(row) => (row.key.clone(), a.pane_id.as_str(), row.sub.as_str()),
            None => (format!("{OTHER}!{i:04}"), a.pane_id.as_str(), ""),
        })
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for (key, pane, sub) in rows {
        let tokens = vec![("hp_sub".to_string(), (!sub.is_empty()).then(|| sub.to_string())), ("hp_group".to_string(), Some(key))];
        out.agents.push((pane.to_string(), tokens));
    }

    out
}

/// The Spaces of this root's projects in sidebar order — projects by slug,
/// each project's Spaces in its `part` order — keeping only the ones Herdr
/// lists. A Space two projects both claim belongs to the first (slugs sort).
pub fn owned(parts: &Parts, current: &[String]) -> Vec<String> {
    let listed: HashSet<&str> = current.iter().map(String::as_str).collect();
    let mut out: Vec<String> = Vec::new();
    for part in parts.values() {
        for id in &part.spaces {
            if listed.contains(id.as_str()) && !out.iter().any(|x| x == id) {
                out.push(id.clone());
            }
        }
    }
    out
}

/// Whether this root's Spaces already stand as one block, in [`owned`]'s
/// order, wherever in the list that block happens to be. The block's place
/// is no part of the target: another root's ticker may hold the head of the
/// list, and this one leaves it be instead of the two trading places every
/// tick. A Space of ours the list does not show (closed, not yet opened)
/// does not count.
pub fn settled(current: &[String], own: &[String]) -> bool {
    let positions: Vec<usize> = own.iter().filter_map(|id| current.iter().position(|x| x == id)).collect();
    positions.windows(2).all(|w| w[1] == w[0] + 1)
}

/// The `workspace.move` calls that turn `current` into this root's layout:
/// its own Spaces one block, in order, ahead of everything else, with the
/// rest keeping their relative order. Nothing moves while the block already
/// stands somewhere ([`settled`]); when it does not, each Space in turn goes
/// to its index, so at most one call per misplaced Space — and every moved
/// Space is one of ours, so a second root sharing the session is never
/// reordered, only shifted, and the two alternating `apply`s settle instead
/// of swinging the sidebar back and forth.
pub fn moves(current: &[String], own: &[String]) -> Vec<(String, usize)> {
    if settled(current, own) {
        return Vec::new();
    }
    let mut now = current.to_vec();
    let mut out = Vec::new();
    for (i, id) in own.iter().enumerate() {
        let Some(at) = now.iter().position(|x| x == id) else {
            continue;
        };
        if at != i {
            let moved = now.remove(at);
            now.insert(i, moved);
            out.push((id.clone(), i));
        }
    }
    out
}

/// Home Spaces whose label lacks [`HOME_MARK`] (made before 0.2.19, or
/// renamed by hand): `(id, label with the mark)`.
pub fn unmarked_homes(parts: &Parts, workspaces: &[Workspace]) -> Vec<(String, String)> {
    let homes: HashSet<&str> = parts.values().map(|p| p.home.as_str()).filter(|h| !h.is_empty()).collect();
    workspaces
        .iter()
        .filter(|w| homes.contains(w.workspace_id.as_str()))
        .filter(|w| !w.label.is_empty() && !w.label.ends_with(HOME_MARK))
        .map(|w| (w.workspace_id.clone(), format!("{}{HOME_MARK}", w.label)))
        .collect()
}

/// Tokens sent per `(kind, id)`, so an unchanged card is re-sent only to keep
/// its TTL alive.
#[derive(Default)]
pub struct Sent(HashMap<(String, String), (Tokens, Instant)>);

const RESEND: Duration = Duration::from_millis(crate::sidebar::TOKEN_TTL_MS / 3);

impl Sent {
    fn due(&mut self, key: (String, String), tokens: &Tokens) -> bool {
        match self.0.get(&key) {
            Some((last, at)) if last == tokens && at.elapsed() < RESEND => false,
            _ => {
                self.0.insert(key, (tokens.clone(), Instant::now()));
                true
            }
        }
    }
}

/// Herdr takes at most 16 tokens per report.
const PER_REPORT: usize = 16;

pub fn report(herdr: &Herdr, kind: &str, id: &str, tokens: &Tokens) {
    let ttl = crate::sidebar::TOKEN_TTL_MS.to_string();
    for chunk in tokens.chunks(PER_REPORT) {
        let mut args: Vec<String> = [kind, "report-metadata", id, "--source", crate::herdr::SOURCE, "--ttl-ms", &ttl].iter().map(|s| s.to_string()).collect();
        for (name, value) in chunk {
            match value {
                Some(value) => args.extend(["--token".to_string(), format!("{name}={value}")]),
                None => args.extend(["--clear-token".to_string(), name.clone()]),
            }
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = herdr.call(&args, CALL_TIMEOUT);
    }
}

/// Clears every token this module and the earlier layouts wrote, plus the
/// lane claim: a pane that is no longer the plugin's must not go on claiming
/// a lane (`docs/lanes.md` §5.1).
pub fn clear(herdr: &Herdr, kind: &str, id: &str) {
    if id.is_empty() {
        return;
    }
    let all: Tokens = tokens()
        .into_iter()
        .chain(std::iter::once(crate::lanes::TOKEN.to_string()))
        .chain(legacy())
        .map(|t| (t, None))
        .collect();
    report(herdr, kind, id, &all);
}

/// Lays out one session: marks home Spaces, moves Spaces into blocks and
/// writes every agent's sort key and sub-line. Nothing when no project lives
/// in the session.
pub fn apply(herdr: &Herdr, socket: &str, parts: &Parts, agents: &[Agent], sent: &mut Sent) {
    if parts.is_empty() {
        return;
    }
    let mut workspaces = herdr.workspace_list().unwrap_or_default();
    for (id, label) in unmarked_homes(parts, &workspaces) {
        if herdr.workspace_rename(&id, &label).is_ok()
            && let Some(w) = workspaces.iter_mut().find(|w| w.workspace_id == id)
        {
            w.label = label;
        }
    }
    let layout = plan(parts, agents);
    let current: Vec<String> = workspaces.iter().map(|w| w.workspace_id.clone()).collect();
    for (id, index) in moves(&current, &owned(parts, &current)) {
        if herdr.workspace_move(&id, index).is_err() {
            break;
        }
    }
    for (pane, tokens) in &layout.agents {
        if sent.due((format!("{socket} pane"), pane.clone()), tokens) {
            report(herdr, "pane", pane, tokens);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::WorkspaceWorktree;

    fn agent(pane: &str) -> Agent {
        Agent { pane_id: pane.into(), ..Agent::default() }
    }

    fn space(id: &str, repo: &str, linked: bool) -> Workspace {
        let worktree = (!repo.is_empty()).then(|| WorkspaceWorktree { repo_key: repo.into(), checkout_path: String::new(), is_linked_worktree: linked });
        Workspace { workspace_id: id.into(), label: id.into(), worktree, ..Workspace::default() }
    }

    fn row(pane: &str, key: String, sub: &str) -> PaneRow {
        PaneRow { pane: pane.into(), key, sub: sub.into() }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// Herdr's side of the calls: a Space comes out of the list and goes back
    /// at the index given, a gap in the list before the removal.
    fn herdr_moves(list: &mut Vec<String>, steps: &[(String, usize)]) {
        for (id, i) in steps {
            let at = list.iter().position(|x| x == id).unwrap();
            let target = if at < *i { *i - 1 } else { *i };
            let moved = list.remove(at);
            list.insert(target, moved);
        }
    }

    fn parts() -> Parts {
        let mut parts = Parts::new();
        parts.insert("beta".into(), ProjectPart { panes: vec![row("b1", coordinator_key("beta", "b1"), "")], home: "wb".into(), spaces: vec!["wb".into()] });
        parts.insert(
            "alpha".into(),
            ProjectPart {
                panes: vec![
                    row("a2", thread_key("alpha", 4, "t-0002"), "~40% · Writing"),
                    row("a1", coordinator_key("alpha", "a1"), ""),
                    row("a3", thread_key("alpha", 1, "t-0003"), "review · report"),
                ],
                home: "wa".into(),
                spaces: vec!["wa".into(), "repo".into(), "wt2".into(), "wt3".into()],
            },
        );
        parts
    }

    fn get<'a>(tokens: &'a Tokens, name: &str) -> Option<&'a str> {
        tokens.iter().find(|(n, _)| n == name).and_then(|(_, v)| v.as_deref())
    }

    #[test]
    fn agents_sort_into_projects_with_their_own_sub_lines_and_others_last() {
        let agents: Vec<Agent> = ["x1", "a2", "b1", "a3", "x2", "a1"].into_iter().map(agent).collect();
        let layout = plan(&parts(), &agents);
        let order: Vec<&str> = layout.agents.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(order, ["a1", "a3", "a2", "b1", "x1", "x2"]);
        let t: HashMap<&str, &Tokens> = layout.agents.iter().map(|(p, t)| (p.as_str(), t)).collect();
        assert_eq!(get(t["a3"], "hp_sub"), Some("review · report"));
        assert_eq!(get(t["a1"], "hp_sub"), None, "an empty sub-line is cleared, so no row is drawn");
        assert!(t["a1"].iter().any(|(n, v)| n == "hp_sub" && v.is_none()));
        // No card carries a row of ours besides its own sub-line.
        assert!(layout.agents.iter().all(|(_, t)| t.iter().all(|(n, _)| n == "hp_sub" || n == "hp_group")));
        // The sort key the agent view uses reproduces this order.
        let keys: Vec<&str> = layout.agents.iter().map(|(_, t)| get(t, "hp_group").unwrap()).collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn spaces_gather_into_project_blocks_and_others_keep_their_order_last() {
        let list = s(&["mine", "wt3", "wb", "repo", "wa", "wt2", "other2"]);
        let own = owned(&parts(), &list);
        assert_eq!(own, s(&["wa", "repo", "wt2", "wt3", "wb"]));
        let steps = moves(&list, &own);
        assert!(steps.iter().all(|(id, _)| own.contains(id)), "no foreign Space is moved");
        let mut after = list.clone();
        herdr_moves(&mut after, &steps);
        assert_eq!(after, s(&["wa", "repo", "wt2", "wt3", "wb", "mine", "other2"]));
    }

    #[test]
    fn a_block_standing_somewhere_needs_no_move() {
        // Another root's three Spaces hold the head of the list; ours stand
        // behind them, one block in order. That is the target: no call.
        let list = s(&["wP2", "wP3", "wP4", "wa", "repo", "wt2", "wt3", "wb", "mine"]);
        assert!(settled(&list, &owned(&parts(), &list)));
        assert!(moves(&list, &owned(&parts(), &list)).is_empty());
    }

    #[test]
    fn a_foreign_space_inside_the_block_is_pushed_out_again() {
        let list = s(&["wa", "mine", "repo", "wt2", "wt3", "wb"]);
        let steps = moves(&list, &owned(&parts(), &list));
        let mut after = list.clone();
        herdr_moves(&mut after, &steps);
        assert_eq!(after, s(&["wa", "repo", "wt2", "wt3", "wb", "mine"]));
    }

    #[test]
    fn unmarked_home_labels_get_the_mark() {
        let mut marked = space("wb", "", false);
        marked.label = format!("Beta{HOME_MARK}");
        let mut plain = space("wa", "", false);
        plain.label = "Alpha".into();
        assert_eq!(unmarked_homes(&parts(), &[plain, marked, space("repo", "r", false)]), [("wa".to_string(), format!("Alpha{HOME_MARK}"))]);
    }

    #[test]
    fn clearing_covers_every_earlier_layout_and_fits_herdrs_token_limit_in_chunks() {
        let all = legacy();
        for old in ["hp_top", "hp_top_w", "hp_end_o", "hp_con_n", "hp_sub_i", "hp_gap", "hp_home", "hp_activity"] {
            assert!(all.iter().any(|t| t == old), "{old}");
        }
        assert!(tokens().len() + all.len() > PER_REPORT, "the chunking in report() is needed");
        assert!(all.iter().all(|t| !tokens().contains(t)));
    }

    #[test]
    fn moves_are_minimal_and_reach_the_block() {
        let own = s(&["a1", "a2"]);
        let list = s(&["b", "a2", "x", "a1"]);
        let steps = moves(&list, &own);
        assert_eq!(steps.len(), 2, "one call per misplaced Space");
        let mut after = list.clone();
        herdr_moves(&mut after, &steps);
        assert_eq!(after, s(&["a1", "a2", "b", "x"]));
        assert!(moves(&after, &own).is_empty());
    }

    #[test]
    fn two_roots_alternating_settle_instead_of_trading_the_head() {
        // A second root's projects in the same session: a skill trial with
        // three Spaces of its own.
        let mut theirs = Parts::new();
        theirs.insert("skills".into(), ProjectPart { panes: vec![], home: "wP2".into(), spaces: s(&["wP2", "wP3", "wP4"]) });
        let mine = parts();
        // Both roots scrambled around one foreign Space.
        let mut list = s(&["wP3", "wt3", "wb", "wP2", "mine", "wa", "repo", "wt2", "wP4"]);
        let mut quiet = 0;
        for _ in 0..8 {
            let before = list.clone();
            let mut moved = false;
            for root in [&mine, &theirs] {
                let own = owned(root, &list);
                let foreign: Vec<String> = list.iter().filter(|id| !own.contains(*id)).cloned().collect();
                let steps = moves(&list, &own);
                moved |= !steps.is_empty();
                herdr_moves(&mut list, &steps);
                // Whoever's Spaces move, the ones this root does not own keep
                // their relative order, the other root's block included.
                let kept: Vec<String> = list.iter().filter(|id| foreign.contains(id)).cloned().collect();
                assert_eq!(kept, foreign);
            }
            if moved {
                assert_ne!(before, list, "Spaces moved without the order changing");
                quiet = 0;
            } else {
                quiet += 1;
            }
        }
        // Converged: neither root moves any more, and each stands as one
        // block in order, the root that settled first keeping the head.
        assert!(quiet >= 5, "the two roots kept trading places: {list:?}");
        assert_eq!(list, s(&["wP2", "wP3", "wP4", "wa", "repo", "wt2", "wt3", "wb", "mine"]));
    }

    #[test]
    fn two_tickers_on_one_session_stop_calling_workspace_move() {
        use crate::runner::fake::{FakeRunner, ok};
        use std::cell::RefCell;
        use std::rc::Rc;

        // The session both roots share, scripted: `workspace list` answers
        // with the live order, and each `workspace.move` (a socket call) is
        // applied to it the way Herdr applies one.
        let list = Rc::new(RefCell::new(s(&["wP3", "wt3", "wb", "wP2", "mine", "wa", "repo", "wt2", "wP4"])));
        let runner = FakeRunner::new();
        {
            let list = list.clone();
            runner.on_fn(
                |cmd| cmd.display().contains("workspace list"),
                move |_| {
                    let spaces: Vec<String> = list.borrow().iter().map(|id| format!(r#"{{"workspace_id":"{id}"}}"#)).collect();
                    Ok(ok(&format!(r#"{{"result":{{"workspaces":[{}]}}}}"#, spaces.join(","))))
                },
            );
        }
        let herdr = Herdr::new("herdr", "/tmp/herdr-grouping.sock", &runner);
        let mut theirs = Parts::new();
        theirs.insert("skills".into(), ProjectPart { panes: vec![], home: "wP2".into(), spaces: s(&["wP2", "wP3", "wP4"]) });
        let mut mine_sent = Sent::default();
        let mut their_sent = Sent::default();
        let mut seen = 0;
        let mut quiet = 0;
        for _ in 0..8 {
            apply(&herdr, "sock", &parts(), &[], &mut mine_sent);
            apply(&herdr, "sock", &theirs, &[], &mut their_sent);
            let requests = runner.socket_requests.borrow().clone();
            let mut calls = 0;
            for (_, line) in &requests[seen..] {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                if value["method"] != "workspace.move" {
                    continue;
                }
                calls += 1;
                let id = value["params"]["workspace_id"].as_str().unwrap().to_string();
                let i = value["params"]["insert_index"].as_u64().unwrap() as usize;
                let mut live = list.borrow_mut();
                let at = live.iter().position(|x| *x == id).unwrap();
                let target = if at < i { i - 1 } else { i };
                let moved = live.remove(at);
                live.insert(target, moved);
            }
            seen = requests.len();
            quiet = if calls > 0 { 0 } else { quiet + 1 };
        }
        assert!(quiet >= 5, "workspace.move kept firing: {:?}", list.borrow());
        assert_eq!(*list.borrow(), s(&["wP2", "wP3", "wP4", "wa", "repo", "wt2", "wt3", "wb", "mine"]));
    }

    #[test]
    fn no_project_means_no_layout() {
        assert_eq!(plan(&Parts::new(), &[agent("x")]), Plan::default());
        let list = s(&["w"]);
        assert!(owned(&Parts::new(), &list).is_empty());
        assert!(moves(&list, &owned(&Parts::new(), &list)).is_empty());
    }
}
