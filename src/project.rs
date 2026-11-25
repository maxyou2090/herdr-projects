//! Project folders under the root: slugs, settings, the per-project lock and
//! the coordinator record.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const MAX_SLUG: usize = 40;
pub const BODY_WARN_CHARS: usize = 16_000;

/// A slug matches `[a-z0-9][a-z0-9-]*` and is at most 40 characters. Every
/// subcommand validates the slug it is given before building any path from it.
pub fn validate_slug(slug: &str) -> Result<()> {
    let mut chars = slug.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !first_ok || !rest_ok || slug.len() > MAX_SLUG {
        bail!("`{slug}` is not a valid slug (lower-case letters, digits and hyphens, at most {MAX_SLUG} characters)");
    }
    Ok(())
}

/// Lower-cases and turns each run of other characters into one hyphen. Used for
/// project names and for thread titles in branch names.
pub fn slugify(text: &str) -> String {
    let mut slug = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let mut slug: String = slug.chars().take(MAX_SLUG).collect();
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

/// The slug `new` gives a project name, refusing names that look like paths.
pub fn slug_from_name(name: &str) -> Result<String> {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        bail!("a project name may not contain `/`, `\\` or `..`");
    }
    let slug = slugify(name);
    if slug.is_empty() {
        bail!("`{name}` has no letters or digits to make a slug from");
    }
    validate_slug(&slug)?;
    Ok(slug)
}

/// Words split on `-` and `_`, each with its first letter upper-cased:
/// `herdr-projects` becomes `Herdr Projects`. Plain title case, so `gtm-ai`
/// becomes `Gtm Ai`; a user who wants `GTM AI` sets `name` in PROJECT.md.
pub fn humanize(slug: &str) -> String {
    slug.split(['-', '_'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map(|first| first.to_uppercase().chain(chars).collect::<String>()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The name a project shows: `name` as given unless it is empty or looks like
/// a slug (lower-case letters, digits, `-` and `_` only), else the humanized
/// form of it or of `slug`. A herdr workspace never shows a bare slug, which
/// would read the same as a repository's own workspace.
pub fn display_name(name: &str, slug: &str) -> String {
    let name = name.trim();
    let base = if name.is_empty() { slug } else { name };
    let slug_like = base.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if slug_like { humanize(base) } else { base.to_string() }
}

/// The label of a project's home Space: the name a person reads, which is the
/// same string its row and its briefs carry ([`display_name`]).
pub fn home_label(name: &str, slug: &str) -> String {
    display_name(name, slug)
}

/// Writes through a temporary file in the same directory plus a rename. It never
/// creates parent directories: only `new` creates a project's directories.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    let name = path.file_name().context("path has no file name")?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("could not write {}", path.display()))
}

pub fn now() -> String {
    jiff::Timestamp::now()
        .round(jiff::Unit::Second)
        .map(|t| t.to_string())
        .unwrap_or_default()
}

/// A repository the project works in. `[[repos]]` in PROJECT.md; a line left
/// by an older version may carry `machine = "..."`, which serde ignores.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Repo {
    pub path: String,
    /// The ref this repository's threads start from, when it differs from the
    /// project's `base`: one project over repositories whose default branches
    /// differ. Hand-edited in PROJECT.md; empty means the project's `base`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base: String,
}

/// `PROJECT.md` front matter. `repos` is last so the TOML tables follow the
/// plain keys when `new` serializes it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub name: String,
    /// The profile `open` starts a coordinator with (`coordinator_agent`
    /// before profiles; a Herdr kind is a built-in profile).
    #[serde(alias = "coordinator_agent")]
    pub coordinator_profile: String,
    /// The profile a thread starts with when none is chosen.
    #[serde(alias = "thread_agent")]
    pub thread_profile: String,
    pub auto_resolve_days: u32,
    /// How long a thread that produced nothing (no pull request, no commit of
    /// its own, a report nobody changed) must sit still before the ticker
    /// resolves it (`15m`, `90m`, `off`). See [`NoprWindow`].
    #[serde(default, skip_serializing_if = "nopr_is_off")]
    pub auto_resolve_nopr: NoprWindow,
    pub nudge: bool,
    /// How far into one turn of the project's own coordinator the ticker starts
    /// judging it (`10m`, `5m`, `off`); a turn is only stopped when it has
    /// stopped moving, never for its length. See [`TurnLimit`].
    #[serde(default)]
    pub coordinator_turn_limit: TurnLimit,
    /// The ref a new worktree thread starts from when `--base` is not given
    /// (`port/herdr-core`, `origin/main`), checked against the repository as it
    /// is used. Empty, the default, keeps what came before: git's own choice of
    /// the remote's default branch (`origin/HEAD`, and `origin/main` in most
    /// repositories) — `new` never writes it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base: String,
    pub repos: Vec<Repo>,
}

/// `coordinator_turn_limit` in PROJECT.md: **the earliest** a coordinator's
/// turn is judged, not the point at which it is cut.
///
/// A coordinator's turn is supposed to be an answer, an `hp task`, an
/// `hp thread start`. The 2026-11-21 incident was 41 minutes of `sleep 45; ssh`
/// retries in a coordinator's own turn, 28 dollars of tokens, and a
/// ten-minute warning that could not be delivered into the busy turn it was
/// about. But a turn only ran 20 minutes and was cut in the middle of work a
/// person was waiting for (2026-11-25), which is the opposite failure: the
/// length of a turn is not evidence of anything. What the ticker stops is a
/// turn that stops **moving** — a pane whose screen has not changed for ten
/// minutes, or one command shape three times (see
/// [`crate::steps::interrupt_long_turns`]) — and this setting is when that look
/// starts. Off means nothing is ever stopped, not even a frozen pane.
///
/// Ten minutes, matching the audit's warning band
/// ([`crate::audit::COORDINATOR_LONG_TURN_SECS`]): the warning says the turn is
/// long, the stop is for one that is stuck. A legitimate slow `hp` action
/// (`thread resolve` copying a big tree, `open` starting an agent) prints
/// nothing for a while, so raising this value delays the judgement; it cannot
/// make a working turn be cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnLimit {
    /// `off`: no turn is ever interrupted.
    Off,
    /// Minutes, from the `10m` shape.
    Minutes(i64),
}

impl Default for TurnLimit {
    fn default() -> Self {
        TurnLimit::Minutes(TurnLimit::DEFAULT_MINUTES)
    }
}

impl TurnLimit {
    /// The default: ten minutes, the value docs/operations.md explains, and the
    /// same ten the audit's warning uses.
    pub const DEFAULT_MINUTES: i64 = 10;

    /// Parses `off` or `<minutes>m`. Minutes only: a turn is measured and read
    /// in minutes, and `20` without a unit would be as ambiguous as it looks.
    pub fn parse(text: &str) -> Result<TurnLimit, String> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("off") {
            return Ok(TurnLimit::Off);
        }
        let Some(minutes) = text.strip_suffix('m').and_then(|n| n.trim().parse::<i64>().ok()) else {
            return Err(format!("`{text}` is not a coordinator_turn_limit; write `10m` or `off`"));
        };
        if !(1..=1440).contains(&minutes) {
            return Err(format!("`{text}` is not a coordinator_turn_limit; write 1m to 1440m, or `off`"));
        }
        Ok(TurnLimit::Minutes(minutes))
    }

    /// How many seconds into a turn the ticker starts judging it, `None` for
    /// `off` (nothing is stopped at all, a frozen pane included).
    pub fn secs(self) -> Option<i64> {
        match self {
            TurnLimit::Off => None,
            TurnLimit::Minutes(minutes) => Some(minutes * 60),
        }
    }

    /// What PROJECT.md says, so a rewrite keeps the same value.
    pub fn text(self) -> String {
        match self {
            TurnLimit::Off => "off".into(),
            TurnLimit::Minutes(minutes) => format!("{minutes}m"),
        }
    }
}

impl std::fmt::Display for TurnLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text())
    }
}

/// `auto_resolve_nopr` in PROJECT.md: how long a thread that produced nothing
/// waits before the ticker resolves it by itself.
///
/// "Produced nothing" is a hard test (§[`crate::steps::auto_resolve_nopr`]):
/// no pull request, no commit on a branch of its own, a clean worktree, a
/// report that has not changed, an agent that is not working. The threads it
/// is for are the smoke tests, checks and investigations that end with a
/// report and nothing else — they used to sit in `Idle` until a person
/// resolved them by hand or `auto_resolve_days` (a week) passed.
///
/// `off` — the default — leaves every project exactly as it was: no record,
/// no git call, no item. Fifteen minutes is the value to start with: a thread
/// that lands a commit or opens a pull request a quarter of an hour after its
/// report is a shape nobody has seen, and `thread resolve --reopen` undoes a
/// resolve in one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoprWindow {
    /// `off`: nothing is resolved by this rule.
    Off,
    /// Minutes, from the `15m` shape.
    Minutes(i64),
}

impl Default for NoprWindow {
    fn default() -> Self {
        NoprWindow::Off
    }
}

impl NoprWindow {
    /// Parses `off` or `<minutes>m`. Minutes only, like [`TurnLimit`]: `15`
    /// without a unit reads as an absolute time as easily as a duration.
    pub fn parse(text: &str) -> Result<NoprWindow, String> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("off") {
            return Ok(NoprWindow::Off);
        }
        let Some(minutes) = text.strip_suffix('m').and_then(|n| n.trim().parse::<i64>().ok()) else {
            return Err(format!("`{text}` is not an auto_resolve_nopr; write `15m` or `off`"));
        };
        if !(1..=1440).contains(&minutes) {
            return Err(format!("`{text}` is not an auto_resolve_nopr; write 1m to 1440m, or `off`"));
        }
        Ok(NoprWindow::Minutes(minutes))
    }

    /// How long the report must stand still, `None` for `off`.
    pub fn secs(self) -> Option<i64> {
        match self {
            NoprWindow::Off => None,
            NoprWindow::Minutes(minutes) => Some(minutes * 60),
        }
    }

    /// What PROJECT.md says, so a rewrite keeps the same value.
    pub fn text(self) -> String {
        match self {
            NoprWindow::Off => "off".into(),
            NoprWindow::Minutes(minutes) => format!("{minutes}m"),
        }
    }
}

impl std::fmt::Display for NoprWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text())
    }
}

impl Serialize for NoprWindow {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text())
    }
}

impl<'de> Deserialize<'de> for NoprWindow {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<NoprWindow, D::Error> {
        let text = String::deserialize(deserializer)?;
        // A bad value is a `config-error` item, never a panic and never a
        // silent fallback to a window the project did not ask for.
        NoprWindow::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl Serialize for TurnLimit {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text())
    }
}

impl<'de> Deserialize<'de> for TurnLimit {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<TurnLimit, D::Error> {
        let text = String::deserialize(deserializer)?;
        // A bad value is a `config-error` item (the ticker writes one for a
        // PROJECT.md that does not parse), never a panic and never a silent
        // fallback to a limit the project did not ask for.
        TurnLimit::parse(&text).map_err(serde::de::Error::custom)
    }
}

fn nopr_is_off(window: &NoprWindow) -> bool {
    *window == NoprWindow::Off
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            name: String::new(),
            coordinator_profile: "claude".into(),
            thread_profile: "claude".into(),
            auto_resolve_days: 7,
            auto_resolve_nopr: NoprWindow::Off,
            // On by default (W15): the ticker prompts only a coordinator that
            // has been idle for a quarter minute and whose input box has looked
            // empty for ten seconds, because on herdr 0.9.1 a prompt merges with
            // half-typed text (docs/history/herdr-notes-herdr1.md, stage 2).
            nudge: true,
            coordinator_turn_limit: TurnLimit::default(),
            base: String::new(),
            repos: Vec::new(),
        }
    }
}

/// Splits `+++` TOML front matter from the body.
pub fn parse_project_md(text: &str) -> Result<(Settings, String)> {
    let rest = text
        .strip_prefix("+++\n")
        .context("PROJECT.md must start with a `+++` line")?;
    let (front, body) = match rest.split_once("\n+++\n") {
        Some(parts) => parts,
        None => rest
            .strip_suffix("\n+++")
            .map(|front| (front, ""))
            .context("PROJECT.md front matter has no closing `+++` line")?,
    };
    let settings: Settings = toml::from_str(front).context("PROJECT.md front matter does not parse")?;
    Ok((settings, body.trim_start_matches('\n').to_string()))
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
struct ProjectState {
    /// Slugs the project had before `rename`, oldest first: its threads'
    /// branches (`hp/<slug>/...`) keep the name they were made with.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    former_slugs: Vec<String>,
}

/// The session and workspace the project belongs to, and the coordinator pane
/// `open` last started or focused. Any agent whose working directory is `cwd`
/// is a coordinator; the ticker lists them in `.state/coordinators.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Coordinator {
    pub socket: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    pub agent_name: String,
    /// The canonical project folder.
    pub cwd: String,
    /// The Herdr agent kind `open` last started.
    pub agent: String,
    /// The profile it started with (empty before profiles: the built-in of
    /// `agent`). A resume or a reuse needs the same profile.
    pub profile: String,
    /// The last native session id Herdr reported for that kind, for resume.
    pub agent_session: String,
    pub updated: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Safety {
    pub start_threads: String,
    pub coordinator_agent_args: Vec<String>,
    pub thread_agent_args: Vec<String>,
    /// Yolo mode: threads start without asking and every agent launches with
    /// its harness's skip-permissions flag (`crate::safety::yolo_flags`).
    pub yolo: bool,
    /// Who answers trust screens in thread panes: `coordinator` or `user`.
    /// Unset, it follows yolo (on: the coordinator; off: the user).
    pub trust_screens: String,
    /// The profiles threads may use; `None`: every profile.
    pub thread_profiles: Option<Vec<String>>,
    pub coordinator_profiles: Option<Vec<String>>,
}

impl Default for Safety {
    fn default() -> Self {
        Safety {
            start_threads: "propose".into(),
            coordinator_agent_args: Vec::new(),
            thread_agent_args: Vec::new(),
            yolo: false,
            trust_screens: crate::safety::USER.into(),
            thread_profiles: None,
            coordinator_profiles: None,
        }
    }
}

impl Safety {
    /// The launch arguments for an agent of `kind`: the user's own `args`,
    /// plus the kind's skip-permissions flag in yolo mode (once).
    pub fn launch_args(&self, kind: &str, args: &[String]) -> Vec<String> {
        let mut out = args.to_vec();
        if self.yolo {
            for flag in crate::safety::yolo_flags(kind).unwrap_or_default() {
                if !out.iter().any(|a| a == flag) {
                    out.push(flag.to_string());
                }
            }
        }
        out
    }
}

/// One `[safety.*]` table as written: absent keys fall through to the
/// all-projects `[safety.default]` table, then to the built-in defaults.
#[derive(Debug, Clone, Deserialize, Default, PartialEq)]
pub struct SafetyLayer {
    pub start_threads: Option<String>,
    pub coordinator_agent_args: Option<Vec<String>>,
    pub thread_agent_args: Option<Vec<String>>,
    pub yolo: Option<bool>,
    pub trust_screens: Option<String>,
    pub thread_profiles: Option<Vec<String>>,
    pub coordinator_profiles: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub slug: String,
}

/// Held while reading and rewriting anything under `threads/`, `inbox/` or
/// `.state/`. Never held across a herdr, git, gh, ssh or scp call.
pub struct ProjectLock {
    _file: File,
}

impl Project {
    /// An existing project. Validates the slug before building any path.
    pub fn load(root: &Path, slug: &str) -> Result<Project> {
        validate_slug(slug)?;
        let project = Project {
            root: root.to_path_buf(),
            slug: slug.to_string(),
        };
        if !project.project_md().is_file() {
            bail!("no project `{slug}` in {}", root.display());
        }
        Ok(project)
    }

    pub fn dir(&self) -> PathBuf {
        self.root.join(&self.slug)
    }

    pub fn project_md(&self) -> PathBuf {
        self.dir().join("PROJECT.md")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.dir().join(".state")
    }

    /// The canonical folder (symlinks resolved): the key of the project's
    /// `[safety]` table and of its routine approvals.
    pub fn canonical_dir(&self) -> PathBuf {
        std::fs::canonicalize(self.dir()).unwrap_or_else(|_| self.dir())
    }

    /// The name this project shows wherever a person reads it: `name` from
    /// `PROJECT.md` when it has one, else its slug humanized (`herdr-projects`
    /// reads `Herdr Projects`). A `PROJECT.md` that cannot be read falls back to
    /// the humanized slug, so a Space label is never empty.
    pub fn display_name(&self) -> String {
        self.read_project_md()
            .map(|(settings, _)| display_name(&settings.name, &self.slug))
            .unwrap_or_else(|_| humanize(&self.slug))
    }

    /// Takes the per-project lock. The lock file is opened without creating
    /// parent directories, and the project is re-checked afterwards, so a
    /// `delete` that lands mid-operation cannot be resurrected by a writer.
    pub fn lock(&self) -> Result<ProjectLock> {
        let path = self.state_dir().join("lock");
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("project `{}` is gone ({})", self.slug, path.display()))?;
        file.lock()?;
        if !self.project_md().is_file() {
            bail!("project `{}` is gone", self.slug);
        }
        Ok(ProjectLock { _file: file })
    }

    pub fn read_project_md(&self) -> Result<(Settings, String)> {
        let text = std::fs::read_to_string(self.project_md())
            .with_context(|| format!("could not read {}", self.project_md().display()))?;
        parse_project_md(&text)
    }

    /// The slugs this project had before, oldest first.
    pub fn former_slugs(&self) -> Vec<String> {
        read_json::<ProjectState>(&self.state_dir().join("project.json"))
            .unwrap_or_default()
            .former_slugs
    }

    /// Records `slug` as a former slug (once).
    pub fn add_former_slug(&self, slug: &str) -> Result<()> {
        let _lock = self.lock()?;
        let path = self.state_dir().join("project.json");
        let mut state = read_json::<ProjectState>(&path).unwrap_or_default();
        if !state.former_slugs.iter().any(|s| s == slug) {
            state.former_slugs.push(slug.to_string());
        }
        write_json(&path, &state)
    }

    /// The branch prefixes of this project's threads: `hp/<slug>/` for the
    /// slug and for each former one.
    pub fn branch_prefixes(&self) -> Vec<String> {
        std::iter::once(self.slug.clone()).chain(self.former_slugs()).map(|s| format!("hp/{s}/")).collect()
    }

    /// The project's coordinator record.
    ///
    /// A project with a lane registry answers from its **default lane**
    /// (`docs/lanes.md` §10): `coordinator.json` becomes the lane's view, and
    /// the sixty-odd call sites of this function keep working unchanged.
    /// A project with no `lanes.json` — every project until `open` learns to
    /// write one — reads the file itself, byte for byte as before. Nothing
    /// here creates a registry.
    pub fn coordinator(&self) -> Option<Coordinator> {
        if crate::lanes::exists(self) {
            return crate::lanes::Registry::for_project(self).default_coordinator(self);
        }
        read_json(&self.coordinator_path())
    }

    pub fn coordinator_path(&self) -> PathBuf {
        self.state_dir().join(crate::lanes::COORDINATOR_FILE)
    }

    /// Read-modify-write of `coordinator.json` under the lock: re-reads the
    /// file, lets `change` touch only the fields its step owns, writes.
    ///
    /// It reads and writes the **file**, never [`Project::coordinator`], so a
    /// lane registry cannot feed a synthesized record back into the record it
    /// was synthesized from. Writing `coordinator.json` stays this function's
    /// job alone; the double write to `lanes.json` belongs to `open` (§12,
    /// commit 4).
    pub fn update_coordinator(&self, change: impl FnOnce(&mut Coordinator)) -> Result<Coordinator> {
        let _lock = self.lock()?;
        self.update_coordinator_locked(change)
    }

    /// The same write, for a caller that already holds `project.lock()`.
    ///
    /// [`Project::lock`] opens the lock file each time, so a lock taken again in
    /// the same process waits on the one already held and never returns: `open`
    /// holds the lock from making its Space to the record that names it
    /// (2026-11-21), and every write inside that section comes through here.
    pub fn update_coordinator_locked(&self, change: impl FnOnce(&mut Coordinator)) -> Result<Coordinator> {
        let mut record = crate::lanes::read_coordinator(self).unwrap_or_default();
        change(&mut record);
        record.updated = now();
        write_json(&self.coordinator_path(), &record)?;
        Ok(record)
    }

    pub fn safety(&self, config_dir: &Path) -> Result<Safety> {
        load_safety(config_dir, &self.canonical_dir())
    }
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    write_atomic(path, text.as_bytes())
}

/// The effective safety settings: `[safety."<canonical project path>"]` in
/// `<config_dir>/config.toml`, then `[safety.default]` for keys it leaves
/// out, then the built-in defaults. Yolo mode forces `start_threads = "auto"`.
pub fn load_safety(config_dir: &Path, canonical_project_dir: &Path) -> Result<Safety> {
    let (default, own) = load_safety_layers(config_dir, canonical_project_dir)?;
    let base = Safety::default();
    let yolo = own.yolo.or(default.yolo).unwrap_or(base.yolo);
    Ok(Safety {
        start_threads: if yolo { "auto".into() } else { own.start_threads.or(default.start_threads).unwrap_or(base.start_threads) },
        coordinator_agent_args: own.coordinator_agent_args.or(default.coordinator_agent_args).unwrap_or_default(),
        thread_agent_args: own.thread_agent_args.or(default.thread_agent_args).unwrap_or_default(),
        trust_screens: own.trust_screens.or(default.trust_screens).unwrap_or_else(|| default_trust_screens(yolo).into()),
        yolo,
        thread_profiles: own.thread_profiles.or(default.thread_profiles),
        coordinator_profiles: own.coordinator_profiles.or(default.coordinator_profiles),
    })
}

/// Who answers trust screens when `trust_screens` is not set: the
/// coordinator in yolo mode, the user otherwise.
pub fn default_trust_screens(yolo: bool) -> &'static str {
    if yolo { crate::safety::COORDINATOR } else { crate::safety::USER }
}

/// The `[safety.default]` table and the project's own table, as written.
/// `canonical_project_dir` empty reads only the default table.
pub fn load_safety_layers(config_dir: &Path, canonical_project_dir: &Path) -> Result<(SafetyLayer, SafetyLayer)> {
    let file = config_dir.join("config.toml");
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Ok(Default::default());
    };
    load_safety_layers_from(&text, &file.display().to_string(), canonical_project_dir)
}

/// [`load_safety_layers`] on config.toml's `text`; `file` names it in errors.
pub fn load_safety_layers_from(text: &str, file: &str, canonical_project_dir: &Path) -> Result<(SafetyLayer, SafetyLayer)> {
    #[derive(Deserialize, Default)]
    struct Config {
        #[serde(default)]
        safety: std::collections::BTreeMap<String, SafetyLayer>,
    }
    let mut config: Config = toml::from_str(text).with_context(|| format!("{file} does not parse"))?;
    let default = config.safety.remove(crate::safety::DEFAULT_TABLE).unwrap_or_default();
    let own = config
        .safety
        .remove(&*canonical_project_dir.to_string_lossy())
        .unwrap_or_default();
    for layer in [&default, &own] {
        if let Some(value) = &layer.start_threads
            && !matches!(value.as_str(), "propose" | "auto")
        {
            bail!("{file}: start_threads must be \"propose\" or \"auto\", not {value:?}");
        }
        if let Some(value) = &layer.trust_screens
            && !matches!(value.as_str(), "coordinator" | "user")
        {
            bail!("{file}: trust_screens must be \"coordinator\" or \"user\", not {value:?}");
        }
    }
    Ok((default, own))
}

/// Slugs of the projects in `root`: folders that contain `PROJECT.md`. Entries
/// whose names start with a dot are ignored. A missing root has no projects.
pub fn list_slugs(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut slugs: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.') && validate_slug(name).is_ok())
        .filter(|name| root.join(name).join("PROJECT.md").is_file())
        .collect();
    slugs.sort();
    slugs
}

/// The skeleton `hp new` writes and `hp task add` keeps: `## Backlog` with no
/// task is a list, not litter, so no writer removes it unless asked.
pub(crate) const TASKS_TEMPLATE: &str = "# Tasks\n\n## Backlog\n";

const INSTRUCTIONS_TEMPLATE: &str = "\
# Instructions

Standing instructions for this project. Every thread starts from this text and
from the project's memory. Replace this paragraph with how you want work done:
conventions, what to check before finishing, what never to do. Ask the
coordinator to change it, or edit it here.

The settings above, between the `+++` lines, are changed with `hp set` or by
asking the coordinator.
";

/// The folders every project has. `uploads/` is yours (files for threads),
/// `library/` holds what threads produced.
pub const SUBDIRS: [&str; 9] = ["memory", "scratch", "routines", "threads", "inbox", "inbox/done", "library", "uploads", ".state"];

/// The text of `AGENTS.md`. Harnesses load it from every ancestor of their
/// working directory, and tab threads run under `threads/<id>/`, so it says
/// who is who by working directory. `prefix` is `<absolute binary> --root
/// <root>`: bare `hp` is on no harness's `PATH`.
pub fn agents_md(name: &str, slug: &str, prefix: &str) -> String {
    format!(
        "# {name}\n\n\
         This folder is the home of the project \"{name}\" (`{slug}`). Written by hp; `doctor --fix` refreshes it.\n\n\
         If your working directory is exactly this folder, you are the coordinator of {name}: run `{prefix} skill` now and follow what it prints, and run `{prefix} context {slug}` now and whenever you need project state.\n\n\
         If your working directory is under `threads/`, you are a thread: your brief is in your own folder (`.herdr-project/{slug}-<id>/brief.md`); ignore the rest of this file.\n"
    )
}

/// The command prefix `AGENTS.md` carries, so `doctor` can check that its
/// binary still exists.
pub fn prefix_in_agents_md(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.starts_with("If your working directory is exactly this folder"))?;
    let start = line.find('`')? + 1;
    let rest = &line[start..];
    let end = rest.find(" skill`")?;
    Some(rest[..end].to_string())
}

pub const PR_FOLLOWUP: &str = "routines/pr-followup.md";

const PR_FOLLOWUP_TEMPLATE: &str = "+++\non = \"pr\"\nevents = [\"checks-failed\", \"review\"]\nenabled = true\n+++\n\nFix the failing checks and address the new review comments on your pull request. Read them with `gh`, push the fixes, reply where a reviewer asked something, and then rewrite your report. If a comment asks for something outside your task, say so in the report instead of doing it.\n";

pub const COORDINATOR_SWEEP: &str = "routines/coordinator-sweep.md";

/// The periodic coordinator sweep: list ready-for-review threads and check
/// their reports were handled, close seen-but-not-done inbox items, read the
/// last line of every idle thread's pane, and cross-check the registry against
/// herdr (`thread audit`). Its text started as law's own file; the audit step is
/// the one addition, and a project whose file already exists keeps its own text
/// (nothing here overwrites it).
const COORDINATOR_SWEEP_TEMPLATE: &str = "+++\nschedule = \"every 20m\"\nenabled = true\n+++\n\n例行巡检并把账对平，只做核对与推进，不改代码：\n\n1. 列出所有 ready-for-review 的线程，逐个确认其报告已被处置：已 review、已合并或明确记录等待原因；报告标 Done 但未收口的，按既定验收流程推进（需要用户确认就先问）。\n2. inbox 里 seen 未 done 的项逐条了结，不允许长期滞留。\n3. 对每个 idle 线程的 pane 最后一条输出扫一眼：有口头交接或完成信息但没落进 threads/*.md 报告的，先让该线程补写报告，再按报告处置。\n4. 跑一次 `hp thread audit <slug>`（只读）：resolved-but-running 说明有 agent 活过了它的线程，working-but-stale 说明有 agent 声称在工作却很久没写任何东西；把看到的如实报给用户，要不要杀掉那个进程由用户决定，不要自己动手。\n\n上次巡检结论仍然成立且无变化的项，简要记一句即可，不要重复展开。\n";

/// Every ready-made routine, as `(file, text)`: `pr-followup` (on a pull
/// request) and `coordinator-sweep` (every twenty minutes). Both are enabled by
/// default; `hp routine toggle` or the coordinator turns one off.
const DEFAULT_ROUTINES: [(&str, &str); 2] = [(PR_FOLLOWUP, PR_FOLLOWUP_TEMPLATE), (COORDINATOR_SWEEP, COORDINATOR_SWEEP_TEMPLATE)];

/// Writes the ready-made routines a project is missing, recording each one in
/// `.state/routine-journal.log`. Written by `create` and by `open` or
/// `doctor --fix`; a file that already exists is never touched, so text the
/// user edited survives every later `open` and `doctor --fix`. `source` is the
/// command that got here.
pub fn write_default_routine(project: &Project, source: &str) -> Result<bool> {
    let _lock = project.lock()?;
    write_default_routine_locked(project, source)
}

/// The write itself. `create` runs it before `PROJECT.md` exists, when there is
/// no project to lock yet and no other command can see the folder.
fn write_default_routine_locked(project: &Project, source: &str) -> Result<bool> {
    let mut wrote = false;
    for (file, text) in DEFAULT_ROUTINES {
        let path = project.dir().join(file);
        if path.exists() {
            continue;
        }
        write_atomic(&path, text.as_bytes())?;
        let name = file.trim_start_matches("routines/").trim_end_matches(".md");
        let enabled = crate::routine::parse(name, text).map(|r| r.enabled).unwrap_or(true);
        crate::routine::journal_locked_or_warn(project, "create", name, if enabled { "enabled" } else { "disabled" }, source);
        wrote = true;
    }
    Ok(wrote)
}

/// Writes `AGENTS.md`, `CLAUDE.md` (a relative symbolic link to it) and
/// creates `uploads/`. Idempotent; used by `open` and by `doctor --fix`.
pub fn write_priming(project: &Project, prefix: &str, source: &str) -> Result<()> {
    let dir = project.dir();
    let (settings, _) = project.read_project_md()?;
    let name = display_name(&settings.name, &project.slug);
    let agents = dir.join("AGENTS.md");
    if let Ok(existing) = std::fs::read_to_string(&agents)
        && !existing.contains("Written by hp") && !existing.contains("Written by herdr-projects")
    {
        // Someone else's AGENTS.md: keep its text beside ours, once.
        let kept = dir.join("AGENTS.md.before-herdr-projects");
        if !kept.exists() {
            std::fs::rename(&agents, &kept)?;
        }
    }
    write_atomic(&agents, agents_md(&name, &project.slug, prefix).as_bytes())?;
    let claude = dir.join("CLAUDE.md");
    let link_ok = std::fs::read_link(&claude).is_ok_and(|target| target == Path::new("AGENTS.md"));
    if !link_ok {
        if std::fs::symlink_metadata(&claude).is_ok() {
            // A regular file or a link elsewhere: keep its text beside it, once.
            let kept = dir.join("CLAUDE.md.before-herdr-projects");
            if !kept.exists() {
                std::fs::rename(&claude, &kept)?;
            } else {
                std::fs::remove_file(&claude)?;
            }
        }
        std::os::unix::fs::symlink("AGENTS.md", &claude).with_context(|| format!("could not link {}", claude.display()))?;
    }
    if !dir.join("uploads").is_dir() {
        std::fs::create_dir(dir.join("uploads"))?;
    }
    write_default_routine(project, source)?;
    Ok(())
}

/// What `doctor` finds wrong with a project's priming files, as short notes.
pub fn priming_problems(project: &Project, prefix: &str) -> Vec<String> {
    let dir = project.dir();
    let mut problems = Vec::new();
    match std::fs::read_to_string(dir.join("AGENTS.md")) {
        Err(_) => problems.push("AGENTS.md is missing".into()),
        Ok(text) => match prefix_in_agents_md(&text) {
            None => problems.push("AGENTS.md does not name the binary".into()),
            Some(found) => {
                let binary = found.split(" --root ").next().unwrap_or("").trim_matches('\'');
                if !Path::new(binary).is_file() {
                    problems.push(format!("AGENTS.md points at a binary that does not exist ({binary})"));
                } else if found != prefix {
                    problems.push("AGENTS.md names another binary or root than this one".into());
                }
            }
        },
    }
    if !std::fs::read_link(dir.join("CLAUDE.md")).is_ok_and(|t| t == Path::new("AGENTS.md")) {
        problems.push("CLAUDE.md is not a link to AGENTS.md".into());
    }
    if !dir.join("uploads").is_dir() {
        problems.push("uploads/ is missing".into());
    }
    for (file, _) in DEFAULT_ROUTINES {
        if !dir.join(file).exists() {
            problems.push(format!("{file} is missing"));
        }
    }
    problems
}

/// Creates the folder and skeleton files. The only code path that creates a
/// project's directories. Fails if the slug exists.
pub fn create(root: &Path, name: &str, repos: Vec<Repo>) -> Result<Project> {
    let slug = slug_from_name(name)?;
    let project = Project {
        root: root.to_path_buf(),
        slug: slug.clone(),
    };
    let dir = project.dir();
    if dir.exists() {
        bail!("`{slug}` already exists in {}", root.display());
    }
    let repos = repos
        .into_iter()
        .map(|repo| Repo {
            path: std::fs::canonicalize(&repo.path)
                .or_else(|_| std::path::absolute(&repo.path))
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(repo.path),
            ..Repo::default()
        })
        .collect();
    let settings = Settings {
        name: display_name(name, &slug),
        repos,
        ..Settings::default()
    };
    let front = toml::to_string(&settings)?;

    std::fs::create_dir_all(root)?;
    std::fs::create_dir(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    for sub in SUBDIRS {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    write_atomic(
        &dir.join("MEMORY.md"),
        b"# Memory\n\nOne line per memory file: `- [title](memory/file.md): what it holds`.\n",
    )?;
    write_atomic(&dir.join("TASKS.md"), TASKS_TEMPLATE.as_bytes())?;
    write_default_routine_locked(&project, "new")?;
    write_json(&project.state_dir().join("project.json"), &ProjectState::default())?;
    // PROJECT.md last: a folder without it is not a project, so a half-made
    // skeleton is never picked up by `list` or the ticker.
    write_atomic(
        &project.project_md(),
        format!("+++\n{front}+++\n\n{INSTRUCTIONS_TEMPLATE}").as_bytes(),
    )?;
    Ok(project)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `[[repos]]` entry as `new` writes it: a path and no `base` of its own.
    fn repo(path: &str) -> Repo {
        Repo { path: path.to_string(), ..Repo::default() }
    }

    /// The three shapes `coordinator_turn_limit` takes, and the fourth: a value
    /// nobody can read. A bad one must fail the PROJECT.md parse (the ticker
    /// then writes one `config-error` item, like every other unusable setting)
    /// rather than panic or silently fall back to ten minutes.
    #[test]
    fn the_coordinator_turn_limit_reads_minutes_off_and_nothing_else() {
        assert_eq!(TurnLimit::default(), TurnLimit::Minutes(10));
        let read = |text: &str| parse_project_md(&format!("+++\ncoordinator_turn_limit = \"{text}\"\n+++\n\nBody\n")).map(|(s, _)| s.coordinator_turn_limit);
        assert_eq!(read("20m").unwrap(), TurnLimit::Minutes(20));
        assert_eq!(read("5m").unwrap(), TurnLimit::Minutes(5));
        assert_eq!(read("off").unwrap(), TurnLimit::Off);
        assert_eq!(read("OFF").unwrap(), TurnLimit::Off, "case is not a way to get it wrong");
        for bad in ["abc", "20", "0m", "-5m", "m", ""] {
            let error = format!("{:#}", read(bad).unwrap_err());
            assert!(error.contains("coordinator_turn_limit"), "`{bad}` must say which setting failed: {error}");
        }
        // A file without the key keeps the default; writing it back keeps the
        // value in the same `20m` shape a person typed.
        let (settings, _) = parse_project_md("+++\nname = \"X\"\n+++\n").unwrap();
        assert_eq!(settings.coordinator_turn_limit, TurnLimit::Minutes(10));
        assert_eq!(TurnLimit::Minutes(5).text(), "5m");
        assert_eq!(TurnLimit::Off.text(), "off");
        assert_eq!(TurnLimit::Minutes(10).secs(), Some(600));
        assert_eq!(TurnLimit::Off.secs(), None);
    }

    /// An older `hp` wrote the lifecycle state into `.state/project.json`. The
    /// `status` key is gone from the struct, so the file must still parse:
    /// `#[serde(default)]` ignores the stale key, and the former slugs it sits
    /// beside are what a rename reads.
    #[test]
    fn an_old_status_key_does_not_take_the_former_slugs_with_it() {
        let root = tempfile::tempdir().unwrap();
        let project = create(root.path(), "Demo", vec![]).unwrap();
        std::fs::write(project.state_dir().join("project.json"), "{\"status\":\"archived\",\"former_slugs\":[\"old-demo\"]}").unwrap();
        assert_eq!(project.former_slugs(), ["old-demo"]);
    }

    /// A `PROJECT.md` from an older `hp` carries `status = "..."` in its front
    /// matter. It must parse, and be nothing but an unknown line.
    #[test]
    fn an_old_status_line_in_project_md_parses() {
        let text = "+++\nname = \"Demo\"\nstatus = \"paused\"\n+++\n\n# Instructions\n";
        let (settings, body) = parse_project_md(text).unwrap();
        assert_eq!(settings.name, "Demo");
        assert_eq!(body, "# Instructions\n");
    }

    #[test]
    fn only_folders_with_project_md_count() {
        let root = tempfile::tempdir().unwrap();
        for name in ["b", "a", ".trash", "empty", "Not_A_Slug"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        for name in ["b", "a", ".trash", "Not_A_Slug"] {
            std::fs::write(root.path().join(name).join("PROJECT.md"), "").unwrap();
        }
        assert_eq!(list_slugs(root.path()), ["a", "b"]);
        assert!(list_slugs(&root.path().join("missing")).is_empty());
    }

    #[test]
    fn slug_validation() {
        for good in ["a", "demo", "demo-2", "0x", &"a".repeat(40)] {
            assert!(validate_slug(good).is_ok(), "{good}");
        }
        for bad in ["", "-a", "A", "a_b", "a/b", "../x", "a b", ".", "..", &"a".repeat(41)] {
            assert!(validate_slug(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_slug_like_name_is_humanized_and_a_typed_name_is_kept() {
        assert_eq!(humanize("herdr-projects"), "Herdr Projects");
        assert_eq!(humanize("gtm_ai"), "Gtm Ai");
        assert_eq!(humanize("-v2--api-"), "V2 Api");
        assert_eq!(display_name("herdr-projects", "herdr-projects"), "Herdr Projects");
        assert_eq!(display_name("", "herdr-projects"), "Herdr Projects");
        assert_eq!(display_name("  ", "demo"), "Demo");
        for typed in ["GTM AI", "my project", "Demo", "herdr-Projects"] {
            assert_eq!(display_name(typed, "x"), typed);
        }
    }

    #[test]
    fn create_stores_a_display_name_and_keeps_the_slug() {
        let root = tempfile::tempdir().unwrap();
        let project = create(root.path(), "herdr-projects", vec![]).unwrap();
        assert_eq!(project.slug, "herdr-projects");
        assert_eq!(project.read_project_md().unwrap().0.name, "Herdr Projects");
        let project = create(root.path(), "GTM AI", vec![]).unwrap();
        assert_eq!(project.slug, "gtm-ai");
        assert_eq!(project.read_project_md().unwrap().0.name, "GTM AI");
    }

    #[test]
    fn a_project_shows_the_name_it_has_and_else_the_humanized_slug() {
        let root = tempfile::tempdir().unwrap();
        let project = create(root.path(), "herdr-projects", vec![]).unwrap();
        assert_eq!(project.display_name(), "Herdr Projects", "the name `new` wrote into PROJECT.md");
        // A PROJECT.md with no `name`: an older hp's (it has a `goal` instead,
        // which no version reads any more), or one a person trimmed.
        write_atomic(&project.project_md(), b"+++\ngoal = \"Old goal\"\n+++\n# Instructions\n").unwrap();
        assert_eq!(project.display_name(), "Herdr Projects", "no name line: the slug, humanized");
        write_atomic(&project.project_md(), b"+++\nname = \"REXA\"\n+++\n").unwrap();
        assert_eq!(project.display_name(), "REXA", "a typed name is used as it is");
    }

    #[test]
    fn slug_derivation_and_name_refusals() {
        assert_eq!(slug_from_name("My Demo  Project!").unwrap(), "my-demo-project");
        assert_eq!(slug_from_name("  Ünï 42 ").unwrap(), "n-42");
        assert_eq!(slug_from_name(&"x".repeat(60)).unwrap().len(), 40);
        for bad in ["../x", "a/b", "a\\b", "..", "!!!", ""] {
            assert!(slug_from_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn create_writes_the_skeleton_and_refuses_a_second_time() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().join("root");
        let project = create(&root, "Demo", vec![repo("/srv/app"), repo("/no/such/repo")]).unwrap();
        assert_eq!(project.slug, "demo");
        for sub in ["memory", "scratch", "routines", "threads", "inbox/done", "library", "uploads", ".state"] {
            assert!(project.dir().join(sub).is_dir(), "{sub}");
        }
        assert!(project.dir().join("MEMORY.md").is_file());
        assert!(project.dir().join("TASKS.md").is_file());
        let (settings, body) = project.read_project_md().unwrap();
        assert_eq!(settings.name, "Demo");
        assert_eq!(settings.coordinator_profile, "claude");
        assert_eq!(settings.auto_resolve_days, 7);
        assert!(settings.nudge);
        assert_eq!(settings.coordinator_turn_limit, TurnLimit::Minutes(10), "ten minutes unless PROJECT.md says otherwise");
        assert_eq!(settings.base, "", "`new` names no base: the remote's default branch, as before");
        let front = std::fs::read_to_string(project.project_md()).unwrap();
        assert!(!front.contains("base = "), "the key is not written at all, not written empty: {front}");
        assert!(project.dir().join(PR_FOLLOWUP).is_file());
        assert!(project.dir().join(COORDINATOR_SWEEP).is_file());
        assert!(crate::routine::load_all(&project).1.is_empty(), "the default routines parse");
        assert_eq!(settings.repos, vec![repo("/srv/app"), repo("/no/such/repo")]);
        assert!(body.starts_with("# Instructions"));
        assert!(create(&root, "demo", vec![]).is_err());
    }

    #[test]
    fn new_writes_both_default_routines_and_an_edited_one_is_never_rewritten() {
        let root = tempfile::tempdir().unwrap();
        let project = create(root.path(), "Demo", vec![]).unwrap();
        let prefix = format!("{} --root {}", std::env::current_exe().unwrap().display(), root.path().display());

        // `new` writes both defaults, and the sweep is the file law already has.
        let sweep = std::fs::read_to_string(project.dir().join(COORDINATOR_SWEEP)).unwrap();
        assert_eq!(sweep, COORDINATOR_SWEEP_TEMPLATE);
        assert!(sweep.starts_with("+++\nschedule = \"every 20m\"\nenabled = true\n+++\n\n"), "{sweep}");
        let (routines, broken) = crate::routine::load_all(&project);
        assert!(broken.is_empty(), "{broken:?}");
        let parsed = routines.iter().find(|r| r.name == "coordinator-sweep").expect("the sweep parses as a routine");
        assert!(parsed.enabled, "an omitted `enabled` is not what makes it run: it says so");
        assert_eq!(parsed.schedule_text, "every 20m");
        assert_eq!(parsed.trigger, crate::routine::Trigger::Schedule(crate::routine::Schedule::Every(1200)));
        assert!(parsed.command.is_empty(), "the sweep is a prompt, not a command");
        assert!(parsed.prompt.starts_with("例行巡检并把账对平"), "{}", parsed.prompt);

        // A second `write_priming` (any `open`, `doctor --fix`) leaves text the
        // user edited alone, and reports nothing missing.
        std::fs::write(project.dir().join(COORDINATOR_SWEEP), "+++\nschedule = \"daily 09:00\"\n+++\n\nMine.\n").unwrap();
        write_priming(&project, &prefix, "open").unwrap();
        assert_eq!(std::fs::read_to_string(project.dir().join(COORDINATOR_SWEEP)).unwrap(), "+++\nschedule = \"daily 09:00\"\n+++\n\nMine.\n");
        assert!(priming_problems(&project, &prefix).is_empty());

        // A deleted sweep is reported by name, and `--fix`'s writer puts it back.
        std::fs::remove_file(project.dir().join(COORDINATOR_SWEEP)).unwrap();
        let problems = priming_problems(&project, &prefix);
        assert_eq!(problems, ["routines/coordinator-sweep.md is missing"], "{problems:?}");
        write_priming(&project, &prefix, "open").unwrap();
        assert!(project.dir().join(COORDINATOR_SWEEP).is_file());
        assert!(priming_problems(&project, &prefix).is_empty());
    }

    #[test]
    fn new_records_both_default_routines_as_create_events() {
        let root = tempfile::tempdir().unwrap();
        let project = create(root.path(), "Demo", vec![]).unwrap();
        let journal = std::fs::read_to_string(crate::routine::journal_path(&project)).unwrap();
        let lines: Vec<&str> = journal.lines().collect();
        assert_eq!(lines.len(), 2, "{journal}");
        for (line, name) in lines.iter().zip(["pr-followup", "coordinator-sweep"]) {
            let mut parts = line.split("  ");
            assert_eq!(parts.next().unwrap().len(), 20, "{line}");
            assert_eq!((parts.next(), parts.next(), parts.next(), parts.next()), (Some("create"), Some(name), Some("enabled"), Some("source=new")), "{line}");
            let rest: Vec<&str> = parts.collect();
            assert!(rest[0].starts_with("binary="), "{line}");
            assert_eq!(rest[1], format!("root={}", project.root.display()), "{line}");
        }
        // A later pass finds both files and records nothing.
        assert!(!write_default_routine(&project, "open").unwrap());
        assert_eq!(crate::routine::journal_lines(&project), Some(2));
    }

    #[test]
    fn priming_files_are_written_linked_and_checked() {
        let root = tempfile::tempdir().unwrap();
        let project = create(root.path(), "Demo Project", vec![]).unwrap();
        let prefix = format!("{} --root {}", std::env::current_exe().unwrap().display(), root.path().display());
        write_priming(&project, &prefix, "open").unwrap();
        let text = std::fs::read_to_string(project.dir().join("AGENTS.md")).unwrap();
        assert!(text.contains("you are the coordinator of Demo Project"));
        assert!(text.contains(&format!("`{prefix} skill`")));
        assert!(text.contains(&format!("`{prefix} context demo-project`")));
        assert!(text.contains("under `threads/`, you are a thread"));
        assert_eq!(prefix_in_agents_md(&text).as_deref(), Some(prefix.as_str()));
        assert_eq!(std::fs::read_link(project.dir().join("CLAUDE.md")).unwrap(), Path::new("AGENTS.md"));
        assert!(project.dir().join("uploads").is_dir());
        assert!(priming_problems(&project, &prefix).is_empty());

        // Idempotent, and a stale binary path is reported.
        write_priming(&project, &prefix, "open").unwrap();
        let stale = agents_md("Demo Project", "demo-project", "/no/such/binary --root /r");
        std::fs::write(project.dir().join("AGENTS.md"), stale).unwrap();
        let problems = priming_problems(&project, &prefix);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("does not exist"));
        write_priming(&project, &prefix, "open").unwrap();
        assert!(priming_problems(&project, &prefix).is_empty());

        // A foreign AGENTS.md is kept beside ours.
        std::fs::write(project.dir().join("AGENTS.md"), "codex notes").unwrap();
        write_priming(&project, &prefix, "open").unwrap();
        assert_eq!(std::fs::read_to_string(project.dir().join("AGENTS.md.before-herdr-projects")).unwrap(), "codex notes");
        // A hand-written CLAUDE.md is kept beside the link, not lost.
        std::fs::remove_file(project.dir().join("CLAUDE.md")).unwrap();
        std::fs::write(project.dir().join("CLAUDE.md"), "mine").unwrap();
        assert!(priming_problems(&project, &prefix).iter().any(|p| p.contains("CLAUDE.md")));
        write_priming(&project, &prefix, "open").unwrap();
        assert_eq!(std::fs::read_to_string(project.dir().join("CLAUDE.md.before-herdr-projects")).unwrap(), "mine");
        assert!(priming_problems(&project, &prefix).is_empty());
    }

    #[test]
    fn a_project_file_that_still_has_a_goal_reads_and_the_goal_is_dropped() {
        // Every project made before `goal` was removed carries the line. It must
        // read as an ordinary project, and the value must not reach anything.
        let text = "+++\nname = \"Old\"\ngoal = \"Ship it\"\nnudge = true\n+++\n\n# Instructions\nBody\n";
        let (settings, body) = parse_project_md(text).unwrap();
        assert_eq!((settings.name.as_str(), settings.nudge), ("Old", true));
        assert_eq!(body, "# Instructions\nBody\n");
    }

    #[test]
    fn front_matter_parsing() {
        let (settings, body) =
            parse_project_md("+++\nname = \"X\"\nnudge = true\n+++\n\nBody\n+++\nmore\n").unwrap();
        assert_eq!(settings.name, "X");
        assert!(settings.nudge);
        assert_eq!(settings.thread_profile, "claude");
        assert_eq!(body, "Body\n+++\nmore\n");
        assert!(parse_project_md("no front matter").is_err());
        assert!(parse_project_md("+++\nname = \n+++\n").is_err());
        assert!(parse_project_md("+++\nname = \"X\"\n").is_err());
        let (_, body) = parse_project_md("+++\nname = \"X\"\n+++").unwrap();
        assert_eq!(body, "");
    }

    #[test]
    fn a_leftover_mute_line_is_ignored() {
        // Projects made before `mute` was removed still carry the line; it must
        // read as an ordinary project, not as an error.
        let text = "+++\nname = \"Old\"\nmute = true\nnudge = true\n+++\n\nBody\n";
        let (settings, body) = parse_project_md(text).unwrap();
        assert_eq!((settings.name.as_str(), settings.nudge, body.as_str()), ("Old", true, "Body\n"));
        // And a `set` on such a file leaves the stale line alone rather than
        // failing on it.
        let edited = crate::settings::set_in(text, "name", "Later").unwrap();
        let (settings, _) = parse_project_md(&edited).unwrap();
        assert_eq!((settings.name.as_str(), settings.nudge), ("Later", true));
    }

    #[test]
    fn a_project_file_left_with_a_machine_column_still_loads() {
        // Written by a version that could place a repo on another machine.
        let text = "+++\nname = \"Demo\"\n\n[[repos]]\npath = \"/srv/app\"\nmachine = \"box\"\n\n[[repos]]\npath = \"/home/me/lib\"\n+++\n\n# Instructions\nBody\n";
        let (settings, _) = parse_project_md(text).unwrap();
        assert_eq!(settings.repos, vec![repo("/srv/app"), repo("/home/me/lib")]);
    }

    #[test]
    fn safety_defaults_and_overrides_keyed_by_canonical_path() {
        let config = tempfile::tempdir().unwrap();
        let here = Path::new("/projects/demo");
        assert_eq!(load_safety(config.path(), here).unwrap(), Safety::default());

        std::fs::write(
            config.path().join("config.toml"),
            "root = \"/projects\"\n\n[safety.\"/projects/demo\"]\nstart_threads = \"auto\"\nthread_agent_args = [\"--x\"]\n",
        )
        .unwrap();
        let safety = load_safety(config.path(), here).unwrap();
        assert_eq!(safety.start_threads, "auto");
        assert_eq!(safety.thread_agent_args, ["--x"]);
        assert!(safety.thread_profiles.is_none());
        assert!(safety.coordinator_agent_args.is_empty());
        assert_eq!(
            load_safety(config.path(), Path::new("/projects/other")).unwrap(),
            Safety::default()
        );

        std::fs::write(
            config.path().join("config.toml"),
            "[safety.\"/projects/demo\"]\nstart_threads = \"yolo\"\n",
        )
        .unwrap();
        assert!(load_safety(config.path(), here).is_err());
    }

    #[test]
    fn the_default_table_fills_keys_a_project_leaves_out_and_yolo_starts_threads() {
        let config = tempfile::tempdir().unwrap();
        let here = Path::new("/projects/demo");
        std::fs::write(
            config.path().join("config.toml"),
            "[safety.default]\nyolo = true\nthread_agent_args = [\"--a\"]\n\n[safety.\"/projects/demo\"]\nthread_agent_args = []\nstart_threads = \"propose\"\n",
        )
        .unwrap();
        let safety = load_safety(config.path(), here).unwrap();
        assert!(safety.yolo, "inherited from the default table");
        assert_eq!(safety.start_threads, "auto", "yolo wins over propose");
        assert!(safety.thread_agent_args.is_empty(), "the project's own empty list wins");
        let other = load_safety(config.path(), Path::new("/projects/other")).unwrap();
        assert_eq!((other.yolo, other.thread_agent_args), (true, vec!["--a".to_string()]));

        let args = safety.launch_args("claude", &["--model".into(), "opus".into()]);
        assert_eq!(args, ["--model", "opus", "--dangerously-skip-permissions"]);
        assert_eq!(safety.launch_args("codex", &[]), ["--dangerously-bypass-approvals-and-sandbox"]);
        assert_eq!(safety.launch_args("claude", &["--dangerously-skip-permissions".into()]), ["--dangerously-skip-permissions"], "not twice");
        assert!(safety.launch_args("kiro", &[]).is_empty(), "no known flag: nothing added");
        assert!(Safety::default().launch_args("claude", &[]).is_empty(), "careful mode adds nothing");
    }

    #[test]
    fn writers_drop_their_write_when_project_md_is_gone() {
        let root = tempfile::tempdir().unwrap();
        let project = create(root.path(), "demo", vec![]).unwrap();
        std::fs::remove_file(project.project_md()).unwrap();
        assert!(project.update_coordinator(|c| c.pane_id = "w1:p1".into()).is_err());
        assert!(project.coordinator().is_none());

        // A deleted folder is not recreated by taking the lock.
        std::fs::remove_dir_all(project.dir()).unwrap();
        assert!(project.lock().is_err());
        assert!(!project.dir().exists());
    }

    /// A coordinator record is read by `socket`, the Space, tab, pane, folder
    /// and agent fields. `session` was written on every `open` and read by
    /// nothing: the socket already names the core session.
    #[test]
    fn a_coordinator_record_writes_only_the_fields_something_reads() {
        let root = tempfile::tempdir().unwrap();
        let project = create(root.path(), "demo", vec![]).unwrap();
        project.update_coordinator(|c| c.socket = "/s".into()).unwrap();
        let text = std::fs::read_to_string(project.state_dir().join("coordinator.json")).unwrap();
        assert!(!text.contains("\"session\""), "`session` had no reader and is gone: {text}");
        assert!(text.contains("\"socket\""), "{text}");
        // A file an older `hp` wrote still reads: the key is ignored.
        std::fs::write(project.state_dir().join("coordinator.json"), "{\"socket\":\"/s\",\"session\":\"main\"}").unwrap();
        let record = project.coordinator().unwrap();
        assert_eq!(record.socket, "/s");
    }

    #[test]
    fn coordinator_updates_keep_other_fields() {
        let root = tempfile::tempdir().unwrap();
        let project = create(root.path(), "demo", vec![]).unwrap();
        project.update_coordinator(|c| c.socket = "/s".into()).unwrap();
        project.update_coordinator(|c| c.agent_session = "sess".into()).unwrap();
        let record = project.coordinator().unwrap();
        assert_eq!(record.socket, "/s");
        assert_eq!(record.agent_session, "sess");
        assert!(std::fs::read_dir(project.state_dir())
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")));
    }
}
