//! Typed calls to herdr-core's `hc` CLI. Differences between core and the
//! Herdr 1 plugin API stay here.
//!
//! herdr-core has no workspaces, tabs, panes, agent registry or plugin API: it
//! runs programs in terminals, keeps them alive, takes labels and reads status
//! from OSC 7501. This module keeps the shapes the rest of the binary speaks
//! (a Space, a tab, a pane, an agent) and builds them out of terminals and
//! labels:
//!
//! | here | core |
//! | --- | --- |
//! | Space | every terminal labelled `hp_ws=<id>` |
//! | tab | every terminal labelled `hp_tab=<id>` |
//! | pane | one terminal, named `hp-<...>` (a name is stable across restarts) |
//! | agent | a terminal labelled `hp_agent=<name>`, `hp_kind=<kind>` |
//!
//! So a project's panes are whatever carries its labels, and losing them is
//! losing the Space — the same thing core means by a finished terminal.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::runner::{Cmd, Runner};

/// herdr-core 0.1.21 is the first release this binary was written against.
pub const MIN_VERSION: Version = Version(0, 1, 21);
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);
pub const AGENT_START_TIMEOUT: Duration = Duration::from_secs(20);
/// How long `agent_prompt_confirmed` lets core watch for the turn to start.
pub const PROMPT_WAIT: Duration = Duration::from_secs(8);

/// Label keys this binary owns. Nothing else writes them.
pub const L_WS: &str = "hp_ws";
pub const L_WS_LABEL: &str = "hp_ws_label";
pub const L_TAB: &str = "hp_tab";
pub const L_TAB_LABEL: &str = "hp_tab_label";
pub const L_AGENT: &str = "hp_agent";
pub const L_KIND: &str = "hp_kind";
pub const L_SESSION: &str = "hp_session";
pub const L_WORKTREE: &str = "hp_worktree";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Parses `hc 0.1.21` and `hc 0.1.22-preview.3`; a pre-release suffix is ignored.
pub fn parse_version(text: &str) -> Option<Version> {
    let token = text
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let core = token.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    Some(Version(parts.next()??, parts.next()??, parts.next()??))
}

/// Two recorded sessions are the same one, paths from Herdr 1 included.
pub fn same_session(a: &str, b: &str) -> bool {
    session_key(a) == session_key(b)
}

/// True when the named core answers. A session that is gone is not an error:
/// the caller decides whether to rebind, report or ignore it.
pub fn session_alive(bin: &str, session: &str, runner: &dyn Runner) -> bool {
    Herdr::new(bin, session, runner).reachable()
}

/// A herdr call that does not need a session (`--version`).
fn bare(bin: &str) -> Cmd {
    Cmd::new(bin, CALL_TIMEOUT)
}

pub fn version(bin: &str, runner: &dyn Runner) -> Result<Version> {
    let out = runner.run(&bare(bin).arg("--version"))?;
    if !out.success() {
        bail!("`{bin} --version` failed: {}", out.error_text());
    }
    parse_version(&out.stdout)
        .with_context(|| format!("could not read a version from `{}`", out.stdout.trim()))
}

/// A herdr call that failed. `code` is core's own error kind (for example
/// `blocked` or `not_found`), or `timeout` / `unreachable` / `failed` when core
/// never answered with one. The names Herdr 1 used for the same refusals are
/// kept where callers match on them.
#[derive(Debug, Clone, PartialEq)]
pub struct HerdrError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for HerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "herdr: {} ({})", self.message, self.code)
    }
}

impl std::error::Error for HerdrError {}

/// core refused the prompt before typing anything.
pub fn refused_before_typing(error: &HerdrError) -> bool {
    matches!(error.code.as_str(), "agent_blocked" | "pane_not_found" | "unreachable")
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Pane {
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub foreground_cwd: String,
    #[serde(default)]
    pub terminal_id: String,
    #[serde(default)]
    pub focused: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Workspace {
    pub workspace_id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub pane_count: usize,
    #[serde(default)]
    pub worktree: Option<WorkspaceWorktree>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct WorkspaceWorktree {
    #[serde(default)]
    pub repo_key: String,
    #[serde(default)]
    pub checkout_path: String,
    #[serde(default)]
    pub is_linked_worktree: bool,
}

/// The native session reference an agent reported (core: the `hp_session` label).
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct AgentSession {
    #[serde(default)]
    pub value: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct Agent {
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub agent_status: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub foreground_cwd: String,
    #[serde(default)]
    pub terminal_id: String,
    #[serde(default)]
    pub state_change_seq: u64,
    #[serde(default)]
    pub agent_session: Option<AgentSession>,
    #[serde(default)]
    pub interactive_ready: bool,
    #[serde(default)]
    pub tokens: std::collections::BTreeMap<String, String>,
}

impl Agent {
    /// The one "ready for a prompt" predicate: state `idle` or `done`.
    pub fn ready(&self) -> bool {
        ready_state(&self.agent_status)
    }

    pub fn ready_for_brief(&self, kind: &str) -> bool {
        self.ready() || (self.agent_status == "unknown" && self.interactive_ready && crate::prompt_box::knows(kind))
    }

    pub fn works_in(&self, dir: &str) -> bool {
        !dir.is_empty() && own_dir(&self.cwd, &self.foreground_cwd) == dir
    }

    /// Whether this agent runs in the core terminal `terminal` — the address
    /// `HC_TERMINAL_ID` names. A pane id is not it: a pane keeps the name it was
    /// given, while the terminal address is what core hands the processes in it.
    pub fn in_terminal(&self, terminal: &str) -> bool {
        !terminal.is_empty() && self.terminal_id == terminal
    }

    pub fn session_id(&self) -> &str {
        self.agent_session.as_ref().map(|s| s.value.as_str()).unwrap_or("")
    }
}

/// The directory a pane's agent works in.
pub fn own_dir<'a>(cwd: &'a str, foreground_cwd: &'a str) -> &'a str {
    if foreground_cwd.is_empty() { cwd } else { foreground_cwd }
}

/// The pane id of the terminal a command runs in: the agent's pane when one
/// occupies it, else the plain terminal's. Empty when the terminal is in neither
/// list — a command from another session, or about a terminal that is gone.
pub fn pane_of_terminal(terminal: &str, agents: &[Agent], panes: &[Pane]) -> String {
    if terminal.is_empty() {
        return String::new();
    }
    agents
        .iter()
        .find(|agent| agent.terminal_id == terminal)
        .map(|agent| agent.pane_id.clone())
        .or_else(|| panes.iter().find(|pane| pane.terminal_id == terminal).map(|pane| pane.pane_id.clone()))
        .unwrap_or_default()
}

pub fn ready_state(state: &str) -> bool {
    matches!(state, "idle" | "done")
}

#[derive(Debug, Clone, PartialEq)]
pub struct Created {
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
}

/// A terminal as core reports it, cut to what this binary reads.
#[derive(Debug, Clone, Deserialize, Default)]
struct Term {
    #[serde(default)]
    address: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    labels: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    last_cwd: Option<String>,
    #[serde(default)]
    status: Option<TermStatus>,
    #[serde(default)]
    turn: u64,
    #[serde(default)]
    state: String,
    #[serde(default)]
    reported: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct TermStatus {
    #[serde(default)]
    state: String,
}

impl Term {
    /// The pane id the rest of the binary stores: the terminal's name, which
    /// survives a core restart, else its address.
    fn pane_id(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.address.clone())
    }
    fn label(&self, key: &str) -> String {
        self.labels.get(key).cloned().unwrap_or_default()
    }
    fn live(&self) -> bool {
        self.state != "finished"
    }
    fn cwd(&self) -> String {
        if self.cwd.is_empty() { self.last_cwd.clone().unwrap_or_default() } else { self.cwd.clone() }
    }
}

/// `hc` bound to one core session. Every call a project makes goes through this,
/// so a project always talks to the session it was opened in.
pub struct Herdr<'a> {
    bin: String,
    /// The `HC_SESSION` this call runs in (empty: the default one). Herdr 1 put
    /// a socket path here; a path is read as the default session, so records
    /// written before the port keep working.
    session: String,
    runner: &'a dyn Runner,
}

/// A recorded socket path from Herdr 1 means "the default session" here.
pub fn session_key(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() || raw == "-" || raw.contains('/') || raw.ends_with(".sock") {
        String::new()
    } else {
        raw.to_string()
    }
}

impl<'a> Herdr<'a> {
    pub fn new(bin: impl Into<String>, session: impl std::fmt::Display, runner: &'a dyn Runner) -> Self {
        let raw = session.to_string();
        Herdr {
            bin: bin.into(),
            session: session_key(&raw),
            runner,
        }
    }

    /// The core this call reaches: `HC_SESSION` is either set to the session
    /// this project recorded or removed, so the caller's value never competes.
    pub fn cmd(&self, timeout: Duration) -> Cmd {
        let mut cmd = Cmd::new(&self.bin, timeout);
        cmd = if self.session.is_empty() {
            cmd.env_remove("HC_SESSION")
        } else {
            cmd.env("HC_SESSION", &self.session)
        };
        cmd
    }

    /// True when the core answers.
    pub fn reachable(&self) -> bool {
        self.runner
            .run(&self.cmd(CALL_TIMEOUT).args(["server", "status"]))
            .map(|out| out.success())
            .unwrap_or(false)
    }

    /// Runs one `hc` command and returns its JSON result.
    fn json(&self, args: &[&str], timeout: Duration) -> Result<serde_json::Value, HerdrError> {
        let cmd = self.cmd(timeout).arg("--json").args(args.iter().copied());
        let out = self.runner.run(&cmd).map_err(|e| HerdrError {
            code: "unreachable".into(),
            message: format!("{e:#}"),
        })?;
        if out.timed_out {
            return Err(HerdrError { code: "timeout".into(), message: format!("`hc {}` timed out", args.join(" ")) });
        }
        if out.success() {
            let text = out.stdout.trim();
            if text.is_empty() {
                return Ok(serde_json::Value::Null);
            }
            return serde_json::from_str(text).map_err(|e| HerdrError {
                code: "failed".into(),
                message: format!("`hc {}` reply did not parse: {e}", args.join(" ")),
            });
        }
        // `hc` prints one JSON object with `error` on stderr when asked for JSON.
        let body = serde_json::from_str::<serde_json::Value>(out.stderr.trim()).ok();
        if let Some(error) = body.as_ref().and_then(|b| b.get("error")) {
            let kind = error["kind"].as_str().unwrap_or("failed").to_string();
            let code = match kind.as_str() {
                // core says `blocked`: the program is waiting for a person.
                "blocked" => "agent_blocked".to_string(),
                "not_found" | "unknown_terminal" => "pane_not_found".to_string(),
                _ => kind,
            };
            let message = error["message"].as_str().unwrap_or("").to_string();
            let hint = error["hint"].as_str().unwrap_or("");
            let message = if hint.is_empty() { message } else { format!("{message} ({hint})") };
            return Err(HerdrError { code, message });
        }
        Err(HerdrError { code: "failed".into(), message: format!("`hc {}`: {}", args.join(" "), out.error_text()) })
    }

    /// Every live terminal of this session, with its labels: `hc ls` prints one
    /// short row per terminal and leaves the labels to `hc get`.
    fn terminals(&self) -> Result<Vec<Term>, HerdrError> {
        let result = self.json(&["ls"], CALL_TIMEOUT)?;
        let rows = result["terminals"].as_array().cloned().unwrap_or_default();
        let mut out = Vec::new();
        for row in rows {
            let id = row["name"]
                .as_str()
                .filter(|n| !n.is_empty())
                .or_else(|| row["address"].as_str())
                .unwrap_or_default()
                .to_string();
            if id.is_empty() {
                continue;
            }
            if let Ok(term) = self.term(&id)
                && term.live()
            {
                out.push(term);
            }
        }
        Ok(out)
    }

    fn term(&self, pane: &str) -> Result<Term, HerdrError> {
        let result = self.json(&["get", pane], CALL_TIMEOUT)?;
        serde_json::from_value(result).map_err(|e| HerdrError {
            code: "failed".into(),
            message: format!("`hc get {pane}` reply changed: {e}"),
        })
    }

    fn label(&self, pane: &str, pairs: &[(&str, &str)]) -> Result<(), HerdrError> {
        let args: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let mut all: Vec<&str> = vec!["label", pane];
        all.extend(args.iter().map(String::as_str));
        self.json(&all, CALL_TIMEOUT).map(|_| ())
    }

    /// Writes labels onto a terminal. Herdr 1's token reports are labels here:
    /// the same names (`hp_project`, `hp_lane`, `hp_rank`), no TTL.
    pub fn set_labels(&self, pane: &str, pairs: &[(String, String)]) -> Result<(), HerdrError> {
        if pairs.is_empty() || pane.is_empty() {
            return Ok(());
        }
        let refs: Vec<(&str, &str)> = pairs.iter().map(|(key, value)| (key.as_str(), value.as_str())).collect();
        self.label(pane, &refs)
    }

    /// Removes labels from a terminal, in one call.
    pub fn clear_labels(&self, pane: &str, keys: &[&str]) -> Result<(), HerdrError> {
        if keys.is_empty() || pane.is_empty() {
            return Ok(());
        }
        let pairs: Vec<(&str, &str)> = keys.iter().map(|key| (*key, "")).collect();
        self.label(pane, &pairs)
    }

    pub fn pane_list(&self) -> Result<Vec<Pane>, HerdrError> {
        Ok(self
            .terminals()?
            .into_iter()
            .map(|t| Pane {
                pane_id: t.pane_id(),
                tab_id: t.label(L_TAB),
                workspace_id: t.label(L_WS),
                cwd: t.cwd(),
                foreground_cwd: t.cwd(),
                terminal_id: t.address.clone(),
                focused: false,
            })
            .collect())
    }

    pub fn agent_list(&self) -> Result<Vec<Agent>, HerdrError> {
        Ok(self
            .terminals()?
            .into_iter()
            .filter(|t| !t.label(L_AGENT).is_empty())
            .map(|t| {
                let status = t.status.clone().unwrap_or_default();
                Agent {
                    pane_id: t.pane_id(),
                    tab_id: t.label(L_TAB),
                    workspace_id: t.label(L_WS),
                    name: t.label(L_AGENT),
                    agent: t.label(L_KIND),
                    agent_status: if t.reported { status.state.clone() } else { String::new() },
                    cwd: t.cwd(),
                    foreground_cwd: t.cwd(),
                    terminal_id: t.address.clone(),
                    state_change_seq: t.turn,
                    agent_session: {
                        let value = t.label(L_SESSION);
                        (!value.is_empty()).then_some(AgentSession { value })
                    },
                    interactive_ready: t.reported,
                    tokens: t.labels.clone(),
                }
            })
            .collect())
    }

    pub fn workspace_list(&self) -> Result<Vec<Workspace>, HerdrError> {
        let mut out: Vec<Workspace> = Vec::new();
        for term in self.terminals()? {
            let id = term.label(L_WS);
            if id.is_empty() {
                continue;
            }
            let worktree = {
                let path = term.label(L_WORKTREE);
                (!path.is_empty()).then(|| WorkspaceWorktree {
                    repo_key: String::new(),
                    checkout_path: path,
                    is_linked_worktree: true,
                })
            };
            match out.iter_mut().find(|w| w.workspace_id == id) {
                Some(space) => {
                    space.pane_count += 1;
                    if space.worktree.is_none() {
                        space.worktree = worktree;
                    }
                }
                None => out.push(Workspace {
                    workspace_id: id,
                    label: term.label(L_WS_LABEL),
                    focused: false,
                    pane_count: 1,
                    worktree,
                }),
            }
        }
        Ok(out)
    }

    /// True when nothing runs in the pane: no program there reports a status.
    pub fn pane_idle_shell(&self, pane: &str) -> bool {
        match self.term(pane) {
            Ok(term) => !term.reported,
            Err(_) => false,
        }
    }

    fn created(&self, pane: &Term) -> Created {
        Created {
            workspace_id: pane.label(L_WS),
            tab_id: pane.label(L_TAB),
            pane_id: pane.pane_id(),
        }
    }

    /// Starts a terminal and returns it once core lists it.
    fn start(&self, name: &str, cwd: &Path, labels: &[(&str, &str)], argv: &[String]) -> Result<Term, HerdrError> {
        let cwd_text = cwd.to_string_lossy().into_owned();
        let mut args: Vec<String> = vec![
            "run".into(),
            "--bg".into(),
            "--no-parent".into(),
            "--name".into(),
            name.to_string(),
            "--cwd".into(),
            cwd_text,
        ];
        for (key, value) in labels {
            args.push("--label".into());
            args.push(format!("{key}={value}"));
        }
        args.extend(argv.iter().cloned());
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = self.json(&refs, Duration::from_secs(20))?;
        // `hc run` answers with the short row `hc ls` prints; the labels it was
        // given come back only from `hc get`.
        let id = result["name"].as_str().filter(|n| !n.is_empty()).or_else(|| result["address"].as_str()).unwrap_or_default();
        if id.is_empty() {
            return Err(HerdrError { code: "failed".into(), message: "`hc run` reply has no terminal".into() });
        }
        self.term(id)
    }

    pub fn workspace_create(&self, cwd: &Path, label: &str, _focus: bool) -> Result<Created, HerdrError> {
        let dir = cwd.to_string_lossy().into_owned();
        // A person who works in the Herdr app has to see the project: the app
        // makes the Space, and the terminal inside it is ours — named by us, so
        // a record keeps working across restarts, and carrying
        // `herdr.space` so the window keeps showing it.
        if crate::app::running(self.runner)
            && let Some(placed) = crate::app::space_new(self.runner, label, &dir)
        {
            let tab = format!("{}-{}", placed.space, suffix());
            let name = self.next_name(label)?;
            let term = self.start(
                &name,
                cwd,
                &[
                    (L_WS, placed.space.as_str()),
                    (L_WS_LABEL, label),
                    (L_TAB, tab.as_str()),
                    (L_TAB_LABEL, "coordinator"),
                    ("herdr.space", placed.space.as_str()),
                ],
                &Vec::new(),
            )?;
            // The space's own first tab is a placeholder shell: close it once
            // ours is there (the app will not leave a space without a tab).
            let _ = crate::app::retry(|| crate::app::tab_close(self.runner, &placed.tab));
            let address = term.address.clone();
            let _ = crate::app::retry(|| {
                crate::app::tab_of(self.runner, &address)
                    .is_some_and(|tab| crate::app::tab_rename(self.runner, &tab, "coordinator"))
            });
            return Ok(self.created(&term));
        }
        let ws = self.next_id("hpw", label)?;
        let tab = format!("{ws}-t");
        let name = self.next_name(label)?;
        let argv: Vec<String> = Vec::new();
        let term = self.start(
            &name,
            cwd,
            &[(L_WS, ws.as_str()), (L_WS_LABEL, label), (L_TAB, tab.as_str()), (L_TAB_LABEL, "root")],
            &argv,
        )?;
        Ok(self.created(&term))
    }

    pub fn tab_create(&self, workspace: &str, cwd: &Path, label: &str, _focus: bool) -> Result<Created, HerdrError> {
        if workspace.starts_with("sp") && crate::app::running(self.runner) {
            let tab = format!("{}-{}", workspace, suffix());
            let name = self.next_name(label)?;
            let term = self.start(
                &name,
                cwd,
                &[
                    (L_WS, workspace),
                    (L_TAB, tab.as_str()),
                    (L_TAB_LABEL, label),
                    ("herdr.space", workspace),
                ],
                &Vec::new(),
            )?;
            let address = term.address.clone();
            let _ = crate::app::retry(|| {
                crate::app::tab_of(self.runner, &address)
                    .is_some_and(|tab| crate::app::tab_rename(self.runner, &tab, label))
            });
            return Ok(self.created(&term));
        }
        let existing = self.terminals()?;
        let ws_label = existing
            .iter()
            .find(|t| t.label(L_WS) == workspace)
            .map(|t| t.label(L_WS_LABEL))
            .unwrap_or_default();
        let _ = existing;
        let tab = format!("{}-{}", workspace, suffix());
        let name = self.next_name(label)?;
        let term = self.start(
            &name,
            cwd,
            &[(L_WS, workspace), (L_WS_LABEL, &ws_label), (L_TAB, tab.as_str()), (L_TAB_LABEL, label)],
            &Vec::new(),
        )?;
        Ok(self.created(&term))
    }

    /// Creates a real git worktree under `~/.herdr/worktrees/<repo>/<branch>`,
    /// the path Herdr 1 used, so a restored thread record still names it.
    pub fn worktree_create(&self, repo: &str, branch: &str, base: &str, label: &str) -> Result<(Created, String, String), HerdrError> {
        let path = worktree_path(repo, branch)?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let path_text = path.to_string_lossy().into_owned();
        let git = |args: &[&str]| -> Result<(), HerdrError> {
            let out = self.runner.run(&Cmd::new("git", Duration::from_secs(60)).arg("-C").arg(repo).args(args.iter().copied())).map_err(
                |e| HerdrError { code: "failed".into(), message: format!("{e:#}") },
            )?;
            if out.success() {
                Ok(())
            } else {
                Err(HerdrError { code: "failed".into(), message: out.error_text() })
            }
        };
        let base = if base.is_empty() { "HEAD" } else { base };
        if git(&["worktree", "add", "-b", branch, &path_text, base]).is_err() {
            // The branch may already exist: reuse it instead of failing.
            git(&["worktree", "add", &path_text, branch])?;
        }
        let created = self.workspace_create(&path, label, false)?;
        let term = self.term(&created.pane_id)?;
        self.label(&term.pane_id(), &[(L_WORKTREE, path_text.as_str())])?;
        Ok((created, path_text.clone(), term.cwd()))
    }

    pub fn worktree_open(&self, _repo: &str, path: &str, label: &str) -> Result<(Created, String, String), HerdrError> {
        let path = PathBuf::from(path);
        if !path.is_dir() {
            return Err(HerdrError { code: "worktree_not_found".into(), message: format!("{path:?} is not a folder") });
        }
        let created = self.workspace_create(&path, label, false)?;
        let term = self.term(&created.pane_id)?;
        self.label(&term.pane_id(), &[(L_WORKTREE, path.to_string_lossy().as_ref())])?;
        Ok((created, path.to_string_lossy().into_owned(), term.cwd()))
    }

    /// Never passes `--force`: git refuses a dirty worktree and that refusal is
    /// reported unchanged.
    pub fn worktree_remove(&self, workspace: &str) -> Result<(), HerdrError> {
        let term = self
            .terminals()?
            .into_iter()
            .find(|t| t.label(L_WS) == workspace)
            .ok_or_else(|| HerdrError { code: "pane_not_found".into(), message: format!("no terminal in {workspace}") })?;
        let path = term.label(L_WORKTREE);
        if path.is_empty() {
            return Err(HerdrError { code: "worktree_not_found".into(), message: format!("{workspace} is not a worktree") });
        }
        let out = self
            .runner
            .run(&Cmd::new("git", Duration::from_secs(60)).arg("-C").arg(&path).args(["worktree", "remove", &path]))
            .map_err(|e| HerdrError { code: "failed".into(), message: format!("{e:#}") })?;
        if out.success() {
            return Ok(());
        }
        let text = out.error_text();
        let code = if text.contains("modified or untracked") { "dirty_worktree_requires_force" } else { "failed" };
        Err(HerdrError { code: code.into(), message: text })
    }

    /// One pane as this binary sees panes, or an error when it is not live.
    pub fn pane_get(&self, pane: &str) -> Result<Pane, HerdrError> {
        let term = self.term(pane)?;
        Ok(Pane {
            pane_id: term.pane_id(),
            tab_id: term.label(L_TAB),
            workspace_id: term.label(L_WS),
            cwd: term.cwd(),
            foreground_cwd: term.cwd(),
            terminal_id: term.address.clone(),
            focused: false,
        })
    }

    /// The kind of agent a pane runs, from the label this binary writes.
    #[cfg(test)]
    pub fn agent_kind(&self, pane: &str) -> Option<String> {
        let kind = self.term(pane).ok()?.label(L_KIND);
        (!kind.is_empty()).then_some(kind)
    }

    /// Ends every terminal of a Space.
    pub fn workspace_close(&self, workspace: &str) -> Result<(), HerdrError> {
        if workspace.starts_with("sp") && crate::app::running(self.runner) && crate::app::space_close(self.runner, workspace) {
            return Ok(());
        }
        for term in self.terminals()?.into_iter().filter(|t| t.label(L_WS) == workspace) {
            self.end(&term.pane_id())?;
        }
        Ok(())
    }

    /// Ends a tab: its terminal, and any other terminal carrying the label.
    pub fn tab_close(&self, tab: &str) -> Result<(), HerdrError> {
        for term in self.terminals()?.into_iter().filter(|t| t.label(L_TAB) == tab) {
            self.end(&term.pane_id())?;
        }
        Ok(())
    }

    /// Ends one pane.
    pub fn pane_close(&self, pane: &str) -> Result<(), HerdrError> {
        self.end(pane)
    }

    /// Kills a terminal and removes its record, tolerating one that is gone.
    fn end(&self, pane: &str) -> Result<(), HerdrError> {
        let _ = self.json(&["kill", pane], CALL_TIMEOUT);
        let _ = self.json(&["rm", pane], CALL_TIMEOUT);
        Ok(())
    }

    /// A Space's label, as the overview shows it.
    pub fn workspace_label(&self, workspace: &str) -> Result<String, HerdrError> {
        self.terminals()?
            .into_iter()
            .find(|t| t.label(L_WS) == workspace)
            .map(|t| t.label(L_WS_LABEL))
            .ok_or_else(|| HerdrError { code: "pane_not_found".into(), message: format!("no Space {workspace}") })
    }

    pub fn workspace_rename(&self, workspace: &str, label: &str) -> Result<(), HerdrError> {
        for term in self.terminals()?.into_iter().filter(|t| t.label(L_WS) == workspace) {
            self.label(&term.pane_id(), &[(L_WS_LABEL, label)])?;
        }
        Ok(())
    }

    pub fn tab_rename(&self, tab: &str, label: &str) -> Result<(), HerdrError> {
        for term in self.terminals()?.into_iter().filter(|t| t.label(L_TAB) == tab) {
            self.label(&term.pane_id(), &[(L_TAB_LABEL, label)])?;
        }
        Ok(())
    }

    /// The working directory core reports for a pane.
    pub fn pane_cwd(&self, pane: &str) -> Result<String, HerdrError> {
        Ok(self.term(pane)?.cwd())
    }

    /// Starts an agent in a pane. core has no `agent start`: the agent is a
    /// program typed into the pane's shell, and "ready" is its first status
    /// report, so a trust dialog shows up as `blocked` rather than a timeout.
    pub fn agent_start(&self, name: &str, kind: &str, pane: &str, agent_args: &[String]) -> Result<Agent, HerdrError> {
        self.label(pane, &[(L_AGENT, name), (L_KIND, kind)])?;
        let line = agent_command(kind, agent_args);
        self.json(&["send", pane, &line], AGENT_START_TIMEOUT)?;
        let deadline = std::time::Instant::now() + AGENT_START_TIMEOUT;
        loop {
            let agent = self.agent(pane)?;
            if agent.reported_status_blocked() {
                return Err(HerdrError {
                    code: "agent_not_ready".into(),
                    message: format!("{name} is blocked during startup"),
                });
            }
            if agent.ready() {
                return Ok(agent);
            }
            if std::time::Instant::now() > deadline {
                return Err(HerdrError { code: "timeout".into(), message: format!("{name} did not report ready in time") });
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// One agent by pane id.
    fn agent(&self, pane: &str) -> Result<Agent, HerdrError> {
        let term = self.term(pane)?;
        let status = term.status.clone().unwrap_or_default();
        Ok(Agent {
            pane_id: term.pane_id(),
            tab_id: term.label(L_TAB),
            workspace_id: term.label(L_WS),
            name: term.label(L_AGENT),
            agent: term.label(L_KIND),
            agent_status: if term.reported { status.state.clone() } else { String::new() },
            cwd: term.cwd(),
            foreground_cwd: term.cwd(),
            terminal_id: term.address.clone(),
            state_change_seq: term.turn,
            agent_session: {
                let value = term.label(L_SESSION);
                (!value.is_empty()).then_some(AgentSession { value })
            },
            interactive_ready: term.reported,
            tokens: term.labels.clone(),
        })
    }

    /// Submits a prompt. core refuses to type into a `blocked` program unless
    /// forced, which is what the caller wants to hear about.
    pub fn agent_prompt(&self, target: &str, text: &str) -> Result<(), HerdrError> {
        self.json(&["send", target, text], CALL_TIMEOUT).map(|_| ())
    }

    /// Submits a prompt and returns core's word on whether the turn started.
    pub fn agent_prompt_confirmed(&self, target: &str, text: &str) -> Result<(), HerdrError> {
        let result = self.json(&["send", target, text], PROMPT_WAIT + Duration::from_secs(5))?;
        match result["started"].as_bool() {
            Some(true) => Ok(()),
            // The program does not report status, or it took the text without
            // starting: the caller must not type it again blindly.
            _ => Err(HerdrError {
                code: "agent_prompt_stalled".into(),
                message: format!("{target} did not start working after the prompt"),
            }),
        }
    }

    /// The agent's screen and history as core keeps it.
    pub fn agent_read(&self, target: &str, lines: Option<usize>) -> Result<String, HerdrError> {
        self.agent_read_as(target, lines, "text")
    }

    /// `format` is kept for the call sites; core only prints plain text.
    pub fn agent_read_as(&self, target: &str, lines: Option<usize>, _format: &str) -> Result<String, HerdrError> {
        let lines = lines.map(|n| n.to_string()).unwrap_or_else(|| "0".into());
        let result = self.json(&["read", target, "--lines", &lines], CALL_TIMEOUT)?;
        Ok(result["text"].as_str().unwrap_or_default().to_string())
    }

    pub fn agent_screen(&self, target: &str) -> Result<String, HerdrError> {
        let result = self.json(&["read", target], CALL_TIMEOUT)?;
        Ok(result["text"].as_str().unwrap_or_default().to_string())
    }

    pub fn agent_send_keys(&self, target: &str, keys: &[String]) -> Result<(), HerdrError> {
        let keys: Vec<String> = keys.iter().map(|k| key_name(k)).collect();
        let mut args: Vec<&str> = vec!["keys", target];
        args.extend(keys.iter().map(String::as_str));
        self.json(&args, CALL_TIMEOUT).map(|_| ())
    }

    /// Pasting text without a submit is not something core offers: `send` is
    /// paste plus Enter, and that is what every caller here wants.
    pub fn pane_send_text(&self, pane: &str, text: &str) -> Result<(), HerdrError> {
        self.json(&["send", pane, text], CALL_TIMEOUT).map(|_| ())
    }

    /// With no windows to focus, the honest answer is how to reach the pane.
    pub fn agent_focus(&self, target: &str) -> Result<(), HerdrError> {
        println!("hc attach {target}");
        Ok(())
    }

    pub fn agent_rename(&self, pane: &str, name: &str) -> Result<(), HerdrError> {
        self.label(pane, &[(L_AGENT, name)])
    }

    /// Herdr 1's agent view was a window feature; core draws nothing, so the
    /// projection this used to install is a no-op. Kept until the modules that
    /// call it are removed.
        /// Herdr 1 let a Space be moved in its list. With no list, nothing to move.
        /// `hpw-<label>-<suffix>`, unique among the terminals this session has.
    ///
    /// The suffix is random, not a count: ids outlive the terminals that carried
    /// them (thread records, coordinator records, a digest), and a count hands an
    /// old id to a new terminal the moment one closes — which is how closing a
    /// thread's tab once closed the coordinator in the same tab id.
    fn next_id(&self, prefix: &str, label: &str) -> Result<String, HerdrError> {
        let used: Vec<String> = self.terminals()?.iter().map(|t| t.label(L_WS)).collect();
        let mut id = format!("{prefix}-{}-{}", slug(label), suffix());
        while used.iter().any(|u| u == &id) {
            id = format!("{prefix}-{}-{}", slug(label), suffix());
        }
        Ok(id)
    }

    /// A terminal name core accepts: 1-32 of a-z 0-9 - _ .
    fn next_name(&self, label: &str) -> Result<String, HerdrError> {
        let used: Vec<String> = self.terminals()?.iter().map(|t| t.pane_id()).collect();
        let mut name = truncate_name(&format!("hp-{}-{}", slug(label), suffix()));
        while used.contains(&name) {
            name = truncate_name(&format!("hp-{}-{}", slug(label), suffix()));
        }
        Ok(name)
    }
}

/// Six hex digits that make an id unique within a machine's lifetime: the
/// nanosecond clock with this process's pid mixed in.
fn suffix() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or_default();
    let mixed = nanos ^ ((std::process::id() as u64) << 20);
    format!("{:06x}", mixed & 0xff_ffff)
}

impl Agent {
    /// core reported a program waiting for a person.
    fn reported_status_blocked(&self) -> bool {
        self.agent_status == "blocked"
    }
}

/// `<repo's folder name>/<branch>` under `~/.herdr/worktrees`, as Herdr 1 laid
/// it out, so a restored record's path still resolves.
fn worktree_path(repo: &str, branch: &str) -> Result<PathBuf, HerdrError> {
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| HerdrError {
        code: "failed".into(),
        message: "HOME is not set".into(),
    })?;
    let repo_name = Path::new(repo)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let branch = branch.replace('/', "-");
    Ok(home.join(".herdr").join("worktrees").join(repo_name).join(branch))
}

/// The command that runs an agent under `hp-agent`, so its states reach core.
fn agent_command(kind: &str, args: &[String]) -> String {
    let binary = crate::paths::binary()
        .ok()
        .and_then(|path| path.parent().map(|dir| dir.join("hp-agent")))
        .filter(|path| path.exists())
        .map(|path| crate::shell::quote(&path.to_string_lossy()))
        .unwrap_or_else(|| "hp-agent".into());
    let mut line = format!("{binary} {}", crate::shell::quote(kind));
    for arg in args {
        line.push(' ');
        line.push_str(&crate::shell::quote(arg));
    }
    line
}

/// `hpw-Law Office-2` → `law-office`.
fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "space".into() } else { out }
}

fn truncate_name(name: &str) -> String {
    name.chars().take(32).collect::<String>().trim_end_matches('-').to_string()
}

/// core's key names: enter, esc, tab, up, ctrl+c.
fn key_name(key: &str) -> String {
    match key.to_ascii_lowercase().as_str() {
        "return" => "enter".into(),
        "escape" => "esc".into(),
        other => other.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("hc 0.1.21"), Some(Version(0, 1, 21)));
        assert_eq!(parse_version("hc 0.1.21-preview.1"), Some(Version(0, 1, 21)));
        assert_eq!(parse_version("herdr"), None);
    }

    #[test]
    fn a_recorded_socket_path_means_the_default_session() {
        assert_eq!(session_key("/h/.config/herdr/herdr.sock"), "");
        assert_eq!(session_key("-"), "");
        assert_eq!(session_key("hp-dev"), "hp-dev");
    }

    #[test]
    fn labels_become_the_space_tab_and_pane_ids() {
        let term = Term {
            address: "t7".into(),
            name: Some("hp-law-1".into()),
            labels: [(L_WS, "hpw-law-1"), (L_TAB, "hpw-law-1-t1")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            cwd: "/w".into(),
            state: "live".into(),
            ..Term::default()
        };
        assert_eq!(term.pane_id(), "hp-law-1");
        assert_eq!(term.label(L_WS), "hpw-law-1");
        assert_eq!(term.label(L_TAB), "hpw-law-1-t1");
    }
}
