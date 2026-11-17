//! How a project's threads read: the words of a group, a row's one-line
//! display, and the labels that carry them. Herdr 1 drew these into its
//! sidebar and tab bar; herdr-core draws nothing, so the same strings are what
//! `overview`, `list` and the digest print: the config edits, key handling and
//! view builders this module carried for Herdr 1 are gone, and the leftovers
//! are marked dead until they are pruned.
#![allow(dead_code)]

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

use crate::herdr::{CALL_TIMEOUT, Herdr};
use crate::project::{self, Project};
use crate::thread::{Group, Live, Thread};

pub const TOKEN_TTL_MS: u64 = 300_000;
/// Tokens this plugin wrote before 0.2.0; cleared on every report.
pub const OLD_TOKENS: [&str; 4] = ["project", "thread", "review", "rank"];
/// The token every report writes naming the project a pane belongs to. It is
/// what tells one project's lane table from another's (`lanes::resolve`).
pub const PROJECT_TOKEN: &str = "hp_project";
pub const POPUP_ACTION: &str = "herdr-projects.open-popup";
pub const DEFAULT_KEY: &str = "prefix+a";
/// Minutes without a self-report after which a working row says "Nm quiet".
pub const QUIET_SECS: i64 = 300;

/// The short word of a group, as the sidebar and tab bar show it.
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

/// Groups that need the user: counted in the project row and the tab bar.
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

/// Row 1 of a thread: its id and title, capped by Herdr at 80 characters.
pub fn thread_display(thread: &Thread) -> String {
    format!("{} · {}", thread.id, thread.title)
}

/// A coordinator's row name: the project's name, plus ` · <lane label>` when
/// the project has a lane registry (`docs/lanes.md` §9), marked as its group's
/// head in the agents list (see `grouping::HEAD_MARK`). The gate is the
/// registry, not the lane count: a project that never wrote `lanes.json` reads
/// exactly as before, and a project with lanes shows them even for one lane —
/// including the default — so two coordinators are never two identical rows.
pub fn coordinator_display(project: &Project, lane: Option<&str>) -> String {
    let name = project.read_project_md().map(|(s, _)| project::display_name(&s.name, &project.slug)).unwrap_or_else(|_| project.slug.clone());
    match crate::lanes::display_label(project, lane) {
        Some(label) => format!("{name} · {label}{}", crate::grouping::HEAD_MARK),
        None => format!("{name}{}", crate::grouping::HEAD_MARK),
    }
}

/// Reports one pane's row: display name, project, rank and — for a
/// coordinator whose lane the registry names — its lane claim, with a TTL so
/// the row falls back to Herdr's own when the ticker stops. Never `--seq`: token
/// patches are per key and the grouping tokens come from `grouping`.
///
/// `lane` is the **id** to write as `hp_lane` (`docs/lanes.md` §5.1), or
/// `None` for no claim: a thread pane never claims a lane, and a project with
/// no lane registry reports none, so the token appears exactly where a
/// registry says it should. `hp_lane` is never in `OLD_TOKENS`: those are
/// cleared on every report, which would erase the claim as fast as it is
/// written.
pub fn report_pane(herdr: &Herdr, pane: &str, display: &str, slug: &str, group: Group, lane: Option<&str>) {
    if pane.is_empty() {
        return;
    }
    let mut pairs: Vec<(String, String)> = vec![
        ("hp_project".into(), slug.to_string()),
        ("hp_rank".into(), group.rank().to_string()),
        ("hp_display".into(), display.to_string()),
    ];
    if let Some(lane) = lane.filter(|lane| !lane.is_empty()) {
        pairs.push((crate::lanes::TOKEN.to_string(), lane.to_string()));
    }
    let _ = herdr.set_labels(pane, &pairs);
    let _ = herdr.clear_labels(pane, &OLD_TOKENS);
}

/// Clears every label this plugin set on a pane — ours, the lane claim, and the
/// grouping tokens `grouping::clear` also removes.
pub fn clear_pane(herdr: &Herdr, pane: &str) {
    if pane.is_empty() {
        return;
    }
    let mut keys: Vec<&str> = vec!["hp_project", "hp_rank", "hp_display", crate::lanes::TOKEN];
    keys.extend(OLD_TOKENS);
    let _ = herdr.clear_labels(pane, &keys);
    crate::grouping::clear(herdr, "pane", pane);
}

/// `2 need you · 3 working`, `paused`, or `idle`.
pub fn project_line(groups: &[Group], paused: bool) -> String {
    if paused {
        return "paused".into();
    }
    let count = |f: &dyn Fn(Group) -> bool| groups.iter().filter(|g| f(**g)).count();
    let mut parts = Vec::new();
    let need = count(&|g| needs_you(g));
    if need > 0 {
        parts.push(format!("{need} need you"));
    }
    for (group, label) in [(Group::Working, "working"), (Group::Landing, "landing")] {
        let n = count(&|g| g == group);
        if n > 0 {
            parts.push(format!("{n} {label}"));
        }
    }
    if parts.is_empty() { "idle".into() } else { parts.join(" · ") }
}

/// Clears the grouping tokens of a Space.
pub fn clear_workspace(herdr: &Herdr, workspace: &str) {
    crate::grouping::clear(herdr, "workspace", workspace);
}

/// A card's sub-line: what the state line adds to Herdr's own state word,
/// then the agent's last activity. `working · ~40%` and `Writing tests` give
/// `~40% · Writing tests`; a bare `idle` or `working` gives nothing.
pub fn sub_line(state_line: &str, activity: &str) -> String {
    let fact = match state_line {
        "idle" | "working" | "resolved" => "",
        line => line.strip_prefix("working · ").unwrap_or(line),
    };
    [fact, activity.trim()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" · ")
}

/// The groups of a project's open threads, as the ticker last persisted them.
/// A closed lane's threads are not counted (§9): `needs-you` and the project
/// row must not carry a lane that has been put away. Without a registry every
/// thread counts, exactly as before.
pub fn recorded_groups(project: &Project) -> Vec<Group> {
    crate::thread::list(project)
        .into_iter()
        .filter(|t| t.status != crate::thread::Status::Resolved)
        .filter(|t| crate::lanes::thread_visible(project, &t.lane))
        .filter_map(|t| Group::from_token(&t.last_group))
        .collect()
}

/// `needs-you --line`, the tab-bar entry: `projects: N need you`, from the
/// thread records the ticker persists. Nothing when none need the user or when
/// the ticker is not running (a stale count must not sit in the tab bar).
pub fn needs_you_line(root: &Path) -> Option<String> {
    if crate::ticker::lock_state(root) == crate::ticker::LockState::Free {
        return None;
    }
    let n: usize = project::list_slugs(root)
        .iter()
        .filter_map(|slug| Project::load(root, slug).ok())
        .filter(|p| p.status() == project::Status::Active)
        .map(|p| recorded_groups(&p).into_iter().filter(|g| needs_you(*g)).count())
        .sum();
    (n > 0).then(|| format!("projects: {n} need you"))
}

// ---------------------------------------------------------------- agent view

/// The default view: agents in project blocks (`hp_group`, see `grouping`),
/// each by need; agents without it last.
pub fn default_view() -> serde_json::Value {
    serde_json::json!({
        "source": crate::herdr::SOURCE,
        "label": "projects",
        "sort": [{ "field": { "token": "hp_group" }, "order": "asc" }, { "field": { "token": "hp_rank" }, "order": "asc" }],
    })
}

/// `focus <slug>`: only that project's agents, by need.
pub fn project_view(slug: &str) -> serde_json::Value {
    serde_json::json!({
        "source": crate::herdr::SOURCE,
        "label": format!("project: {slug}"),
        "filter": { "op": "eq", "field": { "token": "hp_project" }, "value": slug },
        "sort": [{ "field": { "token": "hp_group" }, "order": "asc" }, { "field": { "token": "hp_rank" }, "order": "asc" }],
    })
}

// ---------------------------------------------------------------- config.toml

/// What `configure` adds to the user's Herdr config.
pub struct Spec {
    pub key: String,
    /// The tab-bar command (absolute binary path; runs under `/bin/sh -lc`).
    pub tab_command: String,
}

/// The sub-line under an agent: its own detail (`review · report`). The
/// only row of ours; a card without a sub-line draws none.
fn sub_row() -> Value {
    let mut t = InlineTable::new();
    t.insert("token", "$hp_sub".into());
    t.insert("dim", true.into());
    let mut row = Array::new();
    row.push(t);
    row.into()
}

/// `{ token, rules = [{ contains = mark, bold = true }] }` plus `extra`: the
/// token of a row that is a project's head when its value carries `mark`.
fn head_token(token: &str, mark: char, extra: &[(&str, bool)]) -> InlineTable {
    let mut rule = InlineTable::new();
    rule.insert("contains", mark.to_string().into());
    rule.insert("bold", true.into());
    let mut rules = Array::new();
    rules.push(rule);
    let mut t = InlineTable::new();
    t.insert("token", token.into());
    for (key, value) in extra {
        t.insert(*key, (*value).into());
    }
    t.insert("rules", rules.into());
    t
}

fn row_of(tokens: &[&str]) -> Value {
    let mut row = Array::new();
    for t in tokens {
        row.push(*t);
    }
    row.into()
}

/// Herdr's built-in rows (0.9.1), which `configure` wrote into a config that
/// had none.
fn herdr_agent_rows() -> [Value; 2] {
    [row_of(&["state_icon", "machine", "workspace", "tab"]), row_of(&["agent"])]
}

fn herdr_space_rows() -> [Value; 2] {
    [row_of(&["state_icon", "workspace"]), row_of(&["branch", "git_status"])]
}

/// The agent card that replaces Herdr's built-in rows: one line, status
/// icon, name, Herdr's state word. Names are plain; a coordinator's (its
/// project's name, see `grouping::HEAD_MARK`) is bold, the head of its group.
fn agent_card() -> Value {
    let mut row = Array::new();
    row.push("state_icon");
    row.push(head_token("agent", crate::grouping::HEAD_MARK, &[("bold", false), ("dim", false)]));
    row.push("state_text");
    row.into()
}

/// The Space card that replaces Herdr's built-in rows: one line, with the
/// branch after the name. A home Space's label (its project's name, see
/// `grouping::HOME_MARK`) is bold, the head of its group.
fn space_card() -> Value {
    let mut row = Array::new();
    row.push("state_icon");
    row.push(head_token("workspace", crate::grouping::HOME_MARK, &[]));
    let mut branch = InlineTable::new();
    branch.insert("token", "branch".into());
    branch.insert("dim", true.into());
    row.push(branch);
    row.push("git_status");
    row.into()
}

/// The 0.2.19 cards, swapped for today's wherever they stand.
fn cards_0_2_19() -> (Value, Value) {
    let mut agent = Array::new();
    agent.push("state_icon");
    let mut name = InlineTable::new();
    name.insert("token", "agent".into());
    name.insert("bold", true.into());
    name.insert("dim", false.into());
    agent.push(name);
    agent.push("state_text");
    let mut space = Array::new();
    space.push("state_icon");
    let mut rule = InlineTable::new();
    rule.insert("contains", crate::grouping::HOME_MARK.to_string().into());
    rule.insert("hide", true.into());
    let mut rules = Array::new();
    rules.push(rule);
    let mut workspace = InlineTable::new();
    workspace.insert("token", "workspace".into());
    workspace.insert("rules", rules.into());
    space.push(workspace);
    let mut home = InlineTable::new();
    home.insert("token", "$hp_home".into());
    space.push(home);
    let mut branch = InlineTable::new();
    branch.insert("token", "branch".into());
    branch.insert("dim", true.into());
    space.push(branch);
    space.push("git_status");
    (agent.into(), space.into())
}


/// The tokens a row names, in order.
fn row_tokens(row: &Value) -> Vec<String> {
    row.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.value().clone()),
                    Value::InlineTable(t) => t.get("token").and_then(Value::as_str).map(str::to_string),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A row this plugin owns: every token is one of ours, today's or an
/// earlier layout's (the 0.2.17/0.2.18 headings, the 0.2.19 rails).
fn ours(row: &Value) -> bool {
    let known: Vec<String> = crate::grouping::tokens().into_iter().chain(crate::grouping::legacy()).map(|t| format!("${t}")).collect();
    let tokens = row_tokens(row);
    !tokens.is_empty() && tokens.iter().all(|t| known.contains(t))
}

fn same(a: &Value, b: &Value) -> bool {
    a.to_string().split_whitespace().collect::<String>() == b.to_string().split_whitespace().collect::<String>()
}

/// Rebuilds one `rows` array: drops every row of ours (today's and earlier
/// layouts'), swaps an earlier card of ours for `card` in place, swaps
/// Herdr's built-in rows for `card` when they are exactly Herdr's (never rows
/// the user wrote), then puts `last` at the bottom. With `remove`, only drops
/// ours and puts Herdr's built-in rows back in place of `card`.
fn edit_rows(item: &mut Item, last: &[Value], builtin: &[Value], card: Option<(&Value, &Value)>, remove: bool) -> Result<()> {
    let rows = item.as_array_mut().context("sidebar rows must be an array")?;
    rows.retain(|v| !ours(v));
    let card = card.map(|(card, old)| {
        for row in rows.iter_mut() {
            if same(row, old) {
                *row = card.clone();
            }
        }
        card
    });
    let current: Vec<Value> = rows.iter().cloned().collect();
    if let Some(card) = card {
        let replace = |rows: &mut Array, with: &[Value]| {
            rows.clear();
            for row in with {
                rows.push(row.clone());
            }
        };
        let is_builtin = current.len() == builtin.len() && current.iter().zip(builtin).all(|(a, b)| same(a, b));
        let is_card = current.len() == 1 && same(&current[0], card);
        if !remove && is_builtin {
            replace(rows, std::slice::from_ref(card));
        } else if remove && is_card {
            // The card's head rule (an invisible mark of ours) makes it ours.
            replace(rows, builtin);
        }
    }
    if remove {
        return Ok(());
    }
    ensure!(rows.len() + last.len() <= 16, "the sidebar already has {} rows; remove one before configuring", rows.len());
    for row in last {
        rows.push(row.clone());
    }
    Ok(())
}

fn table_mut<'a>(parent: &'a mut Item, key: &str) -> Result<&'a mut Item> {
    let table = parent.as_table_like_mut().with_context(|| format!("`{key}`'s parent must be a table"))?;
    if table.get(key).is_none() {
        let mut t = Table::new();
        t.set_implicit(true);
        table.insert(key, Item::Table(t));
    }
    Ok(table.get_mut(key).unwrap())
}

/// Adds (or removes) the rows, the popup key and the tab-bar entry. Existing
/// rows, keys and entries of the user are never touched. Idempotent.
pub fn config_edit(input: &str, spec: &Spec, remove: bool) -> Result<String> {
    let mut doc = input.parse::<DocumentMut>().context("config.toml does not parse")?;
    let agent_last = [sub_row()];
    let (agent_card, space_card) = (agent_card(), space_card());
    let (old_agent_card, old_space_card) = cards_0_2_19();

    // Agent rows, and every per-harness override (which replaces `rows`).
    {
        let ui = table_mut(doc.as_item_mut(), "ui")?;
        let sidebar = table_mut(ui, "sidebar")?;
        let agents = table_mut(sidebar, "agents")?;
        if agents.get("rows").is_none() && !remove {
            agents["rows"] = toml_edit::value(Array::from_iter(herdr_agent_rows()));
        }
        if let Some(rows) = agents.get_mut("rows").filter(|v| !v.is_none()) {
            edit_rows(rows, &agent_last, &herdr_agent_rows(), Some((&agent_card, &old_agent_card)), remove)?;
        }
        if let Some(overrides) = agents.get_mut("rows_by_agent").filter(|v| !v.is_none()) {
            for (_, rows) in overrides.as_table_like_mut().context("rows_by_agent must be a table")?.iter_mut() {
                edit_rows(rows, &agent_last, &herdr_agent_rows(), None, remove)?;
            }
        }
        let spaces = table_mut(sidebar, "spaces")?;
        if spaces.get("rows").is_none() && !remove {
            spaces["rows"] = toml_edit::value(Array::from_iter(herdr_space_rows()));
        }
        if let Some(rows) = spaces.get_mut("rows").filter(|v| !v.is_none()) {
            edit_rows(rows, &[], &herdr_space_rows(), Some((&space_card, &old_space_card)), remove)?;
        }

        // Tab bar: one command entry.
        let is_ours = |v: &Value| v.as_inline_table().and_then(|t| t.get("command")).and_then(Value::as_str).is_some_and(|c| c.contains("needs-you --line"));
        if ui.get("tab_bar_right").is_none() && !remove {
            ui["tab_bar_right"] = toml_edit::value(Array::new());
        }
        if let Some(entries) = ui.get_mut("tab_bar_right").and_then(Item::as_array_mut) {
            entries.retain(|v| !is_ours(v) || (!remove && v.as_inline_table().and_then(|t| t.get("command")).and_then(Value::as_str) == Some(spec.tab_command.as_str())));
            if !remove && !entries.iter().any(is_ours) {
                let mut entry = InlineTable::new();
                entry.insert("type", "command".into());
                entry.insert("command", spec.tab_command.as_str().into());
                entry.insert("interval_seconds", 15.into());
                entry.insert("timeout_seconds", 5.into());
                entries.push(entry);
            }
        }
    }

    // The popup key: one [[keys.command]] entry.
    {
        let keys = table_mut(doc.as_item_mut(), "keys")?;
        let table = keys.as_table_like_mut().context("`keys` must be a table")?;
        if table.get("command").is_none() && !remove {
            table.insert("command", Item::ArrayOfTables(ArrayOfTables::new()));
        }
        if let Some(commands) = table.get_mut("command").and_then(Item::as_array_of_tables_mut) {
            let ours = |t: &Table| t.get("command").and_then(Item::as_str) == Some(POPUP_ACTION);
            if remove {
                commands.retain(|t| !ours(t));
            } else if let Some(existing) = commands.iter_mut().find(|t| ours(t)) {
                existing["key"] = toml_edit::value(spec.key.as_str());
            } else {
                let mut entry = Table::new();
                entry["key"] = toml_edit::value(spec.key.as_str());
                entry["type"] = toml_edit::value("plugin_action");
                entry["command"] = toml_edit::value(POPUP_ACTION);
                entry["description"] = toml_edit::value("Projects");
                commands.push(entry);
            }
        }
    }
    Ok(doc.to_string())
}

/// Herdr's built-in bindings, from the commented `# action = "key"` lines
/// under `[keys]` in `herdr --default-config`.
pub fn builtin_keys(default_config: &str) -> Vec<(String, String)> {
    let mut inside = false;
    let mut keys = Vec::new();
    for line in default_config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            inside = trimmed == "[keys]";
            continue;
        }
        if !inside {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix('#') else {
            continue;
        };
        // The commented `[[keys.command]]` example is not a binding.
        if rest.trim_start().starts_with("[[") {
            inside = false;
            continue;
        }
        let Some((action, value)) = rest.split_once('=') else {
            continue;
        };
        let action = action.trim();
        if action.is_empty() || action.contains(' ') || matches!(action, "key" | "type" | "command" | "description" | "width" | "height") {
            continue;
        }
        let value = value.trim();
        let Some(key) = value.strip_prefix('"').and_then(|v| v.split('"').next()) else {
            continue;
        };
        if !key.is_empty() {
            keys.push((action.to_string(), key.to_string()));
        }
    }
    keys
}

/// Why `key` cannot be the popup key, or `None` when it is free: bound in the
/// user's `[keys]`, by another `[[keys.command]]`, or in Herdr's built-in map
/// (unless the user rebound that action to another key).
pub fn key_conflict(config: &str, key: &str, builtin: &[(String, String)]) -> Option<String> {
    let doc = config.parse::<DocumentMut>().ok()?;
    let keys = doc.get("keys").and_then(Item::as_table_like);
    let mut overridden = Vec::new();
    if let Some(keys) = keys {
        for (action, value) in keys.iter() {
            if action == "command" {
                continue;
            }
            let bound: Vec<String> = match value {
                Item::Value(Value::String(s)) => vec![s.value().clone()],
                Item::Value(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
                _ => Vec::new(),
            };
            if bound.iter().any(|b| b == key) {
                return Some(format!("`{key}` is bound to `{action}` in your [keys]"));
            }
            overridden.push(action.to_string());
        }
        if let Some(commands) = keys.get("command").and_then(Item::as_array_of_tables) {
            for c in commands.iter() {
                if c.get("key").and_then(Item::as_str) == Some(key) && c.get("command").and_then(Item::as_str) != Some(POPUP_ACTION) {
                    return Some(format!("`{key}` already runs `{}`", c.get("command").and_then(Item::as_str).unwrap_or("a custom command")));
                }
            }
        }
    }
    builtin
        .iter()
        .find(|(action, bound)| bound == key && !overridden.contains(action))
        .map(|(action, _)| format!("`{key}` is Herdr's built-in `{action}` key"))
}

/// Checks a candidate config with `herdr config check` before it goes live.
pub fn check_config(herdr_bin: &str, runner: &dyn crate::runner::Runner, text: &str, scratch: &Path) -> Result<()> {
    std::fs::create_dir_all(scratch)?;
    let candidate = scratch.join(format!("config-check-{}.toml", std::process::id()));
    std::fs::write(&candidate, text)?;
    let out = runner.run(&crate::runner::Cmd::new(herdr_bin, CALL_TIMEOUT).env("HERDR_CONFIG_PATH", candidate.to_string_lossy()).args(["config", "check"]));
    let _ = std::fs::remove_file(&candidate);
    let out = out?;
    if !out.success() {
        bail!("Herdr rejected the proposed config ({}); nothing was changed", out.error_text());
    }
    Ok(())
}
