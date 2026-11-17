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

//! The order a Herdr 1 sidebar showed agents and Spaces in. herdr-core draws
//! nothing, so nothing consumes it now; what is left is kept only until it is
//! pruned, and `hp_rank` is the one label a reader still uses.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::herdr::{Agent, Herdr, Workspace};

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
///
/// With lanes (`docs/lanes.md` §9) the key carries the lane's rank between the
/// `!0!` coordinator marker and the pane, so coordinators sort by lane and
/// panes break ties inside a lane. The rank is fixed-width: keys are sorted as
/// plain strings, and `10` would otherwise sort before `2`. `None` — a project
/// with no registry — keeps today's two-part key exactly.
pub fn coordinator_key(slug: &str, lane_rank: Option<usize>, pane: &str) -> String {
    match lane_rank {
        Some(rank) => format!("{slug}!0!{rank:02}!{pane}"),
        None => format!("{slug}!0!{pane}"),
    }
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

pub fn report(herdr: &Herdr, _kind: &str, id: &str, tokens: &Tokens) {
    if id.is_empty() {
        return;
    }
    let set: Vec<(String, String)> =
        tokens.iter().filter_map(|(name, value)| value.as_ref().map(|value| (name.clone(), value.clone()))).collect();
    let _ = herdr.set_labels(id, &set);
    let clear: Vec<&str> = tokens.iter().filter(|(_, value)| value.is_none()).map(|(name, _)| name.as_str()).collect();
    let _ = herdr.clear_labels(id, &clear);
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
