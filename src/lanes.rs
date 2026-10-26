//! Lanes: several coordinators in one project, and how a pane says which one
//! it is. See `docs/lanes.md`.
//!
//! A **lane** is one line of work inside one project: its own coordinator
//! instance, its own slice of TASKS.md, its own inbox. The registry
//! `.state/lanes.json` is the source of truth; `coordinator.json` is a *view*
//! of its default lane ([`Registry::default_coordinator`], used by
//! [`crate::project::Project::coordinator`]).
//!
//! Identity is **declared, not inferred**. [`resolve`] answers "which lane is
//! this pane?" through the five rules of `docs/lanes.md` §5.2, in order, and
//! it is three-valued: a pane that cannot be classified comes back
//! [`LaneResolution::Ambiguous`] rather than a guess. Rule 4 — several lanes
//! and no positive signal — is a *refusal*, and the `Coordinators:` line of
//! `hp context` ([`identity_line`]) is what makes that refusal visible every
//! turn (§5.4). The failure this exists for was a standing one: both identity
//! signals dead, nudges silently stopped for a week, nothing logged, no
//! transition to catch. So the line is printed whether or not anything
//! resolved, and an ambiguous entry carries the command that fixes it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::herdr::Agent;
use crate::paths::Ctx;
use crate::project::{self, Coordinator, Project};

/// The registry file, under the project's `.state/`.
pub const REGISTRY_FILE: &str = "lanes.json";
/// `coordinator.json`, still the store of the shared session and folder
/// fields, and still the only record a pre-lane project has.
pub const COORDINATOR_FILE: &str = "coordinator.json";
/// The handle the synthesized first lane carries.
pub const DEFAULT_NAME: &str = "default";
/// The pane token a coordinator claims its lane with (§5.1). Its value is a
/// lane **id**, so a rename can never make a token lie.
pub const TOKEN: &str = "hp_lane";
/// The lane id a `lane` field means when it is empty (§10): every
/// `Thread.lane` and `inbox::Item.lane` written before lanes existed.
pub const FALLBACK_ID: &str = "c-1";

pub fn registry_path(project: &Project) -> PathBuf {
    project.state_dir().join(REGISTRY_FILE)
}

/// True when the project has a lane registry on disk. Callers ask this before
/// they let lanes change anything, so a project that never used one behaves
/// exactly as it did before lanes existed.
pub fn exists(project: &Project) -> bool {
    registry_path(project).is_file()
}

/// Whether lane-aware code may split this project into lanes. This is the
/// single switch behind the compatibility promise (§10): a project with no
/// `lanes.json` keeps today's single-file, single-value paths byte for byte,
/// and only a project that really has a registry is sharded. Callers must use
/// this instead of repeating `exists()` at every decision point.
pub fn split_in_use(project: &Project) -> bool {
    exists(project)
}

/// The id an empty `lane` field means: the project's default lane (§10).
/// Without a registry this is the synthesized `c-1`, which is what every
/// pre-lane record already means.
pub fn default_lane_id(project: &Project) -> String {
    Registry::for_project(project)
        .default_lane()
        .map(|lane| lane.id.clone())
        .unwrap_or_else(|| FALLBACK_ID.to_string())
}

/// The project home as an identity question sees it: the folder
/// `coordinator.json` records (the canonical path `open` wrote), else the
/// canonical folder. The same string the old `works_in` test compared against.
pub fn project_home(project: &Project) -> String {
    read_coordinator(project)
        .map(|record| record.cwd)
        .filter(|cwd| !cwd.is_empty())
        .unwrap_or_else(|| project.canonical_dir().to_string_lossy().into_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Open,
    /// Retired by `hp lane close`: the record, its id and its history stay,
    /// but the lane takes no `context` section, no sidebar row and no nudge.
    /// Closing is not deleting, and a closed lane whose pane still runs stays
    /// on the `Coordinators:` line, marked `closed`.
    Closed,
}

impl Status {
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Open => "open",
            Status::Closed => "closed",
        }
    }
}

impl Serialize for Status {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Status {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Status, D::Error> {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "closed" => Status::Closed,
            // Anything else — including a value a future version invents —
            // reads as open, the safe direction: an open lane is still nudged.
            _ => Status::Open,
        })
    }
}

/// One lane: the identity of a line of work, and the last place its
/// coordinator was seen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Lane {
    /// `c-<n>`, allocated from [`Registry::next`]. The identity: never reused,
    /// never renumbered, never derived from a name — so an id quoted in an old
    /// report can never point at some other lane.
    pub id: String,
    /// The CLI handle (`--lane fix`). Unique per project among open lanes.
    pub name: String,
    /// The display name: sidebar, tab, Space label.
    pub label: String,
    pub goal: String,
    /// The lane's default coordinator profile. Empty falls back to the
    /// project's own `coordinator_profile`.
    pub profile: String,
    pub status: Status,
    /// The last known coordinator instance. A *hint* for rule 2, refreshed by
    /// the ticker — never the identity itself.
    pub pane_id: String,
    pub agent_session: String,
    pub created: String,
    /// When `hp lane close` ran.
    pub closed: String,
    /// When the lane last did anything: the default-lane election of §6.4
    /// keys on it.
    pub last_active: String,
}

impl Lane {
    /// The token value claiming this lane. An empty id reports no token at
    /// all, so a half-written record can never claim lane `c-`.
    pub fn token_value(&self) -> Option<&str> {
        (!self.id.is_empty()).then_some(self.id.as_str())
    }

    pub fn is_open(&self) -> bool {
        self.status == Status::Open
    }
}

/// The registry: every lane of one project, which one is the default, and the
/// counter ids are allocated from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Registry {
    pub lanes: Vec<Lane>,
    /// The id of the default lane.
    pub default: String,
    /// The `c-<n>` counter. It only moves forward: an id is never reused, not
    /// after a close.
    pub next: u32,
}

impl Registry {
    /// Reads `.state/lanes.json`, or `None` when there is no registry — the
    /// project is a pre-lane one — or when the file does not parse. A broken
    /// registry must not make every command fail; it is reported by `doctor`
    /// and reads as "no lanes", which keeps the old identity rules working.
    pub fn load(project: &Project) -> Option<Registry> {
        project::read_json(&registry_path(project))
    }

    /// Writes the registry. The caller holds `project.lock()`: this is the
    /// only writer and the file is written nowhere else.
    #[cfg_attr(not(test), allow(dead_code))] // the writer lands with `lane adopt` (commit 4)
    pub fn save(&self, project: &Project) -> Result<()> {
        project::write_json(&registry_path(project), self)
    }

    /// The registry as lanes see it: the file when it exists, else one
    /// synthesized **in memory** from `coordinator.json` (§10). The read path
    /// writes nothing — a ticker, a `context` or a `lane list` must not create
    /// state nobody asked for. [`Registry::ensure_written`] is the write path,
    /// and no command calls it until `open` learns about lanes.
    pub fn for_project(project: &Project) -> Registry {
        Registry::load(project).unwrap_or_else(|| Registry::synthesized(project))
    }

    /// The one-lane registry a pre-lane project is treated as having.
    pub fn synthesized(project: &Project) -> Registry {
        Registry::from_coordinator(project, read_coordinator(project).as_ref())
    }

    /// `c-1`, named `default`, labelled with the project's display name,
    /// holding the recorded coordinator's instance hints.
    pub fn from_coordinator(project: &Project, record: Option<&Coordinator>) -> Registry {
        let record = record.cloned().unwrap_or_default();
        Registry {
            lanes: vec![Lane {
                id: FALLBACK_ID.to_string(),
                name: DEFAULT_NAME.to_string(),
                label: project_label(project),
                goal: String::new(),
                profile: record.profile.clone(),
                status: Status::Open,
                pane_id: record.pane_id.clone(),
                agent_session: record.agent_session.clone(),
                created: record.updated.clone(),
                closed: String::new(),
                last_active: record.updated.clone(),
            }],
            default: FALLBACK_ID.to_string(),
            next: 2,
        }
    }

    /// Writes the synthesized registry once, under the project lock. Returns
    /// whether it wrote. Nothing calls this yet: creating the file belongs to
    /// `open` (commit 4), and a read must never be the thing that creates it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn ensure_written(project: &Project) -> Result<bool> {
        if exists(project) {
            return Ok(false);
        }
        let registry = Registry::synthesized(project);
        let _lock = project.lock()?;
        // Re-read under the lock: another command may have written meanwhile.
        if exists(project) {
            return Ok(false);
        }
        registry.save(project)?;
        Ok(true)
    }

    pub fn get(&self, id: &str) -> Option<&Lane> {
        self.lanes.iter().find(|l| l.id == id)
    }

    pub fn open_lanes(&self) -> Vec<&Lane> {
        self.lanes.iter().filter(|l| l.is_open()).collect()
    }

    /// The lane a `lane` field means: the id it names when it names one, else
    /// the default lane. An empty field is the default lane, so records
    /// written before lanes existed need no migration (§10).
    pub fn lane_of(&self, field: &str) -> Option<&Lane> {
        if field.is_empty() {
            self.default_lane()
        } else {
            self.get(field)
        }
    }

    /// The default lane: the id the registry names, while it is still an open
    /// lane. A closed or unknown default falls to the open lane with the
    /// newest `last_active`, ties by lowest id; when no lane is open, `c-1` is
    /// the default again — a project always has one (§6.4).
    pub fn default_lane(&self) -> Option<&Lane> {
        if let Some(lane) = self.get(&self.default).filter(|l| l.is_open()) {
            return Some(lane);
        }
        self.open_lanes()
            .into_iter()
            .max_by(|a, b| a.last_active.cmp(&b.last_active).then_with(|| b.id.cmp(&a.id)))
            .or_else(|| self.get(FALLBACK_ID))
    }

    /// The lane `--lane <handle>` names: by `id` first (an id is a handle that
    /// never changes), then by `name`, then by `label`.
    #[cfg_attr(not(test), allow(dead_code))] // `open --lane` and `lane adopt` (commit 4)
    pub fn find(&self, handle: &str) -> Option<&Lane> {
        self.get(handle).or_else(|| {
            self.lanes
                .iter()
                .find(|l| l.name == handle)
                .or_else(|| self.lanes.iter().find(|l| l.label == handle))
        })
    }

    pub fn is_default(&self, id: &str) -> bool {
        self.default_lane().is_some_and(|lane| lane.id == id)
    }

    /// Allocate `c-<n>` and append the lane; the caller fills in what a
    /// registry cannot know and holds `project.lock()`.
    #[cfg_attr(not(test), allow(dead_code))] // `open --lane` (commit 4)
    pub fn create(&mut self, name: &str, label: &str) -> &mut Lane {
        let id = self.alloc_id();
        self.lanes.push(Lane {
            id,
            name: name.to_string(),
            label: label.to_string(),
            created: project::now(),
            ..Lane::default()
        });
        self.lanes.last_mut().expect("just pushed")
    }

    /// The next id. The counter moves past it even if the lane is then thrown
    /// away: an allocated id never comes back.
    #[cfg_attr(not(test), allow(dead_code))] // reached through `create` (commit 4)
    pub fn alloc_id(&mut self) -> String {
        let n = self.next.max(1);
        self.next = n.saturating_add(1);
        format!("c-{n}")
    }

    /// The `Coordinator` a lane's pane is recorded as: the instance fields
    /// (pane, session, profile) from the lane, the shared ones (socket,
    /// session name, Space, tab, agent kind, folder, agent name) from
    /// `coordinator.json`, which still describes the project's session. This
    /// is the compatibility view of §10: it keeps the sixty-odd call sites of
    /// `Project::coordinator()` working while lanes exist.
    pub fn coordinator_for(&self, project: &Project, lane: &Lane) -> Coordinator {
        let shared = read_coordinator(project).unwrap_or_default();
        Coordinator {
            socket: shared.socket,
            session: shared.session,
            workspace_id: shared.workspace_id,
            tab_id: shared.tab_id,
            pane_id: lane.pane_id.clone(),
            agent_name: shared.agent_name,
            cwd: shared.cwd,
            agent: shared.agent,
            profile: lane.profile.clone(),
            agent_session: lane.agent_session.clone(),
            updated: lane.last_active.clone(),
        }
    }

    /// The default lane as a `Coordinator`. `None` only when the project has
    /// no lane at all to read as one.
    pub fn default_coordinator(&self, project: &Project) -> Option<Coordinator> {
        self.default_lane().map(|lane| self.coordinator_for(project, lane))
    }

    /// Every pane of this project that looks like a coordinator, with the rule
    /// it claimed a lane by. Read-only: `context` and `lane list` print the
    /// same facts from this (§5.4, §6.3). `agents` is a whole session's list,
    /// so panes of other projects are in it and are filtered out here.
    ///
    /// A pane **working in the project folder** is always kept, even when no
    /// lane claims it: the line's job is to show every pane that looks like a
    /// coordinator, and §5.4 gives that case its own rule word.
    pub fn sightings(&self, agents: &[Agent], project_home: &str, slug: &str) -> Vec<Sighting> {
        agents
            .iter()
            .filter(|agent| ours(agent, slug))
            .filter_map(|agent| {
                let resolution = resolve(self, agent, project_home, slug);
                let in_folder = !project_home.is_empty() && crate::herdr::own_dir(&agent.cwd, &agent.foreground_cwd) == project_home;
                (!resolution.is_none() || in_folder).then(|| Sighting {
                    pane_id: agent.pane_id.clone(),
                    agent: agent.agent.clone(),
                    agent_status: agent.agent_status.clone(),
                    workspace_id: agent.workspace_id.clone(),
                    resolution,
                })
            })
            .collect()
    }
}

/// Reads `coordinator.json` directly, bypassing the delegation in
/// `Project::coordinator()`, which reads the lanes and would recurse.
pub(crate) fn read_coordinator(project: &Project) -> Option<Coordinator> {
    project::read_json(&project.state_dir().join(COORDINATOR_FILE))
}

/// The label the synthesized lane shows: the project's display name, the same
/// one `sidebar::coordinator_display` and the home Space label use.
fn project_label(project: &Project) -> String {
    project
        .read_project_md()
        .map(|(settings, _)| project::display_name(&settings.name, &project.slug))
        .unwrap_or_else(|_| project.slug.clone())
}

/// The rule [`resolve`] answered with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// The pane's own `hp_lane` token (§5.1): the only signal that means what
    /// it says.
    Token,
    /// The registry's recorded `pane_id` *and* native session id — survives a
    /// restart and a tab move, while a recycled pane id without it fails.
    Record,
    /// The pane works in the project home and the project has exactly one
    /// open lane: the old behaviour, kept for projects that have one lane.
    Dir,
}

impl Rule {
    pub fn as_str(&self) -> &'static str {
        match self {
            Rule::Token => "token",
            Rule::Record => "record",
            Rule::Dir => "dir",
        }
    }
}

/// The three-valued answer to "whose coordinator is this pane?" (§5.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneResolution {
    Known { lane: String, rule: Rule },
    /// Several lanes could claim the pane and no positive signal says which.
    /// Never treated as the default lane, never nudged, never prompted, never
    /// assigned — but it *is* a live coordinator, so the project is never
    /// declared coordinator-less.
    Ambiguous { reason: String },
    /// Not a coordinator of this project at all.
    None,
}

impl LaneResolution {
    pub fn is_none(&self) -> bool {
        self == &LaneResolution::None
    }

    #[cfg_attr(not(test), allow(dead_code))] // kept for tests of the ticker's held reason
    pub fn is_ambiguous(&self) -> bool {
        matches!(self, LaneResolution::Ambiguous { .. })
    }

    pub fn rule(&self) -> Option<Rule> {
        match self {
            LaneResolution::Known { rule, .. } => Some(*rule),
            _ => None,
        }
    }

    /// The lane id, when the answer names one. An ambiguous answer names
    /// nothing: that is the point of rule 4.
    pub fn lane_id(&self) -> Option<&str> {
        match self {
            LaneResolution::Known { lane, .. } => Some(lane),
            _ => None,
        }
    }

    /// Liveness on its own (`docs/lanes.md` §5.2): an ambiguous pane **is** a
    /// coordinator, so a project is never declared coordinator-less and its
    /// routines and nudges never stall in silence. It is never *given*
    /// anything: routing asks for [`LaneResolution::lane_id`] and refuses on
    /// the ambiguous answer. This is what makes the refusal safe.
    pub fn counts_as_coordinator(&self) -> bool {
        !self.is_none()
    }
}

fn token_of(agent: &Agent) -> Option<&str> {
    agent
        .tokens
        .get(TOKEN)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
}

/// True when the pane's own report names no project, or names `slug`: a pane
/// another project owns is never this project's coordinator, whatever its
/// directory says. Lane ids are unique only inside one project — `law` and
/// `rexa` both have a `c-1` — so this is what stops one lane's claim from
/// answering to another project's lane table.
pub(crate) fn ours(agent: &Agent, slug: &str) -> bool {
    agent
        .tokens
        .get(crate::sidebar::PROJECT_TOKEN)
        .is_none_or(|owner| owner == slug)
}

/// §5.2, all five rules, in order. `project_home` is the project's canonical
/// folder — the same string `Coordinator::cwd` stores — and `slug` its slug.
///
/// A pane whose own report names a **different** project is never ours: a lane
/// id is unique inside one project, not across a session (`law` and `rexa` both
/// have a `c-1`), and every pane of a project carries `hp_project`, so that is
/// the token that says whose claim to read. Without this guard one lane's
/// coordinator would answer to another project's default lane and events would
/// be misfiled again — the very thing lanes exist to stop.
pub fn resolve(registry: &Registry, agent: &Agent, project_home: &str, slug: &str) -> LaneResolution {
    if !ours(agent, slug) {
        return LaneResolution::None;
    }
    // 1. The pane's own token: written by the process that knows.
    if let Some(id) = token_of(agent) {
        return match registry.get(id) {
            Some(lane) => LaneResolution::Known {
                lane: lane.id.clone(),
                rule: Rule::Token,
            },
            // A token naming a lane the registry does not have is a stale or
            // hand-written claim: it says "a coordinator" but not "which".
            // Inventing a lane from it would be a guess, so it refuses.
            None => LaneResolution::Ambiguous {
                reason: format!("token {TOKEN}={id} names no lane — run `hp lane adopt <lane>` in it or fix {}", REGISTRY_FILE),
            },
        };
    }
    // 2. The recorded instance: pane id *and* native session id. The session
    // is what stops a recycled pane id from counting.
    if !agent.pane_id.is_empty() {
        let session = agent.session_id();
        if !session.is_empty()
            && let Some(lane) = registry
                .lanes
                .iter()
                .find(|l| !l.pane_id.is_empty() && l.pane_id == agent.pane_id && l.agent_session == session)
        {
            return LaneResolution::Known {
                lane: lane.id.clone(),
                rule: Rule::Record,
            };
        }
    }
    // 3 and 4. The directory. It carries information only when one open lane
    // exists: every lane's coordinator works in the same home (decision 1),
    // so with two the folder says nothing and the answer is a refusal that
    // names the command which fixes it — not `max_by_key` picking one.
    if !project_home.is_empty() && crate::herdr::own_dir(&agent.cwd, &agent.foreground_cwd) == project_home {
        let open = registry.open_lanes();
        return match open.len() {
            1 => LaneResolution::Known {
                lane: open[0].id.clone(),
                rule: Rule::Dir,
            },
            // Not exactly one open lane to claim it, and nothing else does:
            // the folder says "a coordinator" and no lane owns the claim. It
            // stays unassigned — printed by [`Registry::sightings`] and by the
            // identity line, never folded into the default lane.
            0 => LaneResolution::None,
            n => LaneResolution::Ambiguous {
                reason: format!("no token, no record match, {n} open lanes share this folder — run `hp lane adopt <lane>` in it"),
            },
        };
    }
    // 5. An agent in a worktree or a thread folder is a thread; an agent
    // nowhere near the project and claimed by no lane is nobody's.
    LaneResolution::None
}

/// Resolve a pane against its project: the registry on disk when there is one,
/// else the synthesized single-lane view (§10). Read-only.
pub fn resolve_in_project(project: &Project, agent: &Agent) -> LaneResolution {
    let registry = Registry::for_project(project);
    resolve(&registry, agent, &project_home(project), &project.slug)
}

/// The three-valued identity answer the ticker's hot path uses (`docs/lanes.md`
/// §5.2 rule 4, §12 commit 3).
///
/// A project **without** `lanes.json` is classified by the old single record
/// ([`crate::coordinator::is_coordinator`]) and nothing else: that is the
/// zero-change promise of §10, and it is why the gate is [`split_in_use`] and
/// not a second opinion about lanes. Its one lane is the synthesized default
/// lane, whose id answers for every pane the old record matched.
///
/// A project **with** a registry goes through [`resolve`], so a second lane's
/// pane is discovered (and its `hp_lane` token refreshed) even though the
/// shared `coordinator.json` never names it. The `rule` word of a `Known`
/// answer is only meaningful for the registry path: nothing on the old path
/// displays it.
///
/// `Ambiguous` counts as a coordinator for liveness and is never a
/// destination: `nudge` and `prompt` ask for a lane id, which it does not
/// have.
pub fn classify(project: &Project, record: &Coordinator, agent: &Agent) -> LaneResolution {
    classify_all(project, record, std::slice::from_ref(agent)).pop().unwrap_or(LaneResolution::None)
}

/// [`classify`] for a whole agent list, with the registry read **once** — the
/// ticker's `discover` asks about every agent in the session every tick, and a
/// per-pane file read on that path is exactly the kind of cost this commit must
/// not add. The answers keep the agents' order.
pub fn classify_all(project: &Project, record: &Coordinator, agents: &[Agent]) -> Vec<LaneResolution> {
    if !split_in_use(project) {
        let lane = default_lane_id(project);
        return agents
            .iter()
            .map(|agent| {
                if crate::coordinator::is_coordinator(record, agent) {
                    LaneResolution::Known { lane: lane.clone(), rule: Rule::Dir }
                } else {
                    LaneResolution::None
                }
            })
            .collect();
    }
    let registry = Registry::for_project(project);
    let home = project_home(project);
    agents
        .iter()
        .map(|agent| resolve(&registry, agent, &home, &project.slug))
        .collect()
}

/// The lanes one tick routes to (`docs/lanes.md` §4.4, §12 commit 3): every
/// **open** lane of the registry, so a closed lane takes no nudge (§6.4), and
/// without a registry the single synthesized default lane — the one nudge run
/// today's ticker makes.
pub fn routes(project: &Project) -> Vec<String> {
    if !split_in_use(project) {
        return vec![default_lane_id(project)];
    }
    Registry::for_project(project).open_lanes().iter().map(|lane| lane.id.clone()).collect()
}

/// §7's orphans: the open lanes that still have live threads and whose live
/// panes no rule resolves to them at all, with the number of `Open`/`Starting`
/// threads each holds. Empty without a registry: a project that never used
/// lanes has no lane to be orphaned.
///
/// A lane whose recorded pane is **alive but unrecognized** is deliberately not
/// included — that is §5.4's `degraded` case, which the `Coordinators:` line
/// reports every turn; its fix is `hp lane adopt`, never `hp open`, because
/// `open` would start a second coordinator beside the running one (§6.4).
pub fn orphan_lanes(project: &Project, agents: &[Agent]) -> Vec<(Lane, usize)> {
    if !split_in_use(project) {
        return Vec::new();
    }
    let registry = Registry::for_project(project);
    let view = View {
        registry: registry.clone(),
        agents: agents.to_vec(),
        home: project_home(project),
        slug: project.slug.clone(),
        this_pane: String::new(),
        reachable: true,
        counts: BTreeMap::new(),
    };
    let threads = crate::thread::list(project);
    view.orphan_ids()
        .into_iter()
        .filter_map(|id| {
            let lane = registry.get(&id)?.clone();
            let count = threads
                .iter()
                .filter(|t| matches!(t.status, crate::thread::Status::Open | crate::thread::Status::Starting))
                .filter(|t| registry.lane_of(&t.lane).map(|l| l.id.as_str()) == Some(lane.id.as_str()))
                .count();
            (count > 0).then_some((lane, count))
        })
        .collect()
}

/// The lane a **new** thread record inherits: the lane the pane that ran
/// `thread start` / `thread adopt` belongs to (`docs/lanes.md` §4.2). Only a
/// `Known` answer writes a lane id; an ambiguous pane, an unknown pane and no
/// pane at all all write the empty string, which reads as the default lane.
/// `agents` is the whole agent list of the session `HERDR_PANE_ID` lives in —
/// the caller already has it from `threads::session_view` / `adopt`.
pub fn lane_of_initiator(ctx: &Ctx, project: &Project, agents: &[Agent]) -> String {
    // A project that never used lanes keeps writing no lane at all: its new
    // records read as the default lane by being empty (§10), and nothing about
    // its files changes.
    if !split_in_use(project) {
        return String::new();
    }
    let Some(pane) = ctx.env.var("HERDR_PANE_ID").filter(|pane| !pane.is_empty()) else {
        return String::new();
    };
    // `HERDR_SOCKET_PATH` is the session the pane belongs to; read the agent
    // list there rather than trusting a view from another session. A command
    // run outside Herdr has no socket and keeps the caller's `agents`.
    let listed: Vec<Agent>;
    let agents: &[Agent] = match ctx.env.var("HERDR_SOCKET_PATH") {
        Some(socket) => {
            listed = crate::herdr::Herdr::new(ctx.env.herdr_bin(), socket, ctx.runner).agent_list().unwrap_or_default();
            &listed
        }
        None => agents,
    };
    let Some(agent) = agents.iter().find(|agent| agent.pane_id == pane && ours(agent, &project.slug)) else {
        return String::new();
    };
    match resolve_in_project(project, agent) {
        LaneResolution::Known { lane, .. } => lane,
        LaneResolution::Ambiguous { .. } | LaneResolution::None => String::new(),
    }
}

/// The lane a `context` is being read from — `HERDR_PANE_ID` resolved through
/// §5.2, else the default lane — plus the lane an empty `lane` field means.
/// `None` only when the project has no `lanes.json`: then nothing may be split
/// (the zero-change promise for pre-lane projects).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub lane: String,
    pub default_lane: String,
}

impl Scope {
    /// Whether a record's `lane` field belongs to this reader's lane. An
    /// empty field is the default lane (§10), never a third lane.
    pub fn admits(&self, field: &str) -> bool {
        if field.is_empty() { self.lane == self.default_lane } else { field == self.lane }
    }
}

/// The scope of one `context` turn, from the session view the digest already
/// read. A pane that resolves to no lane (ambiguous, unknown, outside Herdr)
/// reads the default lane, as §8 specifies.
pub fn reader_scope(project: &Project, view: &View) -> Option<Scope> {
    if !split_in_use(project) {
        return None;
    }
    let default_lane = view
        .registry
        .default_lane()
        .map(|lane| lane.id.clone())
        .unwrap_or_else(|| FALLBACK_ID.to_string());
    let lane = view
        .agents
        .iter()
        .find(|agent| agent.pane_id == view.this_pane && ours(agent, &view.slug))
        .map(|agent| resolve(&view.registry, agent, &view.home, &view.slug))
        .and_then(|resolution| resolution.lane_id().map(str::to_string))
        .unwrap_or_else(|| default_lane.clone());
    Some(Scope { lane, default_lane })
}

/// The lane id to write as a pane's `hp_lane` token, from the registry on disk
/// only: a project that has never used lanes reports no token at all, which is
/// what keeps commit 1 free of behaviour change. `None` when the pane cannot be
/// tied to exactly one lane — a token must never lie (§5.1).
///
/// `pane_id` is a pane already known to be a coordinator: the caller takes it
/// from `open`'s record (the ticker has the lane its resolver found, in
/// `LivePane.lane`, and writes that instead). With one open lane any
/// coordinator pane claims that lane (the folder rule, and the common case);
/// with several, only the lane whose recorded pane it is claims it.
pub fn token_for_pane(project: &Project, pane_id: &str) -> Option<String> {
    if pane_id.is_empty() {
        return None;
    }
    let registry = Registry::load(project)?;
    if let Some(lane) = registry.lanes.iter().find(|l| l.pane_id == pane_id) {
        return lane.token_value().map(str::to_string);
    }
    let open = registry.open_lanes();
    (open.len() == 1).then(|| open[0].id.clone())
}

/// One live pane as the resolver saw it.
#[derive(Debug, Clone, PartialEq)]
pub struct Sighting {
    pub pane_id: String,
    pub workspace_id: String,
    pub agent: String,
    pub agent_status: String,
    pub resolution: LaneResolution,
}

/// The `Coordinators:` line (§5.4): one entry per pane that claims a lane or
/// sits in the project folder, then any lane whose recorded pane is alive but
/// recognizable by no rule, then the default lane. `this_pane` is
/// `HERDR_PANE_ID`, so a coordinator reading its own digest sees which entry
/// is itself. Every branch prints: an ambiguous entry carries the reason and
/// the command that fixes it, and an unreachable session says so instead of
/// printing nothing.
pub fn identity_line(view: &View) -> String {
    let mut parts: Vec<String> = view.entries();
    if !view.reachable {
        parts.push("session unreachable (no pane checked)".to_string());
    }
    // Only the lanes whose pane is not already on the line: a pane printed as
    // `ambiguous` needs no second entry saying the same about its lane, and a
    // pane that is simply gone is `lane list`'s orphan note, not a degraded one.
    let statuses = view.lane_statuses();
    for status in statuses.iter().filter(|s| !s.orphan && !s.shown) {
        parts.push(format!(
            "{} (degraded, lane {}{}: {}) — run `hp lane adopt {}` in it",
            status.pane_id.clone().unwrap_or_default(),
            status.lane_id,
            if status.this_pane { ", this pane" } else { "" },
            status.why,
            status.lane_id,
        ));
    }
    // And the reader itself, when it is a live agent Herdr lists and nothing
    // above names it: no token, no record, not in the folder. `hp lane adopt`
    // exists for the tab the user opened by hand (§6.2), so the line says the
    // pane they are sitting in is not this project's coordinator — the one
    // verdict a digest could never deliver before, because an unrecognized
    // pane printed no entry at all. A pane outside Herdr, or one that is only a
    // shell, is said nothing about.
    let listed = view.agents.iter().any(|a| a.pane_id == view.this_pane);
    // Only a status this line itself printed counts as naming the pane: an
    // orphan lane leaves its pane unexplained, and that pane is the reader's.
    let named = statuses.iter().any(|s| !s.orphan && s.pane_id.as_deref() == Some(view.this_pane.as_str()));
    let shown = view.entries().iter().any(|e| e.starts_with(&format!("{} ", view.this_pane)));
    if view.reachable && listed && !named && !shown {
        parts.push(format!("this pane {} is no lane's coordinator (run `hp lane adopt <lane>` in it)", view.this_pane));
    }
    parts.push(default_tail(&view.registry));
    format!("Coordinators: {}\n", parts.join(" · "))
}

/// The per-lane counts `hp lane list` prints (§6.3). A thread with an empty
/// `lane` counts for the default lane; an item with an empty lane likewise
/// (§10). Without a registry the synthesized single lane counts everything,
/// which is the same fact the old single-file view had.
fn lane_counts(project: &Project) -> BTreeMap<String, LaneCounts> {
    let registry = Registry::for_project(project);
    let default_lane = registry
        .default_lane()
        .map(|lane| lane.id.clone())
        .unwrap_or_else(|| FALLBACK_ID.to_string());
    let mut counts: BTreeMap<String, LaneCounts> =
        registry.lanes.iter().map(|lane| (lane.id.clone(), LaneCounts::default())).collect();
    let effective = |field: &str| -> String {
        if field.is_empty() { default_lane.clone() } else { field.to_string() }
    };
    for thread in crate::thread::list(project) {
        if let Some(count) = counts.get_mut(&effective(&thread.lane)) {
            count.threads += 1;
        }
    }
    for item in crate::inbox::unhandled(project) {
        if let Some(count) = counts.get_mut(&effective(&item.lane)) {
            count.inbox += 1;
        }
    }
    counts
}

/// One look at a project's live session. The two places §5.4 requires to say
/// the same thing — `hp context`'s identity line and `hp lane list` — render
/// from this one value, so they cannot drift apart. Reading it writes nothing:
/// no registry, no record, no token.
#[derive(Debug, Clone)]
pub struct View {
    pub registry: Registry,
    pub agents: Vec<Agent>,
    pub home: String,
    pub slug: String,
    pub this_pane: String,
    /// False when Herdr could not be asked at all. A line is printed either
    /// way: "nothing printed" is how the week-long stall hid.
    pub reachable: bool,
    /// Per-lane `(threads, unhandled inbox items)`, filled only by
    /// [`View::with_counts`] so the identity line pays no file reads. An empty
    /// map keeps the old row shape (the literal-only tests rely on that).
    pub counts: BTreeMap<String, LaneCounts>,
}

/// The two counts §6.3 promises per lane: every thread record that names the
/// lane (resolved ones included — a closed lane keeps its history), and the
/// unhandled inbox items addressed to it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LaneCounts {
    pub threads: usize,
    pub inbox: usize,
}

impl View {
    /// Everything lanes need from one session, read once: the registry (never
    /// created here), the agents Herdr lists, and whether Herdr answered at
    /// all. Both renderers take this, so they cannot drift apart.
    pub fn load(ctx: &Ctx, project: &Project) -> View {
        View::with_session(ctx, project, crate::threads::session_view(ctx, project).as_ref())
    }

    /// The same, from a session view the caller already read, so one turn
    /// costs one pair of Herdr lists.
    pub fn with_session(ctx: &Ctx, project: &Project, session: Option<&crate::threads::SessionView>) -> View {
        View {
            registry: Registry::for_project(project),
            agents: session.map(|view| view.agents.clone()).unwrap_or_default(),
            home: project_home(project),
            slug: project.slug.clone(),
            this_pane: ctx.env.var("HERDR_PANE_ID").unwrap_or("").to_string(),
            reachable: session.is_some(),
            counts: BTreeMap::new(),
        }
    }

    /// Fills the per-lane counts for `hp lane list` (§6.3). One registry read
    /// and two directory walks; `context` never calls this.
    pub fn with_counts(mut self, project: &Project) -> View {
        self.counts = lane_counts(project);
        self
    }

    pub fn sightings(&self) -> Vec<Sighting> {
        self.registry.sightings(&self.agents, &self.home, &self.slug)
    }

    fn entries(&self) -> Vec<String> {
        self.sightings()
            .iter()
            .map(|s| {
                let who = if s.pane_id == self.this_pane { "this pane" } else { "other pane" };
                match &s.resolution {
                    LaneResolution::Known { lane, rule } => {
                        let closed = self.registry.get(lane).is_some_and(|l| !l.is_open());
                        format!(
                            "{} ({}, lane {lane}{}, {who})",
                            s.pane_id,
                            rule.as_str(),
                            if closed { ", closed" } else { "" }
                        )
                    }
                    LaneResolution::Ambiguous { reason } => format!("{} (ambiguous: {reason})", s.pane_id),
                    // A pane in the project folder no lane can claim: §5.4's
                    // fourth rule word, printed rather than hidden.
                    LaneResolution::None => format!("{} (none, no lane, {who})", s.pane_id),
                }
            })
            .collect()
    }

    /// The open lanes whose coordinator is **alive but not recognizable**,
    /// named while the state stands. This is the incident this module exists
    /// for: the lane records a pane, the pane is still listed, but its native
    /// session no longer matches and its directory is outside the project home,
    /// so rules 1, 2 and 3 all miss it. Under a single record the project is
    /// then judged coordinator-less, nudges stop, and that branch logs nothing
    /// at all — `herdr-projects` lost a week to exactly this, and a once-only
    /// item could not have reported it because there was no transition.
    ///
    /// A lane whose recorded pane is *not* listed is not degraded: that is an
    /// orphan, and [`LaneStatus::orphan`] says which is which so `lane list`
    /// never tells a live pane it is gone.
    fn lane_statuses(&self) -> Vec<LaneStatus> {
        if !self.reachable {
            return Vec::new();
        }
        let sightings = self.sightings();
        self.registry
            .lanes
            .iter()
            .filter(|lane| lane.is_open())
            .filter(|lane| !sightings.iter().any(|s| s.resolution.lane_id() == Some(lane.id.as_str())))
            .map(|lane| {
                // Only a pane of *this* project: a neighbour's pane that reuses
                // the id is not our coordinator.
                let agent = self
                    .agents
                    .iter()
                    .find(|a| !lane.pane_id.is_empty() && a.pane_id == lane.pane_id && ours(a, &self.slug));
                let Some(agent) = agent else {
                    // No live pane at all for the lane: the orphan case.
                    return LaneStatus {
                        lane_id: lane.id.clone(),
                        pane_id: (!lane.pane_id.is_empty()).then(|| lane.pane_id.clone()),
                        why: if lane.pane_id.is_empty() {
                            "no pane was ever recorded for it".to_string()
                        } else {
                            format!("its recorded pane {} is not listed", lane.pane_id)
                        },
                        this_pane: false,
                        shown: false,
                        orphan: true,
                    };
                };
                // Why the rules missed it, in the order they missed it.
                let mut why: Vec<String> = Vec::new();
                if !lane.agent_session.is_empty() && agent.session_id() != lane.agent_session {
                    why.push("its recorded session no longer matches".to_string());
                }
                let folder = crate::herdr::own_dir(&agent.cwd, &agent.foreground_cwd);
                if folder != self.home {
                    why.push(format!("it works in {folder}, outside {}", self.home));
                }
                if why.is_empty() {
                    why.push("it is listed and claims no lane".to_string());
                }
                // A pane the line already prints (ambiguous, or in the folder
                // and unclaimed) needs no second entry about the same pane; it
                // is not an orphan either — the process is running.
                let shown = sightings.iter().any(|s| s.pane_id == agent.pane_id);
                LaneStatus {
                    lane_id: lane.id.clone(),
                    pane_id: Some(lane.pane_id.clone()),
                    why: why.join(" and "),
                    this_pane: lane.pane_id == self.this_pane,
                    shown,
                    orphan: false,
                }
            })
            .collect()
    }

    /// `hp lane list`'s body: the identity line, one row per lane, and a note
    /// for every open lane that no live pane claims (§5.4: "open lanes whose
    /// coordinator is missing entirely").
    pub fn list_text(&self) -> String {
        let mut out = identity_line(self);
        let sightings = self.sightings();
        for row in views(&self.registry, &sightings) {
            let panes: Vec<String> = row
                .panes
                .iter()
                .map(|(pane, rule)| format!("{pane}({rule})"))
                .collect();
            let counts = match self.counts.get(&row.lane.id) {
                Some(counts) => format!("\tthreads={}\tinbox={}", counts.threads, counts.inbox),
                None => String::new(),
            };
            let _ = writeln!(
                out,
                "{}\t{}\t{}\t{}\t{}\tlast_active={}\tpanes={}{}",
                row.lane.id,
                if row.is_default { "default" } else { "" },
                row.lane.status.as_str(),
                row.lane.name,
                row.lane.label,
                if row.lane.last_active.is_empty() { "-" } else { &row.lane.last_active },
                if panes.is_empty() { "-".to_string() } else { panes.join(" ") },
                counts,
            );
        }
        if self.registry.lanes.is_empty() {
            let _ = writeln!(out, "no lanes");
        }
        if self.reachable {
            // Only the lanes whose pane really is gone: a live-but-unrecognized
            // pane is on the identity line already, and telling the user its
            // coordinator "is gone" would be a lie that sends them to `open`,
            // which starts a second coordinator beside the one running.
            for status in self.lane_statuses().into_iter().filter(|s| s.orphan) {
                let _ = write!(out, "lane {} ({}) has no live coordinator", status.lane_id, self.lane_label(&status.lane_id));
                if let Some(pane) = status.pane_id {
                    let _ = write!(out, "; the pane it last ran in, {pane}, is gone");
                }
                let _ = writeln!(out, " — start one with `open {} --lane {}`", self.slug, status.lane_id);
            }
        }
        out
    }

    fn lane_label(&self, id: &str) -> String {
        self.registry.get(id).map(|l| l.label.clone()).unwrap_or_default()
    }

    /// The open lanes whose coordinator is **gone**, as opposed to alive but
    /// unrecognizable: the ids `lane list --json` flags `orphan`.
    pub fn orphan_ids(&self) -> Vec<String> {
        self.lane_statuses().into_iter().filter(|s| s.orphan).map(|s| s.lane_id).collect()
    }
}

/// One open lane that no pane claims, and why. `orphan` is the difference
/// between "its coordinator is gone" and "its coordinator is sitting there
/// unrecognizable": only the first may be told to start a new one.
#[derive(Debug, Clone, PartialEq)]
pub struct LaneStatus {
    pub lane_id: String,
    pub pane_id: Option<String>,
    pub why: String,
    pub this_pane: bool,
    /// The pane is already printed on the line (ambiguous, or in the folder
    /// and claimed by no lane), so no second entry about it is needed.
    pub shown: bool,
    /// No live pane at all for the lane: `lane list` says its coordinator is
    /// gone and offers `open`. A lane whose pane runs unrecognized is not an
    /// orphan, and telling the user to `open` would start a second coordinator
    /// beside the one already running (§6.4).
    pub orphan: bool,
}



fn default_tail(registry: &Registry) -> String {
    let default = registry.default_lane().map(|l| l.id.clone()).unwrap_or_default();
    match registry.lanes.len() {
        0 => "no lane".to_string(),
        1 => format!("default lane {default}"),
        n => {
            let ids: Vec<String> = registry
                .lanes
                .iter()
                .map(|l| if l.is_open() { l.id.clone() } else { format!("{} closed", l.id) })
                .collect();
            format!("lanes {} of {n} (default {default})", ids.join(" "))
        }
    }
}

/// `hp lane list`: one row per lane — record, default, live panes with the
/// rule each matched (§5.4: the same fact `context` prints).
#[derive(Debug, Clone, PartialEq)]
pub struct LaneView {
    pub lane: Lane,
    pub is_default: bool,
    /// `(pane id, rule)`. A pane that resolved to no lane appears in no row:
    /// a pane that *claims* one it cannot be matched to is ambiguous, and the
    /// identity line is where that shows.
    pub panes: Vec<(String, &'static str)>,
}

pub fn views(registry: &Registry, sightings: &[Sighting]) -> Vec<LaneView> {
    let mut claimed: BTreeMap<String, Vec<(String, &'static str)>> = BTreeMap::new();
    for s in sightings {
        if let Some(lane) = s.resolution.lane_id() {
            let rule = s.resolution.rule().map(|r| r.as_str()).unwrap_or("-");
            claimed.entry(lane.to_string()).or_default().push((s.pane_id.clone(), rule));
        }
    }
    let out: Vec<LaneView> = registry
        .lanes
        .iter()
        .map(|lane| LaneView {
            lane: lane.clone(),
            is_default: registry.is_default(&lane.id),
            panes: claimed.remove(lane.id.as_str()).unwrap_or_default(),
        })
        .collect();
    debug_assert!(claimed.is_empty(), "every resolution names a lane the registry has");
    out
}

/// The JSON of `hp lane list --json`: the same lanes, the same panes and the
/// same rules, plus what only a machine reads (goal, profile, recorded pane).
pub fn list_json(view: &View) -> serde_json::Value {
    serde_json::Value::Array(
        views(&view.registry, &view.sightings())
            .iter()
            .map(|row| {
                serde_json::json!({
                    "id": row.lane.id,
                    "name": row.lane.name,
                    "label": row.lane.label,
                    "status": row.lane.status.as_str(),
                    "default": row.is_default,
                    "last_active": row.lane.last_active,
                    "profile": row.lane.profile,
                    "goal": row.lane.goal,
                    "pane_id": row.lane.pane_id,
                    "threads": view.counts.get(&row.lane.id).map(|counts| counts.threads).unwrap_or(0),
                    "inbox": view.counts.get(&row.lane.id).map(|counts| counts.inbox).unwrap_or(0),
                    "orphan": view.orphan_ids().iter().any(|id| id == &row.lane.id),
                    "panes": row
                        .panes
                        .iter()
                        .map(|(pane, rule)| serde_json::json!({ "pane": pane, "rule": rule }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

pub fn lane_list(ctx: &Ctx, slug: &str, json: bool) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    let view = View::load(ctx, &project).with_counts(&project);
    if json {
        println!("{}", serde_json::to_string_pretty(&list_json(&view))?);
        return Ok(());
    }
    print!("{}", view.list_text());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::AgentSession;

    const HOME: &str = "/r/demo";

    fn agent(pane: &str, cwd: &str) -> Agent {
        Agent {
            pane_id: pane.into(),
            tab_id: "t1".into(),
            workspace_id: "w1".into(),
            cwd: cwd.into(),
            agent: "claude".into(),
            agent_status: "idle".into(),
            ..Agent::default()
        }
    }

    fn with_session(pane: &str, cwd: &str, session: &str) -> Agent {
        Agent {
            agent_session: Some(AgentSession { value: session.into() }),
            ..agent(pane, cwd)
        }
    }

    fn claiming(pane: &str, cwd: &str, lane: &str) -> Agent {
        let mut a = agent(pane, cwd);
        a.tokens.insert(TOKEN.into(), lane.into());
        a
    }

    fn lane(id: &str, name: &str) -> Lane {
        Lane {
            id: id.into(),
            name: name.into(),
            label: name.into(),
            ..Lane::default()
        }
    }

    fn lane_at(id: &str, name: &str, pane: &str, session: &str) -> Lane {
        Lane {
            pane_id: pane.into(),
            agent_session: session.into(),
            ..lane(id, name)
        }
    }

    fn closed_lane(id: &str) -> Lane {
        Lane {
            status: Status::Closed,
            ..lane(id, id)
        }
    }

    fn lane_named(id: &str, name: &str, label: &str) -> Lane {
        Lane {
            id: id.into(),
            name: name.into(),
            label: label.into(),
            ..Lane::default()
        }
    }

    fn lane_at_named(id: &str, name: &str, label: &str, pane: &str, session: &str) -> Lane {
        Lane {
            pane_id: pane.into(),
            agent_session: session.into(),
            ..lane_named(id, name, label)
        }
    }

    fn reg(lanes: Vec<Lane>, default: &str, next: u32) -> Registry {
        Registry {
            lanes,
            default: default.into(),
            next,
        }
    }

    /// A rendered view of `agents` as the session's whole list, which is what
    /// both printers take.
    fn view(registry: &Registry, agents: Vec<Agent>, this_pane: &str, reachable: bool) -> View {
        View {
            registry: registry.clone(),
            agents,
            home: HOME.into(),
            slug: "demo".into(),
            this_pane: this_pane.into(),
            reachable,
            counts: BTreeMap::new(),
        }
    }

    fn temp_project(name: &str) -> (tempfile::TempDir, Project) {
        let world = tempfile::tempdir().unwrap();
        let project = project::create(&world.path().join("root"), name, "", vec![]).unwrap();
        (world, project)
    }

    // ---------------------------------------------------------- id rules

    #[test]
    fn rule_1_the_token_beats_the_record_and_the_folder() {
        let registry = reg(
            vec![lane_at("c-1", "default", "w1:p1", "sess-1"), lane_at("c-2", "books", "w1:p1", "sess-1")],
            "c-1",
            3,
        );
        // The token says c-2, although a record matches and although the pane
        // works in the project home.
        assert_eq!(
            resolve(&registry, &claiming("w1:p1", HOME, "c-2"), HOME, "demo"),
            LaneResolution::Known {
                lane: "c-2".into(),
                rule: Rule::Token
            }
        );
    }

    #[test]
    fn rule_1_needs_a_lane_the_registry_has_and_an_empty_token_claims_nothing() {
        let registry = reg(vec![lane("c-1", "default")], "c-1", 2);
        // A token for a lane that is not there is reported, never honoured and
        // never folded into the default lane.
        let answer = resolve(&registry, &claiming("w1:p1", "/elsewhere", "c-9"), HOME, "demo");
        assert!(answer.is_ambiguous(), "{answer:?}");
        assert!(answer.lane_id().is_none());
        // An empty token value is no claim: the folder rule then applies.
        assert_eq!(
            resolve(&registry, &claiming("w1:p1", HOME, ""), HOME, "demo"),
            LaneResolution::Known {
                lane: "c-1".into(),
                rule: Rule::Dir
            }
        );
    }

    #[test]
    fn rule_2_needs_pane_id_and_native_session_together() {
        let registry = reg(vec![lane_at("c-1", "default", "w1:p1", "sess-1")], "c-1", 2);
        // It works in no project folder: the recorded instance still is it.
        assert_eq!(
            resolve(&registry, &with_session("w1:p1", "/elsewhere", "sess-1"), HOME, "demo"),
            LaneResolution::Known {
                lane: "c-1".into(),
                rule: Rule::Record
            }
        );
        // A recycled pane id, a different session: not the lane's coordinator.
        assert_eq!(resolve(&registry, &with_session("w1:p1", "/elsewhere", "sess-2"), HOME, "demo"), LaneResolution::None);
        // The same id with no session at all cannot be vouched for.
        assert_eq!(resolve(&registry, &agent("w1:p1", "/elsewhere"), HOME, "demo"), LaneResolution::None);
        // A lane with no recorded instance matches nothing by rule 2.
        assert_eq!(resolve(&reg(vec![lane("c-2", "x")], "c-2", 3), &with_session("w1:p1", "/elsewhere", "sess-1"), HOME, "demo"), LaneResolution::None);
    }

    #[test]
    fn rule_3_the_folder_when_exactly_one_lane_is_open() {
        let registry = reg(vec![lane("c-1", "default")], "c-1", 2);
        assert_eq!(
            resolve(&registry, &agent("w9:p9", HOME), HOME, "demo"),
            LaneResolution::Known {
                lane: "c-1".into(),
                rule: Rule::Dir
            }
        );
        // `open` runs a coordinator as a child in a shell pane elsewhere: its
        // foreground directory decides.
        let child = Agent {
            foreground_cwd: HOME.into(),
            ..agent("w5:p1", "/tmp")
        };
        assert_eq!(
            resolve(&registry, &child, HOME, "demo"),
            LaneResolution::Known {
                lane: "c-1".into(),
                rule: Rule::Dir
            }
        );
        // One open lane and one closed one: the folder still carries one
        // answer.
        let one_closed = reg(vec![lane("c-1", "default"), closed_lane("c-2")], "c-1", 3);
        assert_eq!(
            resolve(&one_closed, &agent("w1:p1", HOME), HOME, "demo"),
            LaneResolution::Known {
                lane: "c-1".into(),
                rule: Rule::Dir
            }
        );
    }

    #[test]
    fn rule_4_refuses_where_rule_3_would_have_landed() {
        // The same pane, the same folder: the only difference is how many
        // lanes the project has.
        let one = reg(vec![lane("c-1", "default")], "c-1", 2);
        let two = reg(vec![lane("c-1", "default"), lane("c-2", "books")], "c-1", 3);
        assert_eq!(
            resolve(&one, &agent("w1:p1", HOME), HOME, "demo"),
            LaneResolution::Known {
                lane: "c-1".into(),
                rule: Rule::Dir
            }
        );
        let answer = resolve(&two, &agent("w1:p1", HOME), HOME, "demo");
        assert!(answer.is_ambiguous(), "{answer:?}");
        assert_eq!(answer.lane_id(), None, "an ambiguous answer names no lane");
        assert_eq!(answer.rule(), None);
        assert!(answer.counts_as_coordinator(), "{answer:?}");
        assert!(
            format!("{answer:?}").contains("hp lane adopt"),
            "the refusal names the command that fixes it: {answer:?}"
        );
        // Three open lanes refuse the same way.
        let three = reg(vec![lane("c-1", "a"), lane("c-2", "b"), lane("c-3", "c")], "c-1", 4);
        assert!(resolve(&three, &agent("w1:p1", HOME), HOME, "demo").is_ambiguous());
        // Two open plus one closed still refuses: a closed lane answers nothing.
        let two_closed = reg(vec![lane("c-1", "a"), lane("c-2", "b"), closed_lane("c-3")], "c-1", 4);
        assert!(resolve(&two_closed, &agent("w1:p1", HOME), HOME, "demo").is_ambiguous());
        // No open lane at all: nothing can claim it, so it is nobody's lane —
        // and the identity line still prints it, so this is never a silent
        // "no coordinator" (which is the verdict that stalled nudges).
        let none_open = reg(vec![closed_lane("c-1"), closed_lane("c-2")], "c-1", 3);
        assert_eq!(resolve(&none_open, &agent("w1:p1", HOME), HOME, "demo"), LaneResolution::None);
        assert_eq!(none_open.sightings(&[agent("w1:p1", HOME)], HOME, "demo").len(), 1, "the pane in the folder is shown whatever it resolved to");
        assert!(identity_line(&view(&none_open, vec![agent("w1:p1", HOME)], "", true)).contains("w1:p1 (none, no lane, other pane)"));
    }

    #[test]
    fn rule_5_an_agent_nowhere_near_the_project() {
        let registry = reg(vec![lane("c-1", "default")], "c-1", 2);
        assert_eq!(resolve(&registry, &agent("w2:p1", "/r/demo/threads/t-0001"), HOME, "demo"), LaneResolution::None);
        assert_eq!(resolve(&registry, &agent("w2:p1", "/home/me/repos/elsewhere"), HOME, "demo"), LaneResolution::None);
        assert_eq!(resolve(&registry, &agent("w2:p1", ""), "", "demo"), LaneResolution::None);
        // A thread's own folder under the project home is not the home.
        assert_eq!(resolve(&registry, &agent("w2:p1", &format!("{HOME}/threads/t-0001")), HOME, "demo"), LaneResolution::None);
    }

    #[test]
    fn ambiguous_is_live_but_gets_no_lane_and_no_default() {
        let two = reg(vec![lane("c-1", "default"), lane("c-2", "books")], "c-1", 3);
        let pane = agent("w1:p1", HOME);
        let answer = resolve(&two, &pane, HOME, "demo");
        assert!(answer.is_ambiguous());
        // Liveness: the project is not declared coordinator-less.
        assert!(answer.counts_as_coordinator());
        // But nothing anywhere reads it as a lane.
        assert_eq!(answer.lane_id(), None);
        let seen = two.sightings(&[pane], HOME, "demo");
        assert_eq!(seen.len(), 1);
        let rows = views(&two, &seen);
        assert!(rows.iter().all(|r| r.panes.is_empty()), "no lane claims an ambiguous pane");
        let line = identity_line(&view(&two, vec![agent("w1:p1", HOME)], "w1:p1", true));
        assert!(!line.contains("dir"), "{line}");
        assert!(line.contains("ambiguous: no token, no record match, 2 open lanes"), "{line}");
    }

    // ------------------------------------------------------ ownership input

    #[test]
    fn a_new_thread_inherits_the_lane_of_the_pane_that_started_it() {
        let (world, project) = temp_project("demo");
        let mut registry = Registry::synthesized(&project);
        registry.create("books", "收藏");
        registry.save(&project).unwrap();
        let mut worker = agent("w2:p1", "/elsewhere");
        worker.tokens.insert(TOKEN.into(), "c-2".into());
        let runner = crate::runner::fake::FakeRunner::new();
        let with_pane = |pane: &str| {
            let env = crate::paths::Env::for_test(world.path(), &[("HERDR_PANE_ID", pane)]);
            Ctx {
                env: Box::leak(Box::new(env)) as &crate::paths::Env,
                root: world.path().join("root"),
                config_dir: world.path().join("cfg"),
                runner: &runner,
                detached_ticker: false,
            }
        };
        // A known token names the lane.
        assert_eq!(lane_of_initiator(&with_pane("w2:p1"), &project, &[worker.clone()]), "c-2");
        // No `HERDR_PANE_ID`: the default lane.
        assert_eq!(lane_of_initiator(&with_pane(""), &project, &[worker.clone()]), "");
        // A pane the session does not list: the default lane.
        assert_eq!(lane_of_initiator(&with_pane("w9:p9"), &project, &[worker]), "");
        // Ambiguous (two open lanes, pane in the folder, no token): empty,
        // never a guess.
        let folder_pane = agent("w3:p1", HOME);
        assert_eq!(lane_of_initiator(&with_pane("w3:p1"), &project, &[folder_pane]), "");
    }

    // ---------------------------------------------------------- the store

    #[test]
    fn synthesis_reads_the_record_as_one_lane_and_writes_nothing() {
        let (_world, project) = temp_project("demo");
        project
            .update_coordinator(|c| {
                c.pane_id = "w1:p1".into();
                c.agent_session = "sess-1".into();
                c.profile = "pi-deepseek-flash-high".into();
            })
            .unwrap();
        let registry = Registry::for_project(&project);
        assert!(!exists(&project), "a read must not create {}", registry_path(&project).display());
        assert_eq!(registry.lanes.len(), 1);
        let lane = &registry.lanes[0];
        assert_eq!(lane.id, "c-1");
        assert_eq!(lane.name, DEFAULT_NAME);
        assert_eq!(lane.label, "Demo", "the display name, not the slug");
        assert_eq!(lane.status, Status::Open);
        assert_eq!(lane.pane_id, "w1:p1");
        assert_eq!(lane.agent_session, "sess-1");
        assert_eq!(lane.profile, "pi-deepseek-flash-high");
        assert_eq!(registry.default, "c-1");
        assert_eq!(registry.next, 2);
        assert!(!exists(&project));
    }

    #[test]
    fn synthesis_without_any_record_still_gives_one_open_lane() {
        let (_world, project) = temp_project("demo");
        let registry = Registry::for_project(&project);
        assert_eq!(registry.lanes.len(), 1);
        assert!(registry.lanes[0].pane_id.is_empty());
        assert_eq!(registry.default, "c-1");
        let home = project_home(&project);
        assert_eq!(
            resolve(&registry, &agent("w1:p1", &home), &home, "demo"),
            LaneResolution::Known {
                lane: "c-1".into(),
                rule: Rule::Dir
            }
        );
        assert!(!exists(&project));
    }

    #[test]
    fn ensure_written_creates_the_file_once_and_never_twice() {
        let (_world, project) = temp_project("demo");
        project.update_coordinator(|c| c.pane_id = "w1:p1".into()).unwrap();
        assert!(Registry::ensure_written(&project).unwrap());
        assert!(exists(&project));
        let text = std::fs::read_to_string(registry_path(&project)).unwrap();
        assert!(text.contains("\"id\": \"c-1\""), "{text}");
        assert!(text.contains("\"name\": \"default\""));
        assert!(text.contains("\"next\": 2"));
        // A second call leaves the file alone: a hand-edit survives.
        std::fs::write(registry_path(&project), text.replace("\"next\": 2", "\"next\": 9")).unwrap();
        assert!(!Registry::ensure_written(&project).unwrap());
        assert_eq!(Registry::load(&project).unwrap().next, 9);
        assert!(exists(&project));
    }

    #[test]
    fn ids_come_from_the_counter_and_never_come_back() {
        let mut registry = reg(vec![lane("c-1", "default")], "c-1", 2);
        assert_eq!(registry.create("fix", "抢修").id, "c-2");
        assert_eq!(registry.create("books", "收藏").id, "c-3");
        assert_eq!(registry.next, 4);
        // A close releases no id: c-2 stays c-2 forever.
        registry.lanes.iter_mut().find(|l| l.id == "c-2").unwrap().status = Status::Closed;
        assert_eq!(registry.create("other", "Other").id, "c-4");
        assert_eq!(registry.get("c-2").unwrap().status, Status::Closed);
        // A registry written by hand with no counter still allocates c-1 and
        // then moves past it.
        let mut fresh = Registry::default();
        assert_eq!(fresh.alloc_id(), "c-1");
        assert_eq!(fresh.alloc_id(), "c-2");
        assert_eq!(fresh.next, 3);
    }

    #[test]
    fn round_trip_keeps_every_field_and_loads_a_file_without_them() {
        let (_world, project) = temp_project("demo");
        let mut registry = reg(vec![lane("c-1", "default")], "c-1", 2);
        let lane = registry.create("books", "收藏");
        lane.goal = "bookmark features".into();
        lane.profile = "pi-deepseek-flash-high".into();
        lane.pane_id = "w1:p3".into();
        lane.agent_session = "sess-3".into();
        lane.last_active = "2026-10-25T10:42:11Z".into();
        registry.save(&project).unwrap();
        assert_eq!(Registry::load(&project).unwrap(), registry);
        // An old-style file with only some fields still loads.
        std::fs::write(
            registry_path(&project),
            r#"{"lanes":[{"id":"c-1","name":"default"}],"default":"c-1","next":2}"#,
        )
        .unwrap();
        let loaded = Registry::load(&project).unwrap();
        assert_eq!(loaded.lanes[0].status, Status::Open);
        assert_eq!(loaded.lanes[0].label, "");
        assert_eq!(loaded.lanes[0].pane_id, "");
        assert_eq!(loaded.next, 2);
        // Unknown keys are ignored, so a newer file loads in this one.
        std::fs::write(
            registry_path(&project),
            r#"{"lanes":[{"id":"c-1","name":"default","status":"closed","future":1}],"default":"c-1","next":2,"later":true}"#,
        )
        .unwrap();
        assert_eq!(Registry::load(&project).unwrap().lanes[0].status, Status::Closed);
        // A file that does not parse reads as "no registry", not as a failure.
        std::fs::write(registry_path(&project), "not json").unwrap();
        assert_eq!(Registry::load(&project), None);
        assert_eq!(Registry::for_project(&project).lanes.len(), 1);
    }

    #[test]
    fn an_empty_lane_field_reads_as_the_default_lane() {
        let registry = reg(vec![lane("c-1", "default"), lane("c-2", "books")], "c-2", 3);
        assert_eq!(registry.lane_of("").unwrap().id, "c-2");
        assert_eq!(registry.lane_of("c-1").unwrap().id, "c-1");
        assert_eq!(registry.lane_of("c-9"), None);
        assert!(registry.is_default("c-2"));
        assert!(!registry.is_default("c-1"));
    }

    #[test]
    fn the_default_lane_survives_a_closed_default_by_election() {
        let mut registry = reg(vec![lane("c-1", "default"), lane("c-2", "books")], "c-1", 3);
        assert_eq!(registry.default_lane().unwrap().id, "c-1");
        // Close the default: the open lane with the newest last_active wins.
        registry.lanes[0].status = Status::Closed;
        registry.lanes[1].last_active = "2026-02-01T00:00:00Z".into();
        assert_eq!(registry.default_lane().unwrap().id, "c-2");
        // Ties break by lowest id.
        let mut a = lane("c-7", "a");
        a.last_active = "2026-02-01T00:00:00Z".into();
        let mut b = lane("c-3", "b");
        b.last_active = "2026-02-01T00:00:00Z".into();
        let elected = reg(vec![closed_lane("c-1"), a, b], "c-1", 8);
        assert_eq!(elected.default_lane().unwrap().id, "c-3");
        // A default id the registry does not know also elects.
        let mut stranger = reg(vec![lane("c-1", "a"), lane("c-2", "b")], "c-9", 3);
        stranger.lanes[1].last_active = "2026-03-01T00:00:00Z".into();
        assert_eq!(stranger.default_lane().unwrap().id, "c-2");
        // Nothing open at all: c-1 is the default again.
        let all_closed = reg(vec![closed_lane("c-1"), closed_lane("c-2")], "c-2", 3);
        assert_eq!(all_closed.default_lane().unwrap().id, "c-1");
        let empty = Registry::default();
        assert_eq!(empty.default_lane(), None);
    }

    // --------------------------------------------------- the view layer

    #[test]
    fn the_default_lane_views_as_the_coordinator_record() {
        let (_world, project) = temp_project("demo");
        let record = project
            .update_coordinator(|c| {
                c.socket = "/s/a.sock".into();
                c.session = "main".into();
                c.workspace_id = "w1".into();
                c.tab_id = "w1:t1".into();
                c.pane_id = "w1:p1".into();
                c.agent_name = "hp-demo-coordinator".into();
                c.cwd = HOME.into();
                c.agent = "claude".into();
                c.profile = "pi-high".into();
                c.agent_session = "sess-1".into();
            })
            .unwrap();
        // No registry: `Project::coordinator()` is the record itself.
        assert_eq!(project.coordinator().unwrap(), record);
        // With one, the default lane reads back the same record, because the
        // synthesized lane is the record.
        assert_eq!(Registry::for_project(&project).default_coordinator(&project).unwrap(), record);
        assert_eq!(project.coordinator().unwrap(), record);
        // Now the lane disagrees about the pane: the view follows the lane,
        // and keeps the shared session and folder fields from the record.
        let mut registry = Registry::synthesized(&project);
        registry.lanes[0].pane_id = "w7:p3".into();
        registry.lanes[0].agent_session = "sess-9".into();
        registry.lanes[0].profile = "".into();
        registry.save(&project).unwrap();
        let view = project.coordinator().unwrap();
        assert_eq!((view.pane_id.as_str(), view.agent_session.as_str(), view.profile.as_str()), ("w7:p3", "sess-9", ""));
        assert_eq!((view.socket.as_str(), view.cwd.as_str(), view.agent_name.as_str(), view.agent.as_str()), ("/s/a.sock", HOME, "hp-demo-coordinator", "claude"));
        assert_eq!(view.workspace_id, "w1");
        assert_eq!(view.tab_id, "w1:t1");
    }

    #[test]
    fn a_project_with_no_record_at_all_has_no_coordinator_to_lose() {
        let (_world, project) = temp_project("demo");
        assert_eq!(project.coordinator(), None);
        assert_eq!(Registry::for_project(&project).default_coordinator(&project), Some(Coordinator::default()));
    }

    #[test]
    fn the_token_is_written_only_for_a_project_that_has_a_registry() {
        let (_world, project) = temp_project("demo");
        project.update_coordinator(|c| c.pane_id = "w1:p1".into()).unwrap();
        // No lanes.json: no token. Pre-lane projects report exactly what they
        // reported before lanes existed.
        assert_eq!(token_for_pane(&project, "w1:p1"), None);
        Registry::ensure_written(&project).unwrap();
        assert_eq!(token_for_pane(&project, "w1:p1"), Some("c-1".into()));
        assert_eq!(token_for_pane(&project, "w9:p9"), Some("c-1".into()), "one open lane answers for any coordinator pane");
        assert_eq!(token_for_pane(&project, ""), None);
        // Two lanes and a pane no record claims: nothing is written rather
        // than a token that might lie.
        let mut registry = Registry::load(&project).unwrap();
        let id = registry.create("books", "收藏").id.clone();
        registry.lanes.iter_mut().find(|l| l.id == id).unwrap().pane_id = "w2:p2".into();
        registry.save(&project).unwrap();
        assert_eq!(token_for_pane(&project, "w2:p2"), Some("c-2".into()));
        assert_eq!(token_for_pane(&project, "w1:p1"), Some("c-1".into()));
        assert_eq!(token_for_pane(&project, "w5:p5"), None);
    }

    // ------------------------------------------------------- rendering

    #[test]
    fn the_identity_line_of_a_single_lane_project() {
        let registry = reg(vec![lane("c-1", "default")], "c-1", 2);
        assert_eq!(
            identity_line(&view(&registry, vec![agent("w1:p1", HOME)], "w1:p1", true)),
            "Coordinators: w1:p1 (dir, lane c-1, this pane) · default lane c-1\n"
        );
        assert_eq!(
            identity_line(&view(&registry, vec![agent("w1:p1", HOME)], "w9:p9", true)),
            "Coordinators: w1:p1 (dir, lane c-1, other pane) · default lane c-1\n"
        );
        // A token read, in the doc's own shape.
        assert_eq!(
            identity_line(&view(&registry, vec![claiming("w1D3:p1", HOME, "c-1")], "w1D3:p1", true)),
            "Coordinators: w1D3:p1 (token, lane c-1, this pane) · default lane c-1\n"
        );
        // Nothing to report, and still a line: the default is named.
        assert_eq!(identity_line(&view(&registry, vec![], "w1:p1", true), ), "Coordinators: default lane c-1\n");
        assert_eq!(
            identity_line(&view(&registry, vec![], "w1:p1", false)),
            "Coordinators: session unreachable (no pane checked) · default lane c-1\n"
        );
        // A lane with no record of its own still names a default, and an empty
        // registry says there are none rather than printing nothing.
        let no_id = reg(vec![lane("c-1", "default")], "", 2);
        assert!(identity_line(&view(&no_id, vec![], "", true)).ends_with("default lane c-1\n"));
        assert_eq!(identity_line(&view(&Registry::default(), vec![], "", true)), "Coordinators: no lane\n");
    }

    #[test]
    fn an_ambiguous_pane_prints_its_reason_and_the_command_that_fixes_it() {
        let registry = reg(vec![lane_at("c-1", "fix", "w1D3:p1", "sess-1"), lane("c-2", "books")], "c-1", 3);
        let line = identity_line(&view(
            &registry,
            vec![claiming("w1D3:p1", "/elsewhere", "c-1"), agent("w1AB:p1", HOME)],
            "w1D3:p1",
            true,
        ));
        assert_eq!(
            line,
            "Coordinators: w1D3:p1 (token, lane c-1, this pane) · w1AB:p1 (ambiguous: no token, no record match, 2 open lanes share this folder — run `hp lane adopt <lane>` in it) · lanes c-1 c-2 of 2 (default c-1)\n",
            "{line}"
        );
        // Not the word "ambiguous" alone: a coordinator can hand the user a
        // one-line fix.
        assert!(line.contains("hp lane adopt"), "{line}");
    }

    #[test]
    fn a_closed_lane_stays_on_the_line_marked_closed() {
        let registry = reg(vec![lane("c-1", "default"), closed_lane("c-2")], "c-1", 3);
        assert_eq!(
            identity_line(&view(&registry, vec![claiming("w1AB:p1", "/elsewhere", "c-2")], "w1:p1", true)),
            "Coordinators: w1AB:p1 (token, lane c-2, closed, other pane) · lanes c-1 c-2 closed of 2 (default c-1)\n"
        );
    }

    #[test]
    fn views_pair_every_live_pane_with_its_lane() {
        let registry = reg(
            vec![lane_at("c-1", "default", "w1:p1", "sess-1"), lane_at("c-2", "books", "w2:p1", "sess-2")],
            "c-1",
            3,
        );
        let seen = registry.sightings(
            &[
                claiming("w1:p1", "/elsewhere", "c-1"),
                with_session("w2:p1", HOME, "sess-2"),
                agent("w4:p1", HOME),
                agent("w3:p1", "/elsewhere"),
            ],
            HOME,
            "demo",
        );
        let rows = views(&registry, &seen);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].lane.id, "c-1");
        assert!(rows[0].is_default);
        assert_eq!(rows[0].panes, vec![("w1:p1".to_string(), "token")]);
        assert!(!rows[1].is_default);
        assert_eq!(rows[1].panes, vec![("w2:p1".to_string(), "record")]);
        // The pane that refused (w4) belongs to no row — `identity_line` shows
        // it — and a pane nothing resolves (w3) is not a coordinator at all.
        assert!(!seen.iter().any(|s| s.pane_id == "w3:p1"));
        assert!(seen.iter().any(|s| s.pane_id == "w4:p1" && s.resolution.is_ambiguous()));
        // A token for a lane the registry does not have is never turned into a
        // row either: it resolves to `Ambiguous`, which the line prints.
        let seen = registry.sightings(&[claiming("w8:p8", "/elsewhere", "c-4")], HOME, "demo");
        assert_eq!(views(&registry, &seen).len(), 2);
        assert!(seen[0].resolution.is_ambiguous(), "{:?}", seen[0]);
        let line = identity_line(&view(&registry, vec![claiming("w8:p8", "/elsewhere", "c-4")], "", true));
        assert!(line.contains("ambiguous: token hp_lane=c-4 names no lane"), "{line}");
    }

    #[test]
    fn a_pane_another_project_owns_is_never_resolved_here() {
        // Lane ids are unique inside one project, not across a session: every
        // project's first lane is `c-1`. All of this is one Herdr session.
        let registry = reg(vec![lane("c-1", "default")], "c-1", 2);
        let mut mine = agent("w1:p1", "/elsewhere");
        mine.tokens.insert(crate::sidebar::PROJECT_TOKEN.into(), "demo".into());
        mine.tokens.insert(TOKEN.into(), "c-1".into());
        assert_eq!(
            resolve(&registry, &mine, HOME, "demo"),
            LaneResolution::Known {
                lane: "c-1".into(),
                rule: Rule::Token
            }
        );
        // The very same pane and tokens, asked by another project: nobody's,
        // and not even printed as if it were.
        assert_eq!(resolve(&registry, &mine, HOME, "law"), LaneResolution::None);
        assert!(registry.sightings(&[mine.clone()], HOME, "law").is_empty());
        assert_eq!(registry.sightings(&[mine], HOME, "demo").len(), 1);
        // A pane that names no project at all — a fresh report, an expired
        // TTL, one started by hand — is judged by the rules, not refused for
        // silence.
        let mut blank = agent("w1:p1", "/elsewhere");
        blank.tokens.insert(TOKEN.into(), "c-1".into());
        assert_eq!(resolve(&registry, &blank, HOME, "law").lane_id(), Some("c-1"));
        // With neither a project token nor a lane token it falls to the folder,
        // exactly as it did before lanes: here, nowhere near it, so nobody's.
        blank.tokens = Default::default();
        assert_eq!(resolve(&registry, &blank, HOME, "law"), LaneResolution::None);
    }

    #[test]
    fn an_unrecognized_pane_reading_its_own_digest_says_so() {
        // The `herdr-projects` incident: the coordinator's shell sat in
        // `/Users/maxyou`, its recorded session no longer matched, and so every
        // rule missed. The project was treated as having no coordinator, nudges
        // stopped, and nothing was logged for a week.
        let registry = reg(vec![lane_at("c-1", "default", "w1D3:p1", "old-session")], "c-1", 2);
        let drifted = with_session("w1D3:p1", "/Users/maxyou", "new-session");
        assert_eq!(resolve(&registry, &drifted, HOME, "demo"), LaneResolution::None);
        // Read from that pane, the line names it and gives the fix.
        assert_eq!(
            identity_line(&view(&registry, vec![drifted.clone()], "w1D3:p1", true)),
            "Coordinators: w1D3:p1 (degraded, lane c-1, this pane: its recorded session no longer matches and it works in /Users/maxyou, outside /r/demo) — run `hp lane adopt c-1` in it · default lane c-1\n"
        );
        // A pane that resolves is never also called degraded.
        let matched = identity_line(&view(&registry, vec![with_session("w1D3:p1", "/Users/maxyou", "old-session")], "w1D3:p1", true));
        assert_eq!(matched, "Coordinators: w1D3:p1 (record, lane c-1, this pane) · default lane c-1\n");
        // Another project's pane is not this project's degraded lane either.
        let mut theirs = with_session("w1D3:p1", "/Users/maxyou", "new-session");
        theirs.tokens.insert(crate::sidebar::PROJECT_TOKEN.into(), "other".into());
        let line = identity_line(&view(&registry, vec![theirs], "w1D3:p1", true));
        assert!(!line.contains("degraded"), "{line}");
        assert!(line.contains("this pane w1D3:p1 is no lane's coordinator"), "{line}");
    }

    #[test]
    fn a_pane_is_named_once_however_many_lanes_point_at_it() {
        // Found by running the real binary: a pane that is already on the line
        // as `ambiguous` must not also appear as a second entry about its lane,
        // or the reader counts two coordinators where one sits.
        let registry = reg(
            vec![lane_at("c-1", "fix", "w1D3:p1", "sess-1"), lane_at("c-2", "books", "w1AB:p1", "sess-old")],
            "c-1",
            3,
        );
        let line = identity_line(&view(
            &registry,
            vec![claiming("w1D3:p1", "/elsewhere", "c-1"), agent("w1AB:p1", HOME)],
            "",
            true,
        ));
        assert_eq!(
            line,
            "Coordinators: w1D3:p1 (token, lane c-1, other pane) · w1AB:p1 (ambiguous: no token, no record match, 2 open lanes share this folder — run `hp lane adopt <lane>` in it) · lanes c-1 c-2 of 2 (default c-1)\n",
            "{line}"
        );
        assert_eq!(line.matches("w1AB:p1").count(), 1, "one entry per pane: {line}");
    }

    #[test]
    fn a_lane_whose_pane_is_alive_is_never_called_gone() {
        // Also found by running it: the orphan note used to say "the pane it
        // last ran in is gone" about a pane that was listed and alive, and told
        // the user to `open`, which starts a second coordinator beside the one
        // already running (§6.4 says exactly that this must not be suggested).
        let registry = reg(vec![lane_at("c-1", "default", "w1:p1", "sess-1")], "c-1", 2);
        // The pane lives outside the folder with a new session: degraded, not
        // gone, and `lane list` must not offer `open` for it.
        let rendered = view(&registry, vec![with_session("w1:p1", "/elsewhere", "sess-2")], "", true);
        let text = rendered.list_text();
        assert!(text.contains("degraded"), "{text}");
        assert!(!text.contains("has no live coordinator"), "{text}");
        assert!(!text.contains("open demo --lane c-1"), "{text}");
        assert_eq!(list_json(&rendered)[0]["orphan"], serde_json::json!(false));
        // The same lane with its pane actually gone: then, and only then, the
        // note appears.
        let gone = view(&registry, vec![with_session("w9:p9", "/elsewhere", "sess-9")], "w9:p9", true);
        let text = gone.list_text();
        assert!(text.contains("lane c-1 (default) has no live coordinator; the pane it last ran in, w1:p1, is gone — start one with `open demo --lane c-1`"), "{text}");
        assert_eq!(list_json(&gone)[0]["orphan"], serde_json::json!(true));
        // Never started: no recorded pane, and no claim that one was lost.
        let fresh = view(&reg(vec![lane("c-1", "default")], "c-1", 2), vec![], "", true);
        assert!(fresh.list_text().contains("lane c-1 (default) has no live coordinator — start one with `open demo --lane c-1`"), "{}", fresh.list_text());
    }

    #[test]
    fn an_unclaimed_lane_reports_its_status_without_renaming_its_pane() {
        // A lane whose recorded pane is listed, in the folder, with the very
        // session recorded — and yet another lane's token took it: the folder
        // pane still needs explaining, and "nothing explains it" is a fact.
        let registry = reg(vec![lane_at("c-1", "default", "w1:p1", "sess-1")], "c-1", 2);
        let statuses = view(&registry, vec![with_session("w1:p1", HOME, "sess-1")], "", true).lane_statuses();
        assert!(statuses.is_empty(), "the pane claimed it: {:?}", statuses);
        // A second lane and an unrecorded folder pane: the pane is shown as
        // ambiguous, the other lane is not orphaned by a pane it never had.
        let two = reg(vec![lane_at("c-1", "default", "w1:p1", "sess-1"), lane("c-2", "books")], "c-1", 3);
        let statuses = view(&two, vec![with_session("w1:p1", HOME, "sess-1")], "", true).lane_statuses();
        assert_eq!(statuses.len(), 1);
        assert!(statuses[0].orphan, "c-2 never had a pane: {}", statuses[0].why);
        assert_eq!(statuses[0].pane_id, None);
    }

    #[test]
    fn a_handle_resolves_to_a_lane_by_id_then_name_then_label() {
        let registry = reg(
            vec![lane_named("c-1", "fix", "抢修"), lane_named("c-2", "books", "收藏")],
            "c-1",
            3,
        );
        // An id is a handle that never changes, so it answers first: a lane
        // named after another lane's id still resolves to the id's owner.
        assert_eq!(registry.find("c-2").unwrap().name, "books");
        assert_eq!(registry.find("fix").unwrap().id, "c-1");
        // A display label works too, which is what a coordinator quoting the
        // sidebar typed.
        assert_eq!(registry.find("收藏").unwrap().id, "c-2");
        assert_eq!(registry.find("c-9"), None);
        assert_eq!(registry.find(""), None);
        // A renamed handle is not the old one any more; the id stays.
        let mut renamed = registry.clone();
        renamed.lanes[0].name = "incidents".into();
        assert_eq!(renamed.find("incidents").unwrap().id, "c-1");
        assert_eq!(renamed.find("fix"), None);
    }

    #[test]
    fn resolving_against_a_project_reads_its_registry_and_its_home() {
        let (_world, project) = temp_project("demo");
        let home = project_home(&project);
        project.update_coordinator(|c| c.cwd = home.clone()).unwrap();
        // No registry, one synthesized lane, a pane in the home: the old answer.
        assert_eq!(
            resolve_in_project(&project, &agent("w1:p1", &home)),
            LaneResolution::Known {
                lane: "c-1".into(),
                rule: Rule::Dir
            }
        );
        // Two lanes written by hand: the same pane now refuses.
        let mut registry = Registry::synthesized(&project);
        registry.create("books", "收藏");
        registry.save(&project).unwrap();
        assert!(resolve_in_project(&project, &agent("w1:p1", &home)).is_ambiguous());
        // The lane's own recorded pane is still answerable from any folder.
        let mut agents = agent("w7:p7", "/elsewhere");
        agents.agent_session = Some(AgentSession { value: "s7".into() });
        assert_eq!(resolve_in_project(&project, &agents), LaneResolution::None);
        registry.lanes[1].pane_id = "w7:p7".into();
        registry.lanes[1].agent_session = "s7".into();
        registry.save(&project).unwrap();
        assert_eq!(
            resolve_in_project(&project, &agents),
            LaneResolution::Known {
                lane: "c-2".into(),
                rule: Rule::Record
            }
        );
    }

    #[test]
    fn lane_list_counts_threads_and_unhandled_items_for_each_lane() {
        let (_world, project) = temp_project("demo");
        let mut registry = Registry::synthesized(&project);
        registry.create("books", "收藏");
        registry.save(&project).unwrap();
        crate::thread::allocate(&project, |t| {
            t.title = "one".into();
            t.lane = "c-2".into();
            t.status = crate::thread::Status::Resolved;
        })
        .unwrap();
        crate::thread::allocate(&project, |t| {
            t.title = "two".into();
            t.lane = "c-2".into();
        })
        .unwrap();
        crate::inbox::write_in(&project, "c-2", "thread-state", "t-0001", "idle", "s", "").unwrap();
        crate::inbox::write_in(&project, "c-1", "thread-state", "t-0002", "idle", "s", "").unwrap();
        let counts = lane_counts(&project);
        assert_eq!(counts["c-1"], LaneCounts { threads: 0, inbox: 1 });
        assert_eq!(counts["c-2"], LaneCounts { threads: 2, inbox: 1 }, "resolved threads keep their lane's history");
        let rendered = View {
            registry,
            agents: vec![],
            home: HOME.into(),
            slug: "demo".into(),
            this_pane: "".into(),
            reachable: true,
            counts,
        };
        let text = rendered.list_text();
        assert!(text.contains("c-1\tdefault\topen\tdefault\tDemo\tlast_active=-\tpanes=-\tthreads=0\tinbox=1"), "{text}");
        assert!(text.contains("c-2\t\topen\tbooks\t收藏\tlast_active=-\tpanes=-\tthreads=2\tinbox=1"), "{text}");
        let json = list_json(&rendered);
        assert_eq!(json[1]["threads"], 2);
        assert_eq!(json[1]["inbox"], 1);
    }

    #[test]
    fn lane_list_prints_the_identity_line_and_one_row_per_lane() {
        let registry = reg(
            vec![
                lane_at_named("c-1", "fix", "抢修", "w1D3:p1", "sess-1"),
                Lane {
                    last_active: "2026-10-25T10:42:11Z".into(),
                    ..lane_named("c-2", "books", "收藏")
                },
            ],
            "c-1",
            3,
        );
        let agents = vec![claiming("w1D3:p1", "/elsewhere", "c-1"), agent("w1AB:p1", HOME)];
        let rendered = View {
            registry: registry.clone(),
            agents: agents.clone(),
            home: HOME.into(),
            slug: "demo".into(),
            this_pane: "w1D3:p1".into(),
            reachable: true,
            counts: BTreeMap::new(),
        };
        let text = rendered.list_text();
        let lines: Vec<&str> = text.lines().collect();
        // The listing's first line *is* the digest's line: one function, so
        // the two views §5.4 requires to agree cannot drift.
        assert_eq!(lines[0], identity_line(&rendered).trim_end(), "same facts, same words");
        assert_eq!(lines[1], "c-1\tdefault\topen\tfix\t抢修\tlast_active=-\tpanes=w1D3:p1(token)", "{lines:?}");
        assert_eq!(lines[2], "c-2\t\topen\tbooks\t收藏\tlast_active=2026-10-25T10:42:11Z\tpanes=-", "{lines:?}");
        // c-2 is open with no live coordinator, and the listing says it rather
        // than leaving `panes=-` to be noticed.
        assert_eq!(lines[3], "lane c-2 (收藏) has no live coordinator — start one with `open demo --lane c-2`", "{lines:?}");
        // The ambiguous pane is on the identity line but claims no lane, so it
        // is in no row: that is why both are printed together.
        assert!(!text.contains("w1AB:p1(token)"), "{text}");
        // An empty registry says so instead of printing nothing.
        let nothing = view(&Registry::default(), vec![], "", false);
        assert_eq!(identity_line(&nothing), "Coordinators: session unreachable (no pane checked) · no lane\n");
        // The JSON carries the same pairing, plus the orphan flag.
        let value = list_json(&rendered);
        assert_eq!(value[0]["panes"][0]["rule"], "token");
        assert_eq!(value[0]["panes"][0]["pane"], "w1D3:p1");
        assert_eq!(value[0]["orphan"], false);
        assert_eq!(value[1]["panes"].as_array().unwrap().len(), 0);
        assert_eq!(value[1]["orphan"], true);
        assert_eq!(value[0]["status"], "open");
        assert_eq!(value[0]["default"], true);
        assert_eq!(value[0]["label"], "抢修");
    }

    /// A thread record and an inbox item written before the `lane` field
    /// exists still load, and read as the default lane (§10: zero migration).
    #[test]
    fn old_lane_fields_load_as_the_default_lane() {
        let (_world, project) = temp_project("demo");
        std::fs::write(
            crate::thread::record_path(&project, "t-0001"),
            "id = 't-0001'\ntitle = 'Task'\nstatus = 'open'\nkind = 'worktree'\npane_id = 'w2:p1'\n",
        )
        .unwrap();
        let t = crate::thread::load(&project, "t-0001").expect("a pre-lanes record still loads");
        assert_eq!(t.lane, "");
        let registry = Registry::for_project(&project);
        assert_eq!(registry.lane_of(&t.lane).unwrap().id, FALLBACK_ID, "empty reads as the default lane");
        // And it re-saves with the new key, which an old binary ignores.
        crate::thread::update(&project, "t-0001", |t| t.title = "Task".into()).unwrap();
        let text = std::fs::read_to_string(crate::thread::record_path(&project, "t-0001")).unwrap();
        assert!(text.contains("lane = \"\""), "the new key is written: {text}");
        assert_eq!(crate::thread::load(&project, "t-0001").unwrap().lane, "");
        // An inbox item of the same shape.
        let item: crate::inbox::Item = toml::from_str("id = \"x\"\nkind = \"thread-state\"\nsubject = \"t-0001\"\ncreated = \"2026-10-25T00:00:00Z\"\nsummary = \"s\"\nevent = \"new report\"\n")
            .expect("a pre-lanes item still parses");
        assert_eq!(item.lane, "");
        assert_eq!(registry.lane_of(&item.lane).unwrap().id, FALLBACK_ID);
        let written = toml::to_string(&item).unwrap();
        assert!(written.contains("lane = \"\""), "{written}");
        // A record naming a lane keeps the id, not a name or a label.
        let named: crate::inbox::Item = toml::from_str(&format!("{}lane = \"c-2\"\n", written.replace("lane = \"\"\n", "")))
            .unwrap();
        assert_eq!(named.lane, "c-2");
        assert_eq!(registry.lane_of(&named.lane), None, "a lane the project does not have names nothing");
    }

    #[test]
    fn the_lane_token_is_reported_never_cleared_and_cleared_on_release() {
        assert_eq!(TOKEN, "hp_lane");
        // `OLD_TOKENS` is the `--clear-token` list of *every* report: a token
        // that joins it is erased as fast as it is written (§5.1).
        assert!(!crate::sidebar::OLD_TOKENS.contains(&TOKEN), "{TOKEN} would be cleared on every report");
        assert!(!crate::grouping::tokens().iter().any(|t| t == TOKEN));
        assert!(!crate::grouping::legacy().iter().any(|t| t == TOKEN));
        // Releasing a pane clears the claim, in both places that clear.
        let (_world, project) = temp_project("demo");
        Registry::ensure_written(&project).unwrap();
        let runner = crate::runner::fake::FakeRunner::new();
        runner.on("report-metadata", crate::runner::fake::ok("{}"));
        let herdr = crate::herdr::Herdr::new("herdr", "/s/a.sock", &runner);
        crate::coordinator::report_tokens(&herdr, &project, "w1:p1");
        let calls = runner.calls.borrow();
        let report = calls.iter().find(|c| c.display().contains("report-metadata")).expect("report");
        let text = report.display();
        assert!(text.contains(&format!("--token {TOKEN}=c-1")), "{text}");
        assert!(!text.contains(&format!("--clear-token {TOKEN}")), "{text}");
        drop(calls);
        runner.calls.borrow_mut().clear();
        crate::sidebar::clear_pane(&herdr, "w1:p1");
        let cleared: Vec<String> = runner.calls.borrow().iter().map(|c| c.display()).collect();
        assert!(cleared.iter().any(|t| t.contains(&format!("--clear-token {TOKEN}"))), "{cleared:?}");
        runner.calls.borrow_mut().clear();
        crate::grouping::clear(&herdr, "pane", "w1:p1");
        let cleared: Vec<String> = runner.calls.borrow().iter().map(|c| c.display()).collect();
        assert!(cleared.iter().any(|t| t.contains(&format!("--clear-token {TOKEN}"))), "{cleared:?}");
        assert_eq!(lane("c-1", "default").token_value(), Some("c-1"));
        assert_eq!(Lane::default().token_value(), None);
    }

    #[test]
    fn a_report_writes_the_token_only_when_a_lane_says_so() {
        let (_world, project) = temp_project("demo");
        let runner = crate::runner::fake::FakeRunner::new();
        runner.on("report-metadata", crate::runner::fake::ok("{}"));
        let herdr = crate::herdr::Herdr::new("herdr", "/s/a.sock", &runner);
        let reports = |runner: &crate::runner::fake::FakeRunner| -> Vec<String> {
            runner
                .calls
                .borrow()
                .iter()
                .filter(|c| c.display().contains("report-metadata"))
                .map(|c| c.display())
                .collect()
        };
        crate::coordinator::report_tokens(&herdr, &project, "w1:p1");
        let first = reports(&runner);
        assert_eq!(first.len(), 1, "one report per pane");
        assert!(!first[0].contains(TOKEN), "no registry, no token: {}", first[0]);
        Registry::ensure_written(&project).unwrap();
        crate::coordinator::report_tokens(&herdr, &project, "w1:p1");
        let second = reports(&runner);
        assert!(second[1].contains(&format!("--token {TOKEN}=c-1")), "{}", second[1]);
        // A thread pane never claims a lane.
        crate::sidebar::report_pane(&herdr, "w2:p1", "t-0001 · Task", "demo", crate::thread::Group::Working, None);
        let third = reports(&runner);
        assert!(!third[2].contains(TOKEN), "{}", third[2]);
    }

    // -------------------------------------------------- routing (commit 3)

    #[test]
    fn classify_keeps_the_old_single_record_for_a_project_without_a_registry() {
        let (_world, project) = temp_project("demo");
        let mut record = crate::project::Coordinator { cwd: project.canonical_dir().to_string_lossy().into_owned(), ..Default::default() };
        let home = record.cwd.clone();
        assert_eq!(
            classify(&project, &record, &agent("w1:p1", &home)),
            LaneResolution::Known {
                lane: FALLBACK_ID.into(),
                rule: Rule::Dir
            }
        );
        assert!(classify(&project, &record, &agent("w1:p1", "/elsewhere")).is_none());
        // The recorded pane counts from anywhere while its session matches,
        // exactly like `is_coordinator`.
        record.pane_id = "w2:p1".into();
        record.agent_session = "sess-2".into();
        assert_eq!(classify(&project, &record, &with_session("w2:p1", "/elsewhere", "sess-2")).lane_id(), Some(FALLBACK_ID));
    }

    #[test]
    fn classify_uses_the_resolver_once_a_registry_exists() {
        let (_world, project) = temp_project("demo");
        reg(vec![lane("c-1", "default"), lane("c-2", "books")], "c-1", 3).save(&project).unwrap();
        let home = project.canonical_dir().to_string_lossy().into_owned();
        // Two open lanes and a pane in the folder: a refusal, not a guess.
        assert!(classify(&project, &crate::project::Coordinator::default(), &agent("w1:p1", &home)).is_ambiguous());
        // A token picks its lane.
        assert_eq!(
            classify(&project, &crate::project::Coordinator::default(), &claiming("w1:p1", "/elsewhere", "c-2")).lane_id(),
            Some("c-2")
        );
    }

    #[test]
    fn routes_are_the_open_lanes_and_the_one_default_lane_without_a_registry() {
        let (_world, project) = temp_project("demo");
        assert_eq!(routes(&project), [FALLBACK_ID]);
        reg(vec![lane("c-1", "default"), closed_lane("c-2")], "c-1", 3).save(&project).unwrap();
        assert_eq!(routes(&project), ["c-1"], "a closed lane takes no nudge");
    }

    #[test]
    fn an_orphan_lane_has_live_threads_and_no_live_pane_but_a_degraded_one_is_not_an_orphan() {
        let (_world, project) = temp_project("demo");
        reg(vec![lane_at("c-1", "default", "w1:p1", "sess-1"), lane_at("c-2", "books", "w2:p1", "sess-2")], "c-1", 3)
            .save(&project)
            .unwrap();
        for lane in ["c-1", "c-2"] {
            crate::thread::allocate(&project, |t| {
                t.title = lane.into();
                t.status = crate::thread::Status::Open;
                t.lane = lane.into();
            })
            .unwrap();
        }
        // Both coordinators are alive: no orphan at all.
        let alive = vec![with_session("w1:p1", "/elsewhere", "sess-1"), with_session("w2:p1", "/elsewhere", "sess-2")];
        assert!(orphan_lanes(&project, &alive).is_empty());
        // c-2's pane is gone, c-1's is there: only c-2 is an orphan, with its
        // open thread count.
        let found = orphan_lanes(&project, &[with_session("w1:p1", "/elsewhere", "sess-1")]);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].0.id.as_str(), found[0].1), ("c-2", 1));
        // c-2's pane is alive but no rule matches it (§5.4's degraded case):
        // not an orphan, because `open` would start a second coordinator
        // beside it.
        let degraded = with_session("w2:p1", "/elsewhere", "other-session");
        assert!(orphan_lanes(&project, &[with_session("w1:p1", "/elsewhere", "sess-1"), degraded]).is_empty());
        // A lane with no live thread is not an orphan either, however gone its
        // coordinator is.
        let threads: Vec<String> = crate::thread::list(&project).into_iter().map(|t| t.id).collect();
        for id in &threads {
            crate::thread::update(&project, id, |t| t.status = crate::thread::Status::Resolved).unwrap();
        }
        assert!(orphan_lanes(&project, &[]).is_empty());
        // Without a registry there is no lane to orphan.
        let (_world2, plain) = temp_project("plain");
        crate::thread::allocate(&plain, |t| t.status = crate::thread::Status::Open).unwrap();
        assert!(orphan_lanes(&plain, &[]).is_empty());
    }
}
