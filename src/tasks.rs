//! TASKS.md: `## List` headings, one `- [ ] title (owner) · t-0007` line per
//! task, and an optional description indented under its task line.
//!
//! `hp task` is the only writer: every write is one `Project::lock()`-protected
//! read-modify-write that leaves every other byte of the file alone (other
//! sections, blank lines, comments, indentation, CRLF and a missing final
//! newline). The read path never rewrites anything.
//!
//! Headings can be shared (`## Backlog`) or belong to one lane
//! (`## <lane label or name>/<list>`), following `docs/lanes.md` §8. A project
//! without `.state/lanes.json` has no lane headings: `hp task` still works and
//! every list is shared, byte for byte as before lanes existed.

use anyhow::{bail, Result};
use std::fmt;
use std::path::Path;

use crate::lanes::{Lane, Registry};
use crate::project::Project;

/// Who a task is assigned to: the text in its `(...)`. The thread running it
/// is status, kept apart in `Task::thread`.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Owner {
    /// No `(...)`, or an old `(agent)`.
    #[default]
    Unassigned,
    /// `(me)`: the user.
    Me,
    /// `(codex-fast)` on this machine, `(@m1)` for whatever profile m1's
    /// coordinator picks, or `(codex-fast@m1)`. Neither set is an old
    /// `(agent → t-0007)`: this machine's default thread profile.
    Agent { profile: Option<String>, machine: Option<String> },
    /// Free text that is not name-shaped, such as `Priya Rao`: a person the
    /// user named. A name-shaped owner that is no profile here (`Priya`)
    /// parses as `Agent` and is told apart by `is_person`.
    Person(String),
}

impl Owner {
    pub fn parse(text: &str) -> Owner {
        let text = text.trim();
        match text {
            "" | "agent" => return Owner::Unassigned,
            "me" => return Owner::Me,
            _ => {}
        }
        let (profile, machine) = match text.split_once('@') {
            Some((profile, machine)) => (profile, Some(machine)),
            None => (text, None),
        };
        let profile_ok = profile.is_empty() && machine.is_some() || crate::profiles::validate_name(profile).is_ok();
        if !profile_ok || machine.is_some_and(|m| !is_machine_name(m)) {
            return Owner::Person(text.to_string());
        }
        Owner::Agent { profile: (!profile.is_empty()).then(|| profile.to_string()), machine: machine.map(str::to_string) }
    }

    /// Whether this owner is a person: free text, or a bare name that
    /// `is_profile` does not know. `profile@machine` and `@machine` are
    /// always agents.
    pub fn is_person(&self, is_profile: impl Fn(&str) -> bool) -> bool {
        match self {
            Owner::Person(_) => true,
            Owner::Agent { profile: Some(profile), machine: None } => !is_profile(profile),
            _ => false,
        }
    }
}

/// A machine label as Herdr and config.toml name one: letters, digits, `.`,
/// `_` and `-`.
pub fn is_machine_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('-') && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

impl fmt::Display for Owner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Owner::Unassigned => Ok(()),
            Owner::Me => f.write_str("me"),
            Owner::Agent { profile: None, machine: None } => f.write_str("default profile"),
            Owner::Agent { profile, machine } => {
                f.write_str(profile.as_deref().unwrap_or(""))?;
                match machine {
                    Some(m) => write!(f, "@{m}"),
                    None => Ok(()),
                }
            }
            Owner::Person(text) => f.write_str(text),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Task {
    pub list: String,
    pub title: String,
    pub owner: Owner,
    /// The thread running the task: ` · t-0007` after the owner.
    pub thread: Option<String>,
    /// The indented lines under the task line, with the indent removed.
    pub description: String,
    /// The lane id when the task sits under `## <lane label or name>/<list>`
    /// and the prefix names exactly one lane. `None` is a shared list, which
    /// every lane sees (a project with no registry has only shared lists).
    pub lane: Option<String>,
}

fn is_task_line(line: &str) -> bool {
    line.trim_start().starts_with("- [") && line.trim_start().get(3..5).is_some_and(|s| s.ends_with(']'))
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The description lines that follow the task line at `index`: indented,
/// not themselves task lines; blank lines count only between such lines.
fn body(lines: &[&str], index: usize) -> (Vec<String>, usize) {
    let base = indent(lines[index]);
    let mut end = index + 1;
    let mut last = index;
    while end < lines.len() {
        let line = lines[end];
        if line.trim().is_empty() {
            end += 1;
            continue;
        }
        if indent(line) <= base || is_task_line(line) || line.starts_with('#') {
            break;
        }
        last = end;
        end += 1;
    }
    let taken: Vec<&str> = lines[index + 1..=last].to_vec();
    let cut = taken.iter().filter(|l| !l.trim().is_empty()).map(|l| indent(l)).min().unwrap_or(0);
    let text = taken.iter().map(|l| if l.trim().is_empty() { String::new() } else { l[cut..].trim_end().to_string() }).collect();
    (text, last + 1)
}

pub fn parse(text: &str) -> Vec<Task> {
    parse_with(text, None)
}

/// `parse`, with lane headings attributed when a registry is given. A heading
/// whose part before the first `/` names a lane (its `name` first, else its
/// `label`) is that lane's list; everything else is shared, including a prefix
/// that names no lane and the ambiguous case where several labels collide.
/// `Task.list` is the heading as written (a trailing HTML comment is not part
/// of it), so `## 抢修/Backlog` and a shared `## Backlog` stay distinct groups.
pub fn parse_with(text: &str, registry: Option<&Registry>) -> Vec<Task> {
    let lines: Vec<&str> = text.lines().collect();
    let mut list = String::new();
    let mut lane: Option<String> = None;
    let mut tasks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(heading) = line.strip_prefix("## ") {
            match registry {
                // A project without a registry keeps the old parse exactly:
                // the whole heading, comments included, is the list name.
                None => {
                    list = heading.trim().to_string();
                    lane = None;
                }
                Some(registry) => {
                    let (resolved, list_name) = heading_owner(heading, Some(registry));
                    // Keep the heading as written for display (`## 抢修/Backlog`
                    // is a different popup group from a shared `## Backlog`),
                    // while `lanes` write path uses the bare list name.
                    list = if resolved.is_some() { strip_trailing_comment(heading) } else { list_name };
                    lane = resolved;
                }
            }
            i += 1;
            continue;
        }
        if !is_task_line(line) {
            i += 1;
            continue;
        }
        let (title, owner, thread) = split_line(line.trim_start()[5..].trim());
        let (description, next) = body(&lines, i);
        tasks.push(Task { list: list.clone(), title, owner, thread, description: description.join("\n"), lane: lane.clone() });
        i = next;
    }
    tasks
}

/// The lane and list a `## ` heading names. The prefix before the first `/`
/// is matched against lane names first and labels second; a heading with no
/// `/`, an unknown prefix or a colliding label is shared. A trailing
/// `<!-- … -->` comment is not part of the list name.
fn heading_owner(heading: &str, registry: Option<&Registry>) -> (Option<String>, String) {
    let cleaned = strip_trailing_comment(heading);
    if let Some(registry) = registry
        && let Some((prefix, rest)) = cleaned.split_once('/')
        && let Some(lane) = unique_lane(registry, prefix.trim())
    {
        return (Some(lane.id.clone()), rest.trim().to_string());
    }
    (None, cleaned)
}

/// A heading prefix naming a lane: an exact `name` wins over a `label` (names
/// are unique); a label shared by several lanes resolves to none, so the
/// section stays shared rather than disappearing from every lane.
fn unique_lane<'a>(registry: &'a Registry, prefix: &str) -> Option<&'a Lane> {
    let names: Vec<&Lane> = registry.lanes.iter().filter(|lane| lane.name == prefix).collect();
    if names.len() == 1 {
        return Some(names[0]);
    }
    if !names.is_empty() {
        return None;
    }
    let labels: Vec<&Lane> = registry.lanes.iter().filter(|lane| lane.label == prefix).collect();
    (labels.len() == 1).then(|| labels[0])
}

fn strip_trailing_comment(heading: &str) -> String {
    let trimmed = heading.trim_end();
    if trimmed.ends_with("-->")
        && let Some(start) = trimmed.rfind("<!--")
    {
        return trimmed[..start].trim_end().to_string();
    }
    trimmed.to_string()
}

fn is_thread_id(text: &str) -> bool {
    text.strip_prefix("t-").is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
}

/// `Title (owner) · t-0007` into its parts. An old `(agent → t-0007)` is
/// this machine's default profile with thread t-0007.
fn split_line(rest: &str) -> (String, Owner, Option<String>) {
    let (rest, mut thread) = match rest.rsplit_once('·') {
        Some((head, id)) if is_thread_id(id.trim()) => (head.trim_end(), Some(id.trim().to_string())),
        _ => (rest, None),
    };
    let Some(open) = rest.rfind('(').filter(|_| rest.ends_with(')')) else {
        return (rest.to_string(), Owner::Unassigned, thread);
    };
    let title = rest[..open].trim().to_string();
    let inner = rest[open + 1..rest.len() - 1].trim();
    let owner = match inner.split_once('→') {
        Some((who, id)) if is_thread_id(id.trim()) => {
            thread = thread.or_else(|| Some(id.trim().to_string()));
            match who.trim() {
                "agent" => Owner::Agent { profile: None, machine: None },
                who => Owner::parse(who),
            }
        }
        _ => Owner::parse(inner),
    };
    (title, owner, thread)
}

pub fn read(project_dir: &Path) -> String {
    std::fs::read_to_string(project_dir.join("TASKS.md")).unwrap_or_default()
}

/// TASKS.md for `hp context`: every description folded into one line, so a
/// long body costs a few tokens; the coordinator reads the file for the rest.
pub fn compact(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        out.push(lines[i].to_string());
        if !is_task_line(lines[i]) {
            i += 1;
            continue;
        }
        let (description, next) = body(&lines, i);
        let filled: Vec<&String> = description.iter().filter(|l| !l.trim().is_empty()).collect();
        if let Some(first) = filled.first() {
            let mut preview: String = first.trim().chars().take(60).collect();
            if first.trim().chars().count() > 60 {
                preview.push('…');
            }
            let more = if filled.len() > 1 { format!(" (+{} more lines in TASKS.md)", filled.len() - 1) } else { String::new() };
            out.push(format!("{}  notes: {preview}{more}", " ".repeat(indent(lines[i]))));
        }
        i = next;
    }
    out.join("\n")
}

/// `compact` for one lane's `hp context`: shared sections plus only that
/// lane's own sections. Another lane's sections and every closed lane's
/// sections are left out; the file itself is never changed, so a closed
/// lane's tasks are still there when it is reopened. The predicate is a pure
/// line filter over the same text `compact` folds, so a project without a
/// registry keeps `compact` byte for byte.
pub fn compact_for(text: &str, registry: &Registry, lane: &str) -> String {
    compact(&filter_sections(text, registry, lane))
}

fn filter_sections(text: &str, registry: &Registry, lane: &str) -> String {
    let mut out = Vec::new();
    let mut hidden = false;
    for line in text.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            let (owner, _) = heading_owner(heading, Some(registry));
            hidden = owner.is_some_and(|id| id != lane || registry.get(&id).is_some_and(|record| !record.is_open()));
        }
        if !hidden {
            out.push(line);
        }
    }
    out.join("\n")
}

/// The task text for a thread delegated from the TASKS.md task `title`: the
/// coordinator's text, then the task's description when it has one.
pub fn delegated(tasks_md: &str, title: &str, task: &str) -> Result<String> {
    Ok(delegated_from(&find(tasks_md, title)?, task))
}

/// [`delegated`] for a task `find_in` already selected: `--from-task` must
/// append the notes of the same task it validated, not the first same-titled
/// task in another lane.
pub fn delegated_from(found: &Task, task: &str) -> String {
    if found.description.trim().is_empty() {
        return task.to_string();
    }
    format!("{}\n\n## Notes from the task list\n\n{}\n", task.trim_end(), found.description.trim())
}

/// The TASKS.md task titled `title`, ignoring case.
pub fn find(tasks_md: &str, title: &str) -> Result<Task> {
    let wanted = title.trim().to_lowercase();
    match parse(tasks_md).into_iter().find(|t| t.title.to_lowercase() == wanted) {
        Some(task) => Ok(task),
        None => bail!("TASKS.md has no task titled \"{title}\"; --from-task takes the title exactly as it is written there"),
    }
}

/// `find` with lane attribution: the same title search, but if several lanes
/// happen to use one title the caller's own lane (or a shared task) is chosen
/// when that is the only candidate. A real ambiguity is an error rather than
/// the first task in file order, because `--from-task` must not silently take
/// another lane's task.
pub fn find_in(tasks_md: &str, title: &str, registry: Option<&Registry>, lane: Option<&str>) -> Result<Task> {
    let wanted = title.trim().to_lowercase();
    let all: Vec<Task> = parse_with(tasks_md, registry).into_iter().filter(|task| task.title.to_lowercase() == wanted).collect();
    let mut chosen = all.clone();
    if chosen.len() > 1
        && let Some(lane) = lane
    {
        let own: Vec<Task> = chosen.iter().filter(|task| task.lane.as_deref().is_none_or(|id| id == lane)).cloned().collect();
        if own.len() == 1 {
            chosen = own;
        }
    }
    match chosen.len() {
        1 => Ok(chosen.remove(0)),
        0 => {
            let known: Vec<String> = parse_with(tasks_md, registry).iter().take(12).map(|task| format!("\"{}\" ({})", task.title, task.list)).collect();
            if known.is_empty() {
                bail!("TASKS.md has no task titled \"{title}\"; --from-task takes the title exactly as it is written there")
            }
            bail!("TASKS.md has no task titled \"{title}\"; known titles: {}", known.join(", "))
        }
        _ => {
            let mut where_: Vec<String> = chosen.iter().map(|task| format!("{} ({})", task.title, task.list)).collect();
            where_.sort();
            bail!("TASKS.md has {} tasks titled \"{title}\": {}; titles used by `hp task` and --from-task must be unique", chosen.len(), where_.join(", "))
        }
    }
}

/// The `--profile` and `--machine` a delegated task starts with: its owner's,
/// unless the command line gives its own. A flag that contradicts the owner is
/// refused, so the task line and the thread never disagree.
pub fn launch_for(task: &Task, profile: Option<String>, machine: Option<String>) -> Result<(Option<String>, Option<String>)> {
    let title = &task.title;
    match &task.owner {
        Owner::Unassigned => Ok((profile, machine)),
        Owner::Me => bail!("\"{title}\" is the user's own task (me); change its owner in TASKS.md before delegating it"),
        Owner::Person(text) => bail!("\"{title}\" belongs to {text}, a person; people's tasks are never delegated. Change its owner in TASKS.md first if the user asks"),
        Owner::Agent { profile: owned_profile, machine: owned_machine } => {
            if let (Some(flag), Some(owned)) = (&profile, owned_profile)
                && flag != owned
            {
                bail!("\"{title}\" is assigned to profile `{owned}`, not `{flag}`; change its owner in TASKS.md first, or leave out --profile");
            }
            if machine != *owned_machine && machine.is_some() {
                let owned = owned_machine.as_deref().map_or("this machine".to_string(), |m| format!("machine `{m}`"));
                bail!("\"{title}\" is assigned to {owned}, not `{}`; change its owner in TASKS.md first, or leave out --machine", machine.unwrap_or_default());
            }
            Ok((profile.or_else(|| owned_profile.clone()), owned_machine.clone()))
        }
    }
}

/// What `hp task add` puts on the task line and under it.
#[derive(Debug, Clone, Default)]
pub struct AddOptions<'a> {
    /// `--owner`, parsed with [`Owner::parse`] — never a second syntax.
    pub owner: Option<&'a str>,
    /// `--lane`: the lane whose list is written. Without it the list is shared.
    pub lane: Option<&'a str>,
    /// `--notes`: indented under the task line, one line per line.
    pub notes: Option<&'a str>,
}

/// `hp task add`: append one task to a list, creating the list (and, for an
/// empty or missing file, the `# Tasks` skeleton) when it does not exist yet.
/// The whole read-modify-write runs under `Project::lock()`, and the file is
/// rendered line by line from the bytes that were read under that lock: no
/// section, blank line, comment, indent or line ending outside the added block
/// changes.
pub fn add(project: &Project, list: &str, title: &str, options: &AddOptions<'_>) -> Result<String> {
    let title = clean_title(title)?;
    let list = clean_list(list)?;
    let registry = Registry::load(project);
    let target = add_target(registry.as_ref(), list, options.lane)?;
    let owner = options
        .owner
        .map(Owner::parse)
        .filter(|owner| *owner != Owner::Unassigned);
    with_tasks(project, |document| {
        let tasks = located_tasks(&document.lines, registry.as_ref());
        if let Some(found) = tasks
            .iter()
            .find(|task| task.title.to_lowercase() == title.to_lowercase())
        {
            bail!(
                "TASKS.md already has a task titled \"{}\" in list \"{}\"; titles must be unique for `hp task` commands",
                title,
                found.display_list(registry.as_ref())
            );
        }
        let block = task_block(&title, owner.as_ref(), options.notes, &document.newline());
        if let Some(section) = document
            .sections(registry.as_ref())
            .into_iter()
            .find(|section| {
                section.lane.as_deref() == target.lane.as_deref() && section.list == target.list
            })
        {
            let display = section.display(registry.as_ref());
            document.insert_at_section_end(&section, block);
            Ok(format!("added \"{title}\" to \"{display}\""))
        } else {
            if document.blank() {
                document.start_skeleton(&target.heading);
            } else {
                document.append_section(&target.heading);
            }
            let section = document
                .sections(registry.as_ref())
                .pop()
                .expect("just appended");
            let display = section.display(registry.as_ref());
            document.insert_at_section_end(&section, block);
            Ok(format!("added \"{title}\" to new list \"{display}\""))
        }
    })
}

/// `hp task move`: move the uniquely titled task line and its indented notes,
/// as a block, to another list. A title with several matches is refused with
/// every match named — guessing which one the caller meant is data loss.
pub fn move_task(project: &Project, title: &str, list: &str, lane: Option<&str>) -> Result<String> {
    let title = clean_title(title)?;
    let list = clean_list(list)?;
    let registry = Registry::load(project);
    with_tasks(project, |document| {
        let tasks = located_tasks(&document.lines, registry.as_ref());
        let task = unique_task(&tasks, &title, registry.as_ref())?;
        guard_task_lane(
            registry.as_ref(),
            task.lane.as_deref(),
            lane,
            &title,
            "move",
        )?;
        let sections = document.sections(registry.as_ref());
        let target = move_target(registry.as_ref(), list, &sections, lane)?;
        guard_target_lane(
            registry.as_ref(),
            target.lane.as_deref(),
            lane,
            &target.heading,
        )?;
        if task.lane == target.lane && task.list == target.list {
            return Ok(format!(
                "\"{}\" is already in list \"{}\"; nothing was moved",
                task.title,
                task.display_list(registry.as_ref())
            ));
        }
        let from = task.display_list(registry.as_ref());
        let block: Vec<RawLine> = document.lines.drain(task.start..task.end).collect();
        if let Some(section) = document
            .sections(registry.as_ref())
            .into_iter()
            .find(|section| {
                section.lane.as_deref() == target.lane.as_deref() && section.list == target.list
            })
        {
            document.insert_at_section_end(&section, block);
        } else {
            if document.blank() {
                document.start_skeleton(&target.heading);
            } else {
                document.append_section(&target.heading);
            }
            let section = document
                .sections(registry.as_ref())
                .pop()
                .expect("just appended");
            document.insert_at_section_end(&section, block);
        }
        Ok(format!(
            "moved \"{}\" from \"{from}\" to \"{}\"",
            task.title, target.heading
        ))
    })
}

/// `hp task done`: tick the uniquely titled task's box, keeping the line and
/// every byte around it. Running it a second time is a readable no-op, not a
/// second edit.
pub fn done(project: &Project, title: &str, lane: Option<&str>) -> Result<String> {
    let title = clean_title(title)?;
    let registry = Registry::load(project);
    with_tasks(project, |document| {
        let tasks = located_tasks(&document.lines, registry.as_ref());
        let task = unique_task(&tasks, &title, registry.as_ref())?;
        guard_task_lane(
            registry.as_ref(),
            task.lane.as_deref(),
            lane,
            &title,
            "done",
        )?;
        let state = checkbox_state(&document.lines[task.start].text).ok_or_else(|| {
            anyhow::anyhow!(
                "TASKS.md line {} is not a `- [ ]` task line",
                task.start + 1
            )
        })?;
        if state == 'x' || state == 'X' {
            return Ok(format!(
                "\"{}\" in \"{}\" is already done; nothing was changed",
                task.title,
                task.display_list(registry.as_ref())
            ));
        }
        if state != ' ' {
            bail!(
                "TASKS.md line {} has an unknown checkbox `[{state}]`; fix the line by hand first",
                task.start + 1
            );
        }
        let indent = document.lines[task.start].text.len()
            - document.lines[task.start].text.trim_start().len();
        document.lines[task.start]
            .text
            .replace_range(indent + 3..indent + 4, "x");
        Ok(format!(
            "done \"{}\" in \"{}\"",
            task.title,
            task.display_list(registry.as_ref())
        ))
    })
}

/// `hp task delete`: remove the uniquely titled task's whole block — its line
/// and its indented notes — and no other byte. An empty section keeps its
/// heading; removing that would be a second edit the caller did not ask for.
pub fn delete(project: &Project, title: &str, lane: Option<&str>) -> Result<String> {
    let title = clean_title(title)?;
    let registry = Registry::load(project);
    with_tasks(project, |document| {
        let tasks = located_tasks(&document.lines, registry.as_ref());
        let task = unique_task(&tasks, &title, registry.as_ref())?;
        guard_task_lane(registry.as_ref(), task.lane.as_deref(), lane, &title, "delete")?;
        let display = task.display_list(registry.as_ref());
        let removed = task.end - task.start;
        document.lines.drain(task.start..task.end);
        Ok(format!("deleted \"{title}\" from \"{display}\" ({removed} line(s))"))
    })
}

/// What `hp task thread` writes onto the task line.
#[derive(Debug, Clone, Default)]
pub struct ThreadOptions<'a> {
    /// The thread id to record (`t-0012`); `None` with `clear` removes it.
    pub thread: Option<&'a str>,
    /// `--owner`: set or replace the owner in the same write. `None` keeps
    /// whatever owner the line already has.
    pub owner: Option<&'a str>,
    /// `--clear`: remove the thread id instead of setting one.
    pub clear: bool,
}

/// `hp task thread`: record the thread that runs a task, optionally setting
/// its owner in the same locked write, and touch only that task line. A
/// different existing id is refused rather than silently replaced — one task
/// must not point at two threads; `--clear` removes the old id first.
pub fn record_thread(project: &Project, title: &str, options: &ThreadOptions<'_>, lane: Option<&str>) -> Result<String> {
    let title = clean_title(title)?;
    let wanted = options.thread.map(str::trim).filter(|id| !id.is_empty());
    if options.clear && wanted.is_some() {
        bail!("pass a thread id or `--clear`, not both");
    }
    if wanted.is_none() && !options.clear {
        bail!("pass the thread id (`t-0012`) or `--clear` to remove it");
    }
    if let Some(id) = wanted && !is_thread_id(id) {
        bail!("thread ids look like `t-0012`; got `{id}`");
    }
    let new_owner = options.owner.map(Owner::parse).filter(|owner| *owner != Owner::Unassigned);
    let registry = Registry::load(project);
    with_tasks(project, |document| {
        let tasks = located_tasks(&document.lines, registry.as_ref());
        let task = unique_task(&tasks, &title, registry.as_ref())?;
        guard_task_lane(registry.as_ref(), task.lane.as_deref(), lane, &title, "thread")?;
        let line = document.lines[task.start].text.clone();
        let (prefix, task_title, old_owner, old_thread) = task_line_parts(&line).ok_or_else(|| {
            anyhow::anyhow!("TASKS.md line {} is not a `- [ ]` task line", task.start + 1)
        })?;
        let owner = match &new_owner {
            Some(owner) => Some(owner.to_string()).filter(|text| !text.is_empty()),
            None => old_owner.clone(),
        };
        let thread = if options.clear { None } else { wanted.map(str::to_string) };
        if let (Some(old), Some(new)) = (&old_thread, &thread)
            && old != new
            && !options.clear
        {
            bail!(
                "task \"{}\" already records thread {old}; clear it with `hp task thread {} \"{}\" --clear` first, then record {new}",
                task.title, project.slug, task.title
            );
        }
        if old_thread == thread && owner == old_owner {
            return Ok(format!(
                "\"{}\" in \"{}\" already records {}; nothing was changed",
                task.title,
                task.display_list(registry.as_ref()),
                match &thread {
                    Some(id) => format!("thread {id}"),
                    None => "no thread".to_string(),
                }
            ));
        }
        let mut rebuilt = format!("{prefix} {task_title}");
        if let Some(owner) = owner.filter(|owner| !owner.is_empty()) {
            rebuilt.push_str(&format!(" ({owner})"));
        }
        if let Some(id) = &thread {
            rebuilt.push_str(&format!(" · {id}"));
        }
        document.lines[task.start].text = rebuilt;
        Ok(match (&thread, options.clear) {
            (Some(id), _) => format!("recorded thread {id} on \"{}\" in \"{}\"", task.title, task.display_list(registry.as_ref())),
            (None, true) => format!("cleared the thread on \"{}\" in \"{}\"", task.title, task.display_list(registry.as_ref())),
            _ => format!("updated \"{}\" in \"{}\"", task.title, task.display_list(registry.as_ref())),
        })
    })
}

/// The bytes before a task's title, its raw title, and the owner text and
/// thread id already on the line. Raw owner text is kept so a write that does
/// not pass `--owner` cannot quietly normalise an existing `(agent → t-0007)`.
fn task_line_parts(line: &str) -> Option<(String, String, Option<String>, Option<String>)> {
    if !is_task_line(line) {
        return None;
    }
    let start = line.len() - line.trim_start().len();
    let after = &line[start..];
    let close = after.find(']')?;
    let body = line[start + close + 1..].trim_start();
    let mut thread = None;
    let (head, explicit) = match body.rsplit_once('·') {
        Some((head, id)) if is_thread_id(id.trim()) => (head.trim_end(), Some(id.trim().to_string())),
        _ => (body, None),
    };
    thread = thread.or(explicit);
    let (title, owner) = match head.rfind('(').filter(|_| head.ends_with(')')) {
        Some(open) => {
            let inner = head[open + 1..head.len() - 1].trim();
            match inner.split_once('→') {
                Some((who, id)) if is_thread_id(id.trim()) => {
                    if thread.is_none() {
                        thread = Some(id.trim().to_string());
                    }
                    (head[..open].trim_end().to_string(), Some(who.trim().to_string()).filter(|text| !text.is_empty()))
                }
                _ => (head[..open].trim_end().to_string(), Some(inner.to_string()).filter(|text| !text.is_empty())),
            }
        }
        None => (head.trim_end().to_string(), None),
    };
    Some((line[..start + close + 1].to_string(), title, owner, thread))
}

fn clean_title(title: &str) -> Result<String> {
    let title = title.trim();
    if title.is_empty() {
        bail!("the task title may not be empty");
    }
    if title.contains(['\n', '\r']) {
        bail!("the task title may not contain a line break");
    }
    Ok(title.to_string())
}

fn clean_list(list: &str) -> Result<&str> {
    let list = list.trim();
    if list.is_empty() {
        bail!("the list name may not be empty");
    }
    if list.contains(['\n', '\r']) {
        bail!("the list name may not contain a line break");
    }
    Ok(list)
}

/// Where one write goes: the lane id (`None` means shared), the list name
/// without the lane prefix, and the heading text to create when missing.
struct Target {
    lane: Option<String>,
    list: String,
    heading: String,
}

fn add_target(registry: Option<&Registry>, list: &str, lane: Option<&str>) -> Result<Target> {
    let Some(registry) = registry else {
        if lane.is_some() {
            bail!(
                "this project has no `.state/lanes.json`, so it has no lanes to pass `--lane`; leave it out to write a shared list"
            );
        }
        return Ok(Target {
            lane: None,
            list: list.to_string(),
            heading: list.to_string(),
        });
    };
    if let Some(handle) = lane {
        let lane = lane_by_handle(registry, handle)?;
        if !lane.is_open() {
            bail!(
                "lane `{}` ({}) is closed; `hp lane open {}` reopens it before tasks are added",
                lane.label,
                lane.id,
                lane.name
            );
        }
        if let Some((prefix, rest)) = list.split_once('/')
            && unique_lane(registry, prefix.trim()).is_some()
        {
            bail!(
                "the list `{list}` already names a lane; with `--lane` give just the list name (`{}`)",
                rest.trim()
            );
        }
        return Ok(Target {
            lane: Some(lane.id.clone()),
            list: list.to_string(),
            heading: format!("{}/{}", heading_prefix(registry, lane), list),
        });
    }
    if let Some((prefix, rest)) = list.split_once('/')
        && unique_lane(registry, prefix.trim()).is_some()
    {
        bail!(
            "the list `{list}` is a lane's list; `hp task add` writes it with `--lane {} ... {}`",
            prefix.trim(),
            rest.trim()
        );
    }
    Ok(Target {
        lane: None,
        list: list.to_string(),
        heading: list.to_string(),
    })
}

fn move_target(registry: Option<&Registry>, list: &str, sections: &[Section], caller: Option<&str>) -> Result<Target> {
    let Some(registry) = registry else {
        return Ok(Target {
            lane: None,
            list: list.to_string(),
            heading: list.to_string(),
        });
    };
    if let Some((prefix, rest)) = list.split_once('/')
        && let Some(lane) = unique_lane(registry, prefix.trim())
    {
        if !lane.is_open() {
            bail!(
                "lane `{}` ({}) is closed; `hp lane open {}` reopens it before tasks are moved into it",
                lane.label,
                lane.id,
                lane.name
            );
        }
        return Ok(Target {
            lane: Some(lane.id.clone()),
            list: rest.trim().to_string(),
            heading: format!("{}/{}", heading_prefix(registry, lane), rest.trim()),
        });
    }
    // A bare list name means the shared list of that name. If no shared list
    // exists but the caller's own lane has one, that is the natural target
    // (`move "Fix login" Doing` inside lane c-1); the cross-lane guard still
    // runs, so this can never pick another lane's list.
    let shared = sections.iter().any(|section| section.lane.is_none() && section.list == list);
    if !shared
        && let Some(caller) = caller
        && let Some(section) = sections.iter().find(|section| section.lane.as_deref() == Some(caller) && section.list == list)
    {
        return Ok(Target {
            lane: Some(caller.to_string()),
            list: list.to_string(),
            heading: section.display(Some(registry)),
        });
    }
    Ok(Target {
        lane: None,
        list: list.to_string(),
        heading: list.to_string(),
    })
}

fn lane_label(lane: &Lane) -> &str {
    if lane.label.is_empty() {
        &lane.name
    } else {
        &lane.label
    }
}

/// The prefix a newly created heading uses. The label is the readable form
/// (the one §8 shows), but when another lane also uses it as its label or
/// name, the unique lane name is written instead so the heading can never
/// resolve to several lanes (and silently become a shared list).
fn heading_prefix(registry: &Registry, lane: &Lane) -> String {
    let label = lane_label(lane);
    if unique_lane(registry, label).is_some_and(|found| found.id == lane.id) {
        label.to_string()
    } else {
        lane.name.clone()
    }
}

fn lane_by_handle<'a>(registry: &'a Registry, handle: &str) -> Result<&'a Lane> {
    let handle = handle.trim();
    if handle.is_empty() {
        bail!("`--lane` needs a lane name or id");
    }
    if let Some(lane) = registry.get(handle) {
        return Ok(lane);
    }
    let named: Vec<&Lane> = registry
        .lanes
        .iter()
        .filter(|lane| lane.name == handle)
        .collect();
    if named.len() == 1 {
        return Ok(named[0]);
    }
    let labelled: Vec<&Lane> = registry
        .lanes
        .iter()
        .filter(|lane| lane.label == handle)
        .collect();
    match labelled.len() {
        1 => Ok(labelled[0]),
        0 => bail!("no lane `{handle}` in this project; `hp lane list` shows them"),
        _ => bail!(
            "`--lane {handle}` matches several lanes' labels ({}); use the lane name or id",
            labelled
                .iter()
                .map(|lane| format!("{} ({})", lane.label, lane.id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn guard_task_lane(
    registry: Option<&Registry>,
    task_lane: Option<&str>,
    caller: Option<&str>,
    title: &str,
    verb: &str,
) -> Result<()> {
    let Some(lane_id) = task_lane else {
        return Ok(());
    };
    if Some(lane_id) == caller {
        return Ok(());
    }
    let Some(registry) = registry else {
        return Ok(());
    };
    let label = registry.get(lane_id).map(lane_label).unwrap_or(lane_id);
    let run_as = caller
        .and_then(|id| registry.get(id))
        .map(|lane| {
            format!(
                "this command runs as lane `{}` ({})",
                lane_label(lane),
                lane.id
            )
        })
        .unwrap_or_else(|| "this command runs outside a lane".to_string());
    bail!(
        "task \"{title}\" belongs to lane `{label}` ({lane_id}); a coordinator writes only its own lane's lists ({run_as}), so `hp task {verb}` is refused — run it from that lane's coordinator, or ask the user"
    );
}

fn guard_target_lane(
    registry: Option<&Registry>,
    target_lane: Option<&str>,
    caller: Option<&str>,
    list: &str,
) -> Result<()> {
    let Some(lane_id) = target_lane else {
        return Ok(());
    };
    if Some(lane_id) == caller {
        return Ok(());
    }
    let Some(registry) = registry else {
        return Ok(());
    };
    let label = registry.get(lane_id).map(lane_label).unwrap_or(lane_id);
    let run_as = caller
        .and_then(|id| registry.get(id))
        .map(|lane| {
            format!(
                "this command runs as lane `{}` ({})",
                lane_label(lane),
                lane.id
            )
        })
        .unwrap_or_else(|| "this command runs outside a lane".to_string());
    bail!(
        "list \"{list}\" belongs to lane `{label}` ({lane_id}); a coordinator writes only its own lane's lists ({run_as}), so moving a task there is refused"
    );
}

fn task_block(
    title: &str,
    owner: Option<&Owner>,
    notes: Option<&str>,
    newline: &str,
) -> Vec<RawLine> {
    let mut line = format!("- [ ] {title}");
    if let Some(owner) = owner {
        line.push_str(&format!(" ({owner})"));
    }
    let mut lines = vec![RawLine {
        text: line,
        ending: newline.to_string(),
    }];
    if let Some(notes) = notes {
        for note in notes.lines() {
            lines.push(RawLine {
                text: if note.is_empty() {
                    String::new()
                } else {
                    format!("  {note}")
                },
                ending: newline.to_string(),
            });
        }
    }
    lines
}

fn with_tasks<T>(project: &Project, change: impl FnOnce(&mut Document) -> Result<T>) -> Result<T> {
    let _lock = project.lock()?;
    let path = project.dir().join("TASKS.md");
    // Read *inside* the lock: a write never starts from a copy read before it.
    let original = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => bail!("could not read {}: {error}", path.display()),
    };
    let mut document = Document::parse(&original);
    let answer = change(&mut document)?;
    let mut updated = document.render();
    // Preserve a missing final newline when the write appended at EOF: the
    // separator before the new content still has to be added, but the file's
    // own "does it end with a newline" property is part of its bytes.
    if !original.trim().is_empty() && !original.ends_with('\n') && updated.ends_with('\n') {
        if updated.ends_with("\r\n") {
            updated.truncate(updated.len() - 2);
        } else {
            updated.pop();
        }
    }
    if updated != original {
        crate::project::write_atomic(&path, updated.as_bytes())?;
    }
    Ok(answer)
}

/// One physical line with its exact terminator, so CRLF files stay CRLF and a
/// last line without `\n` stays without one when it is not the line changed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawLine {
    text: String,
    ending: String,
}

#[derive(Debug, Clone)]
struct Document {
    lines: Vec<RawLine>,
}

impl Document {
    fn parse(text: &str) -> Document {
        let mut lines = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            if let Some(at) = rest.find('\n') {
                let chunk = &rest[..=at];
                let (text, ending) = match chunk.strip_suffix("\r\n") {
                    Some(text) => (text, "\r\n"),
                    None => (chunk.strip_suffix('\n').expect("found above"), "\n"),
                };
                lines.push(RawLine {
                    text: text.to_string(),
                    ending: ending.to_string(),
                });
                rest = &rest[at + 1..];
            } else {
                lines.push(RawLine {
                    text: rest.to_string(),
                    ending: String::new(),
                });
                break;
            }
        }
        Document { lines }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(&line.text);
            out.push_str(&line.ending);
        }
        out
    }

    fn newline(&self) -> String {
        self.lines
            .iter()
            .find(|line| !line.ending.is_empty())
            .map(|line| line.ending.clone())
            .unwrap_or_else(|| "\n".to_string())
    }

    fn blank(&self) -> bool {
        self.lines.iter().all(|line| line.text.trim().is_empty())
    }

    fn start_skeleton(&mut self, heading: &str) {
        let newline = self.newline();
        self.lines = vec![
            RawLine {
                text: "# Tasks".into(),
                ending: newline.clone(),
            },
            RawLine {
                text: String::new(),
                ending: newline.clone(),
            },
            RawLine {
                text: format!("## {heading}"),
                ending: newline,
            },
        ];
    }

    fn append_section(&mut self, heading: &str) {
        let newline = self.newline();
        if self.lines.is_empty() {
            self.start_skeleton(heading);
            return;
        }
        let last = self.lines.len() - 1;
        if self.lines[last].ending.is_empty() {
            self.lines[last].ending = newline.clone();
        }
        if !self.lines[last].text.trim().is_empty() {
            self.lines.push(RawLine {
                text: String::new(),
                ending: newline.clone(),
            });
        }
        self.lines.push(RawLine {
            text: format!("## {heading}"),
            ending: newline,
        });
    }

    fn sections(&self, registry: Option<&Registry>) -> Vec<Section> {
        let mut sections = Vec::new();
        let mut i = 0;
        while i < self.lines.len() {
            if let Some(heading) = self.lines[i].text.strip_prefix("## ") {
                let (lane, list) = heading_owner(heading, registry);
                let mut end = i + 1;
                while end < self.lines.len() && !self.lines[end].text.starts_with("## ") {
                    end += 1;
                }
                sections.push(Section {
                    heading: i,
                    end,
                    lane,
                    list,
                });
                i = end;
            } else {
                i += 1;
            }
        }
        sections
    }

    fn insert_at_section_end(&mut self, section: &Section, block: Vec<RawLine>) {
        let mut at = section.heading + 1;
        for i in section.heading + 1..section.end {
            if !self.lines[i].text.trim().is_empty() {
                at = i + 1;
            }
        }
        self.insert_lines(at, block);
    }

    fn insert_lines(&mut self, at: usize, block: Vec<RawLine>) {
        let newline = self.newline();
        if at > 0 && self.lines[at - 1].ending.is_empty() {
            self.lines[at - 1].ending = newline.clone();
        }
        let block_len = block.len();
        self.lines.splice(at..at, block);
        // Only the final line of the file may have no terminator. A block
        // moved from the end therefore gets one when it lands in the middle.
        let last = at + block_len - 1;
        if last + 1 < self.lines.len() && self.lines[last].ending.is_empty() {
            self.lines[last].ending = newline;
        }
    }
}

#[derive(Debug, Clone)]
struct Section {
    heading: usize,
    end: usize,
    lane: Option<String>,
    list: String,
}

impl Section {
    fn display(&self, registry: Option<&Registry>) -> String {
        match (&self.lane, registry) {
            (Some(id), Some(registry)) => {
                let label = registry.get(id).map(lane_label).unwrap_or(id);
                format!("{label}/{}", self.list)
            }
            _ => self.list.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct LocatedTask {
    start: usize,
    end: usize,
    lane: Option<String>,
    list: String,
    title: String,
}

impl LocatedTask {
    fn display_list(&self, registry: Option<&Registry>) -> String {
        match (&self.lane, registry) {
            (Some(id), Some(registry)) => {
                let label = registry.get(id).map(lane_label).unwrap_or(id);
                format!("{label}/{}", self.list)
            }
            _ => self.list.clone(),
        }
    }
}

fn located_tasks(lines: &[RawLine], registry: Option<&Registry>) -> Vec<LocatedTask> {
    let mut tasks = Vec::new();
    let mut lane: Option<String> = None;
    let mut list = String::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(heading) = lines[i].text.strip_prefix("## ") {
            (lane, list) = heading_owner(heading, registry);
            i += 1;
            continue;
        }
        if !is_task_line(&lines[i].text) {
            i += 1;
            continue;
        }
        let rest = lines[i].text.trim_start()[5..].trim();
        let (title, _, _) = split_line(rest);
        let end = block_end(lines, i);
        tasks.push(LocatedTask {
            start: i,
            end,
            lane: lane.clone(),
            list: list.clone(),
            title,
        });
        i = end;
    }
    tasks
}

fn block_end(lines: &[RawLine], index: usize) -> usize {
    let base = indent(&lines[index].text);
    let mut end = index + 1;
    let mut last = index;
    while end < lines.len() {
        let text = &lines[end].text;
        if text.trim().is_empty() {
            end += 1;
            continue;
        }
        if indent(text) <= base || is_task_line(text) || text.starts_with('#') {
            break;
        }
        last = end;
        end += 1;
    }
    last + 1
}

fn unique_task<'a>(
    tasks: &'a [LocatedTask],
    title: &str,
    registry: Option<&Registry>,
) -> Result<&'a LocatedTask> {
    let wanted = title.to_lowercase();
    let found: Vec<&LocatedTask> = tasks.iter().filter(|task| task.title.to_lowercase() == wanted).collect();
    match found.len() {
        1 => Ok(found[0]),
        0 => {
            let wanted = title.to_lowercase();
            let mut close: Vec<&LocatedTask> = tasks
                .iter()
                .filter(|task| {
                    task.title.to_lowercase().contains(&wanted)
                        || wanted.contains(&task.title.to_lowercase())
                })
                .collect();
            if close.is_empty() {
                close = tasks.iter().collect();
            }
            let known: Vec<String> = close
                .iter()
                .take(12)
                .map(|task| format!("\"{}\" ({})", task.title, task.display_list(registry)))
                .collect();
            if known.is_empty() {
                bail!("TASKS.md has no task titled \"{title}\"")
            }
            bail!(
                "TASKS.md has no task titled \"{title}\"; known titles: {}",
                known.join(", ")
            )
        }
        _ => {
            let mut places: Vec<String> = found
                .iter()
                .map(|task| {
                    format!(
                        "\"{}\" in \"{}\" (line {})",
                        task.title,
                        task.display_list(registry),
                        task.start + 1
                    )
                })
                .collect();
            places.sort();
            bail!(
                "TASKS.md has {} tasks titled \"{title}\": {}; `hp task` needs a unique title",
                found.len(),
                places.join(", ")
            )
        }
    }
}

fn checkbox_state(text: &str) -> Option<char> {
    let bytes = text.trim_start().as_bytes();
    if bytes.len() >= 5
        && bytes[0] == b'-'
        && bytes[1] == b' '
        && bytes[2] == b'['
        && bytes[4] == b']'
    {
        Some(bytes[3] as char)
    } else {
        None
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "# Tasks\n\n## Backlog\n- [ ] Write the docs (me)\n- [ ] Fix login (agent → t-0007)\n  Users on Safari get logged out.\n\n  Bugs:\n    - cookie is dropped\n  See https://example.com/issue/4\n- [ ] Plain line\n\n## Later\n- [x] Old (agent)\n  - [ ] A subtask stays a task\n";

    fn agent(profile: Option<&str>, machine: Option<&str>) -> Owner {
        Owner::Agent { profile: profile.map(str::to_string), machine: machine.map(str::to_string) }
    }

    #[test]
    fn one_line_tasks_keep_parsing_as_before() {
        let tasks = parse("# Tasks\n\n## Backlog\n- [ ] Write the docs (me)\n- [ ] Plain line\n");
        assert_eq!(tasks.len(), 2);
        assert_eq!((tasks[0].title.as_str(), &tasks[0].owner, tasks[0].description.as_str()), ("Write the docs", &Owner::Me, ""));
        assert_eq!(tasks[1].owner, Owner::Unassigned);
    }

    #[test]
    fn owners_are_a_profile_a_machine_or_both_and_the_thread_stands_apart() {
        let tasks = parse("## A\n- [ ] One (codex-fast)\n- [ ] Two (@m1)\n- [ ] Three (codex-fast@m1) · t-0007\n- [ ] Four (me)\n- [ ] Five\n- [ ] Six (Priya Rao)\n- [ ] Fix (the) login (claude)\n- [ ] Unassigned but running · t-0009\n");
        let owners: Vec<&Owner> = tasks.iter().map(|t| &t.owner).collect();
        assert_eq!(owners, [&agent(Some("codex-fast"), None), &agent(None, Some("m1")), &agent(Some("codex-fast"), Some("m1")), &Owner::Me, &Owner::Unassigned, &Owner::Person("Priya Rao".into()), &agent(Some("claude"), None), &Owner::Unassigned]);
        assert_eq!(tasks[2].thread.as_deref(), Some("t-0007"));
        assert_eq!(tasks[2].title, "Three");
        assert_eq!(tasks[6].title, "Fix (the) login");
        assert_eq!((tasks[7].title.as_str(), tasks[7].thread.as_deref()), ("Unassigned but running", Some("t-0009")));
        let shown: Vec<String> = tasks.iter().map(|t| t.owner.to_string()).collect();
        assert_eq!(shown, ["codex-fast", "@m1", "codex-fast@m1", "me", "", "Priya Rao", "claude", ""]);
        assert!(matches!(Owner::parse("a@"), Owner::Person(_)));
        assert!(matches!(Owner::parse("@"), Owner::Person(_)));
        assert!(matches!(Owner::parse("a@b@c"), Owner::Person(_)));
    }

    #[test]
    fn old_agent_owners_still_read() {
        let tasks = parse("## A\n- [ ] Old (agent)\n- [ ] Running (agent → t-0007)\n");
        assert_eq!(tasks[0].owner, Owner::Unassigned);
        assert_eq!((&tasks[1].owner, tasks[1].thread.as_deref()), (&agent(None, None), Some("t-0007")));
        assert_eq!(tasks[1].owner.to_string(), "default profile");
    }

    #[test]
    fn delegation_takes_profile_and_machine_from_the_owner() {
        let tasks = parse("## A\n- [ ] P (fast)\n- [ ] M (@m1)\n- [ ] PM (fast@m1)\n- [ ] U\n- [ ] Me (me)\n- [ ] X (Bob Smith)\n");
        let s = |v: &str| Some(v.to_string());
        assert_eq!(launch_for(&tasks[0], None, None).unwrap(), (s("fast"), None));
        assert_eq!(launch_for(&tasks[1], s("deep"), None).unwrap(), (s("deep"), s("m1")));
        assert_eq!(launch_for(&tasks[2], None, s("m1")).unwrap(), (s("fast"), s("m1")));
        assert_eq!(launch_for(&tasks[3], s("deep"), s("m2")).unwrap(), (s("deep"), s("m2")));
        assert!(launch_for(&tasks[0], s("deep"), None).unwrap_err().to_string().contains("assigned to profile `fast`"));
        assert!(launch_for(&tasks[0], None, s("m1")).unwrap_err().to_string().contains("assigned to this machine"));
        assert!(launch_for(&tasks[2], None, s("m2")).is_err());
        assert!(launch_for(&tasks[4], None, None).is_err());
        assert!(launch_for(&tasks[5], None, None).unwrap_err().to_string().contains("a person"));
    }

    #[test]
    fn a_bare_name_that_is_no_profile_is_a_person() {
        let known = |p: &str| p == "claude";
        assert!(Owner::parse("Priya").is_person(known));
        assert!(Owner::parse("Priya Rao").is_person(known));
        assert!(!Owner::parse("claude").is_person(known));
        assert!(!Owner::parse("Priya@m1").is_person(known));
        assert!(!Owner::parse("@m1").is_person(known));
        assert!(!Owner::parse("me").is_person(known));
        assert!(!Owner::parse("").is_person(known));
        let tasks = parse("## A\n- [ ] Call the bank (Elias)\n");
        assert_eq!(tasks[0].owner.to_string(), "Elias");
    }

    #[test]
    fn indented_lines_under_a_task_are_its_description() {
        let tasks = parse(TEXT);
        assert_eq!(tasks.len(), 5);
        assert_eq!(tasks[1].thread.as_deref(), Some("t-0007"));
        assert_eq!(tasks[1].description, "Users on Safari get logged out.\n\nBugs:\n  - cookie is dropped\nSee https://example.com/issue/4");
        assert_eq!(tasks[2].description, "");
        assert_eq!(tasks[3].list, "Later");
        assert_eq!(tasks[4].title, "A subtask stays a task");
        assert_eq!(tasks[3].description, "");
    }

    #[test]
    fn a_blank_line_ends_the_description_when_nothing_indented_follows() {
        let tasks = parse("## A\n- [ ] One\n  note\n\nloose text\n- [ ] Two\n");
        assert_eq!(tasks[0].description, "note");
        assert_eq!(tasks[1].description, "");
    }

    #[test]
    fn compact_folds_each_description_into_one_line() {
        let text = compact(TEXT);
        assert!(text.contains("- [ ] Fix login (agent → t-0007)\n  notes: Users on Safari get logged out. (+3 more lines in TASKS.md)\n- [ ] Plain line"), "{text}");
        assert!(!text.contains("cookie"));
        assert!(text.contains("- [ ] Write the docs (me)\n- [ ] Fix login"));
        assert_eq!(compact("## B\n- [ ] One\n"), "## B\n- [ ] One");
        let long = compact(&format!("- [ ] T\n  {}\n", "x".repeat(80)));
        assert!(long.ends_with(&format!("notes: {}…", "x".repeat(60))), "{long}");
    }

    #[test]
    fn delegation_appends_the_description_to_the_task() {
        let task = delegated(TEXT, "fix login", "Fix the Safari logout.\n").unwrap();
        assert_eq!(task, "Fix the Safari logout.\n\n## Notes from the task list\n\nUsers on Safari get logged out.\n\nBugs:\n  - cookie is dropped\nSee https://example.com/issue/4\n");
        assert_eq!(delegated(TEXT, "Plain line", "Do it.").unwrap(), "Do it.");
        assert!(delegated(TEXT, "Nope", "x").is_err());
    }
    // ------------------------------------------------------- lane headings

    fn temp_project(name: &str) -> (tempfile::TempDir, Project) {
        let home = tempfile::tempdir().unwrap();
        let project = crate::project::create(&home.path().join("root"), name, "", vec![]).unwrap();
        (home, project)
    }

    fn two_lanes(project: &Project) -> Registry {
        let registry = Registry {
            lanes: vec![
                Lane { id: "c-1".into(), name: "fix".into(), label: "抢修".into(), ..Default::default() },
                Lane { id: "c-2".into(), name: "books".into(), label: "收藏".into(), ..Default::default() },
            ],
            default: "c-1".into(),
            next: 3,
        };
        registry.save(project).unwrap();
        registry
    }

    fn write_tasks(project: &Project, text: &str) {
        std::fs::write(project.dir().join("TASKS.md"), text).unwrap();
    }

    fn read_tasks(project: &Project) -> String {
        std::fs::read_to_string(project.dir().join("TASKS.md")).unwrap_or_default()
    }

    #[test]
    fn heading_prefixes_match_a_lane_name_or_label_and_closed_lanes_stay_attributed() {
        let (_home, project) = temp_project("demo");
        let mut registry = two_lanes(&project);
        registry.lanes.push(Lane { id: "c-3".into(), name: "old".into(), label: "归档".into(), status: crate::lanes::Status::Closed, ..Default::default() });
        registry.save(&project).unwrap();
        let text = "# Tasks\n\n## Backlog\n- [ ] Shared\n\n## 抢修/Doing\n- [ ] By label\n\n## books/Later\n- [ ] By name\n\n## 归档/Old\n- [ ] Closed lane\n\n## unknown/Thing\n- [ ] Unknown prefix\n";
        let tasks = parse_with(text, Some(&registry));
        let got: Vec<(&str, Option<&str>, &str)> = tasks.iter().map(|task| (task.title.as_str(), task.lane.as_deref(), task.list.as_str())).collect();
        assert_eq!(
            got,
            [
                ("Shared", None, "Backlog"),
                ("By label", Some("c-1"), "抢修/Doing"),
                ("By name", Some("c-2"), "books/Later"),
                ("Closed lane", Some("c-3"), "归档/Old"),
                ("Unknown prefix", None, "unknown/Thing"),
            ]
        );
        // `parse` without a registry keeps the old shape: no lane, headings
        // with a slash are just list names.
        let plain = parse(text);
        assert!(plain.iter().all(|task| task.lane.is_none()));
        assert_eq!(plain[1].list, "抢修/Doing");
    }

    #[test]
    fn colliding_labels_leave_the_heading_shared_and_ambiguous_lane_flags_refuse() {
        let (_home, project) = temp_project("demo");
        let registry = Registry {
            lanes: vec![
                Lane { id: "c-1".into(), name: "one".into(), label: "同名".into(), ..Default::default() },
                Lane { id: "c-2".into(), name: "two".into(), label: "同名".into(), ..Default::default() },
            ],
            default: "c-1".into(),
            next: 3,
        };
        registry.save(&project).unwrap();
        let tasks = parse_with("# Tasks\n\n## 同名/Backlog\n- [ ] Which lane?\n", Some(&registry));
        assert_eq!((tasks[0].lane.as_deref(), tasks[0].list.as_str()), (None, "同名/Backlog"), "a label collision must not pick a lane");
        write_tasks(&project, "# Tasks\n\n## Backlog\n");
        let error = add(&project, "Backlog", "T", &AddOptions { lane: Some("同名"), ..Default::default() }).unwrap_err().to_string();
        assert!(error.contains("matches several lanes' labels"), "{error}");
        // By unique name it works, and the new heading uses the name because
        // the readable label is ambiguous.
        add(&project, "Backlog", "By name", &AddOptions { lane: Some("one"), ..Default::default() }).unwrap();
        assert!(read_tasks(&project).contains("## one/Backlog\n- [ ] By name"), "{}", read_tasks(&project));
    }

    #[test]
    fn compact_for_prints_shared_and_only_the_readers_lane_and_hides_closed_lanes() {
        let (_home, project) = temp_project("demo");
        let mut registry = two_lanes(&project);
        registry.lanes.push(Lane { id: "c-3".into(), name: "old".into(), label: "归档".into(), status: crate::lanes::Status::Closed, ..Default::default() });
        registry.save(&project).unwrap();
        let text = "# Tasks\n\n## Backlog\n- [ ] Shared task\n\n## 抢修/Doing\n- [ ] Fix the leak\n\n## 收藏/Backlog\n- [ ] Sort the books\n\n## 归档/Backlog\n- [ ] Old archived task\n";
        let one = compact_for(text, &registry, "c-1");
        assert!(one.contains("- [ ] Shared task") && one.contains("- [ ] Fix the leak"), "{one}");
        assert!(!one.contains("Sort the books") && !one.contains("Old archived task"), "{one}");
        assert!(!one.contains("## 收藏/Backlog") && !one.contains("## 归档/Backlog"), "{one}");
        let two = compact_for(text, &registry, "c-2");
        assert!(two.contains("- [ ] Shared task") && two.contains("- [ ] Sort the books"), "{two}");
        assert!(!two.contains("Fix the leak") && !two.contains("Old archived task"), "{two}");
        // Reopening the lane brings its section back without touching the file.
        assert_eq!(read_tasks(&project), "# Tasks\n\n## Backlog\n", "the fixture above is only in memory");
        let mut reopened = registry.clone();
        reopened.lanes[2].status = crate::lanes::Status::Open;
        assert!(compact_for(text, &reopened, "c-3").contains("- [ ] Old archived task"));
    }

    const DIRTY: &str = concat!(
        "# Tasks\n",
        "\n",
        "<!-- file comment: keep me -->\n",
        "\n",
        "## Backlog                 <!-- shared list -->\n",
        "- [ ] Write the docs (me)\n",
        "  Keep the tone.   \n",
        "\n",
        "- [ ] Fix login (agent → t-0007) · t-0007\n",
        "  Users on Safari get logged out.\n",
        "\n",
        "  Bugs:\n",
        "    - cookie is dropped\n",
        "\n",
        "## 抢修/Backlog\n",
        "- [ ] Rotate the leaked signing key (me)\n",
        "- [ ] Truncated log lines hide the 500 (codex-fast@m1) · t-0171\n",
        "  codex logs line 1\n",
        "  line 2\n",
        "\n",
        "## 抢修/Doing\n",
        "\n",
        "\n",
        "## 收藏/Backlog\n",
        "- [x] Old books\n",
        "\n",
        "## Archive/Odd\n",
        "  keep this tail without a task"
    );

    /// A real line diff over the two strings (no structural re-rendering): the
    /// only removed and added lines may be the ones the test names. A blank
    /// line, trailing space, CRLF or missing final newline elsewhere therefore
    /// fails the comparison even though `parse` would look identical.
    fn assert_only_changed(before: &str, after: &str, removed: &[&str], added: &[&str]) {
        let a: Vec<&str> = before.split('\n').collect();
        let b: Vec<&str> = after.split('\n').collect();
        let mut dp = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                dp[i][j] = if a[i] == b[j] { dp[i + 1][j + 1] + 1 } else { dp[i + 1][j].max(dp[i][j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        let mut removed_seen: Vec<&str> = Vec::new();
        let mut added_seen: Vec<&str> = Vec::new();
        while i < a.len() && j < b.len() {
            if a[i] == b[j] {
                i += 1;
                j += 1;
            } else if dp[i + 1][j] >= dp[i][j + 1] {
                removed_seen.push(a[i]);
                i += 1;
            } else {
                added_seen.push(b[j]);
                j += 1;
            }
        }
        removed_seen.extend(a[i..].iter().copied());
        added_seen.extend(b[j..].iter().copied());
        let sorted = |lines: &[&str]| {
            let mut out: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
            out.sort();
            out
        };
        assert_eq!(sorted(&removed_seen), sorted(removed), "removed lines differ\nbefore:\n{before}\nafter:\n{after}");
        assert_eq!(sorted(&added_seen), sorted(added), "added lines differ\nbefore:\n{before}\nafter:\n{after}");
    }

    #[test]
    fn add_preserves_every_other_byte() {
        let (_home, project) = temp_project("demo");
        two_lanes(&project);
        write_tasks(&project, DIRTY);
        let message = add(
            &project,
            "Backlog",
            "New task",
            &AddOptions { owner: Some("me"), lane: None, notes: Some("first note\nsecond note") },
        )
        .unwrap();
        assert!(message.contains("New task"), "{message}");
        let after = read_tasks(&project);
        assert_only_changed(DIRTY, &after, &[], &["- [ ] New task (me)", "  first note", "  second note"]);
        let new = after.find("- [ ] New task (me)").unwrap();
        assert!(after.find("## Backlog").unwrap() < new && new < after.find("## 抢修/Backlog").unwrap(), "{after}");
    }

    #[test]
    fn add_creates_a_missing_list_at_the_end_without_touching_other_bytes() {
        let (_home, project) = temp_project("demo");
        two_lanes(&project);
        write_tasks(&project, DIRTY);
        add(&project, "NewList", "Appended task", &AddOptions::default()).unwrap();
        let after = read_tasks(&project);
        assert_eq!(after, format!("{DIRTY}\n\n## NewList\n- [ ] Appended task"));
    }

    #[test]
    fn add_creates_the_skeleton_for_a_missing_or_empty_file() {
        let (_home, project) = temp_project("demo");
        std::fs::remove_file(project.dir().join("TASKS.md")).unwrap();
        add(&project, "Backlog", "First task", &AddOptions::default()).unwrap();
        assert_eq!(read_tasks(&project), "# Tasks\n\n## Backlog\n- [ ] First task\n");
        write_tasks(&project, "");
        add(&project, "Later", "Second task", &AddOptions::default()).unwrap();
        assert_eq!(read_tasks(&project), "# Tasks\n\n## Later\n- [ ] Second task\n");
    }

    #[test]
    fn move_preserves_every_other_byte() {
        let (_home, project) = temp_project("demo");
        two_lanes(&project);
        write_tasks(&project, DIRTY);
        let message = move_task(&project, "Fix login", "抢修/Doing", Some("c-1")).unwrap();
        assert!(message.contains("moved"), "{message}");
        let after = read_tasks(&project);
        let block = [
            "- [ ] Fix login (agent → t-0007) · t-0007",
            "  Users on Safari get logged out.",
            "",
            "  Bugs:",
            "    - cookie is dropped",
        ];
        assert_only_changed(DIRTY, &after, &block, &block);
        let moved = after.find("- [ ] Fix login").unwrap();
        assert!(after.find("## 抢修/Doing").unwrap() < moved && moved < after.find("## 收藏/Backlog").unwrap(), "{after}");
        // A bare list name resolves to the caller's own lane when no shared
        // list of that name exists: `Doing` is c-1's `## 抢修/Doing`.
        move_task(&project, "Write the docs", "Doing", Some("c-1")).unwrap();
        let after = read_tasks(&project);
        let moved = after.find("- [ ] Write the docs").unwrap();
        assert!(after.find("## 抢修/Doing").unwrap() < moved && moved < after.find("## 收藏/Backlog").unwrap(), "{after}");
    }

    #[test]
    fn done_preserves_every_other_byte() {
        let (_home, project) = temp_project("demo");
        two_lanes(&project);
        write_tasks(&project, DIRTY);
        let message = done(&project, "Write the docs", Some("c-1")).unwrap();
        assert!(message.contains("done"), "{message}");
        let after = read_tasks(&project);
        assert_only_changed(DIRTY, &after, &["- [ ] Write the docs (me)"], &["- [x] Write the docs (me)"]);
    }

    #[test]
    fn delete_preserves_every_other_byte() {
        let (_home, project) = temp_project("demo");
        two_lanes(&project);
        write_tasks(&project, DIRTY);
        delete(&project, "Old books", Some("c-2")).unwrap();
        let after = read_tasks(&project);
        assert_only_changed(DIRTY, &after, &["- [x] Old books"], &[]);
        assert!(after.contains("## 收藏/Backlog\n\n## Archive/Odd"), "an empty section keeps its heading: {after}");

        write_tasks(&project, DIRTY);
        delete(&project, "Truncated log lines hide the 500", Some("c-1")).unwrap();
        let after = read_tasks(&project);
        assert_only_changed(
            DIRTY,
            &after,
            &["- [ ] Truncated log lines hide the 500 (codex-fast@m1) · t-0171", "  codex logs line 1", "  line 2"],
            &[],
        );
        assert!(after.contains("- [ ] Rotate the leaked signing key (me)"), "the other task stays: {after}");
    }

    #[test]
    fn thread_preserves_every_other_byte() {
        let (_home, project) = temp_project("demo");
        two_lanes(&project);
        write_tasks(&project, DIRTY);
        let options = ThreadOptions { thread: Some("t-0012"), owner: Some("codex-fast"), clear: false };
        let message = record_thread(&project, "Write the docs", &options, Some("c-1")).unwrap();
        assert!(message.contains("t-0012"), "{message}");
        let after = read_tasks(&project);
        assert_only_changed(DIRTY, &after, &["- [ ] Write the docs (me)"], &["- [ ] Write the docs (codex-fast) · t-0012"]);

        // The same write again is a no-op; a different id is refused until the
        // old one is cleared.
        let once = read_tasks(&project);
        let again = record_thread(&project, "Write the docs", &options, Some("c-1")).unwrap();
        assert!(again.contains("already records"), "{again}");
        assert_eq!(read_tasks(&project), once);
        let other = ThreadOptions { thread: Some("t-0099"), owner: None, clear: false };
        let error = record_thread(&project, "Write the docs", &other, Some("c-1")).unwrap_err().to_string();
        assert!(error.contains("already records thread t-0012") && error.contains("--clear"), "{error}");
        assert_eq!(read_tasks(&project), once, "a refused thread write must not write");

        let cleared = ThreadOptions { thread: None, owner: None, clear: true };
        record_thread(&project, "Write the docs", &cleared, Some("c-1")).unwrap();
        let after = read_tasks(&project);
        assert_only_changed(&once, &after, &["- [ ] Write the docs (codex-fast) · t-0012"], &["- [ ] Write the docs (codex-fast)"]);
    }

    #[test]
    fn delete_and_thread_refuse_duplicate_titles() {
        let (_home, project) = temp_project("demo");
        two_lanes(&project);
        let text = "# Tasks\n\n## Backlog\n- [ ] Same title\n\n## 抢修/Backlog\n- [ ] Same title\n";
        write_tasks(&project, text);
        let error = delete(&project, "Same title", Some("c-1")).unwrap_err().to_string();
        assert!(error.contains("2 tasks titled"), "{error}");
        let options = ThreadOptions { thread: Some("t-0001"), owner: None, clear: false };
        let error = record_thread(&project, "Same title", &options, Some("c-1")).unwrap_err().to_string();
        assert!(error.contains("2 tasks titled"), "{error}");
        assert_eq!(read_tasks(&project), text, "a refused write must not write");
    }

    #[test]
    fn delete_and_thread_work_without_a_registry() {
        let (_home, project) = temp_project("demo");
        write_tasks(&project, "# Tasks\n\n## Backlog\n- [ ] One\n- [ ] Two\n");
        record_thread(&project, "Two", &ThreadOptions { thread: Some("t-0003"), owner: None, clear: false }, None).unwrap();
        assert!(read_tasks(&project).contains("- [ ] Two · t-0003"));
        delete(&project, "One", None).unwrap();
        assert!(!read_tasks(&project).contains("- [ ] One"));
        assert!(!project.state_dir().join("lanes.json").exists(), "hp task must not create a registry");
    }

    #[test]
    fn repeating_a_command_is_a_clear_error_or_a_no_op_never_a_second_copy() {
        let (_home, project) = temp_project("demo");
        two_lanes(&project);
        write_tasks(&project, DIRTY);
        add(&project, "Backlog", "Once", &AddOptions::default()).unwrap();
        let once = read_tasks(&project);
        let error = add(&project, "Backlog", "Once", &AddOptions::default()).unwrap_err().to_string();
        assert!(error.contains("already has a task"), "{error}");
        assert_eq!(read_tasks(&project), once, "a refused add must not write");

        move_task(&project, "Fix login", "抢修/Doing", Some("c-1")).unwrap();
        let moved = read_tasks(&project);
        let again = move_task(&project, "Fix login", "抢修/Doing", Some("c-1")).unwrap();
        assert!(again.contains("already in list"), "{again}");
        assert_eq!(read_tasks(&project), moved, "a repeated move must not write");

        done(&project, "Write the docs", Some("c-1")).unwrap();
        let ticked = read_tasks(&project);
        let again = done(&project, "Write the docs", Some("c-1")).unwrap();
        assert!(again.contains("already done"), "{again}");
        assert_eq!(read_tasks(&project), ticked, "a repeated done must not write");
    }

    #[test]
    fn move_refuses_duplicate_titles_and_lists_every_match() {
        let (_home, project) = temp_project("demo");
        two_lanes(&project);
        write_tasks(&project, "# Tasks\n\n## Backlog\n- [ ] Same title\n\n## 抢修/Backlog\n- [ ] Same title\n");
        let error = move_task(&project, "Same title", "Later", Some("c-1")).unwrap_err().to_string();
        assert!(error.contains("2 tasks titled"), "{error}");
        assert!(error.contains("Backlog") && error.contains("抢修/Backlog"), "{error}");
        assert_eq!(read_tasks(&project), "# Tasks\n\n## Backlog\n- [ ] Same title\n\n## 抢修/Backlog\n- [ ] Same title\n");
    }

    #[test]
    fn crlf_and_a_missing_final_newline_survive_a_write() {
        let (_home, project) = temp_project("demo");
        two_lanes(&project);
        let crlf = DIRTY.replace('\n', "\r\n");
        write_tasks(&project, &crlf);
        add(&project, "Backlog", "Crlf task", &AddOptions { notes: Some("note"), ..Default::default() }).unwrap();
        let after = read_tasks(&project);
        let inserted = "- [ ] Crlf task\r\n  note\r\n";
        assert!(after.contains(inserted), "{after:?}");
        assert_eq!(after.replacen(inserted, "", 1), crlf, "every other byte must be the CRLF original");
        assert!(!after.split("\r\n").any(|line| line.contains('\n')), "no line ending changed to bare LF");
    }

    #[test]
    fn two_adds_from_two_threads_keep_both_tasks() {
        let (_home, project) = temp_project("demo");
        write_tasks(&project, "# Tasks\n\n## Backlog\n");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let first = project.clone();
        let second = project.clone();
        let one = {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                add(&first, "Backlog", "First task", &AddOptions::default())
            })
        };
        let two = {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                add(&second, "Backlog", "Second task", &AddOptions::default())
            })
        };
        barrier.wait();
        one.join().unwrap().unwrap();
        two.join().unwrap().unwrap();
        let after = read_tasks(&project);
        assert!(after.contains("- [ ] First task") && after.contains("- [ ] Second task"), "{after}");
        assert_eq!(after.matches("- [ ]").count(), 2, "no update may be lost: {after}");
    }

    #[test]
    fn a_write_reads_again_inside_the_lock_so_an_external_edit_is_not_overwritten() {
        let (_home, project) = temp_project("demo");
        write_tasks(&project, "# Tasks\n\n## Backlog\n");
        // What a caller could have read before taking the lock.
        let baseline = read_tasks(&project);
        // An editor does not take `project.lock()`, so this lands in between.
        write_tasks(&project, &format!("{baseline}- [ ] Typed by hand\n"));
        add(&project, "Backlog", "From the command", &AddOptions::default()).unwrap();
        let after = read_tasks(&project);
        assert!(after.contains("- [ ] Typed by hand"), "the external edit must survive: {after}");
        assert!(after.contains("- [ ] From the command"), "{after}");
    }

    #[test]
    fn a_project_without_a_registry_keeps_every_list_shared_and_hp_task_usable() {
        let (_home, project) = temp_project("demo");
        assert!(!project.state_dir().join("lanes.json").exists());
        write_tasks(&project, DIRTY);
        // No registry: lane-looking headings are ordinary shared lists,
        // printed whole by `compact`, exactly as before commit 5.
        let digest = compact(&read_tasks(&project));
        assert!(digest.contains("## 抢修/Backlog") && digest.contains("- [ ] Rotate the leaked signing key (me)"), "{digest}");
        add(&project, "Backlog", "No lane task", &AddOptions::default()).unwrap();
        let after = read_tasks(&project);
        let tasks = parse(&after);
        assert!(tasks.iter().any(|task| task.title == "No lane task" && task.lane.is_none()));
        let error = add(&project, "Backlog", "Bad lane", &AddOptions { lane: Some("fix"), ..Default::default() }).unwrap_err().to_string();
        assert!(error.contains("no `.state/lanes.json`"), "{error}");
        assert!(!project.state_dir().join("lanes.json").exists(), "hp task must not create a registry");
    }

    #[test]
    fn add_writes_another_lane_only_when_the_lane_is_explicit_and_move_done_guard_it() {
        let (_home, project) = temp_project("demo");
        two_lanes(&project);
        write_tasks(&project, "# Tasks\n\n## Backlog\n- [ ] Shared task\n");
        add(&project, "Backlog", "Books task", &AddOptions { lane: Some("books"), ..Default::default() }).unwrap();
        let after = read_tasks(&project);
        assert!(after.contains("## 收藏/Backlog\n- [ ] Books task"), "{after}");
        let tasks = parse_with(&after, Some(&two_lanes(&project)));
        let books = tasks.iter().find(|task| task.title == "Books task").unwrap();
        assert_eq!((books.lane.as_deref(), books.list.as_str()), (Some("c-2"), "收藏/Backlog"));
        // A list that names a lane without --lane is refused rather than
        // quietly writing another lane's section.
        let error = add(&project, "收藏/Other", "Bad", &AddOptions::default()).unwrap_err().to_string();
        assert!(error.contains("--lane"), "{error}");
        // Moving/ticking another lane's task is refused for c-1's coordinator.
        let error = move_task(&project, "Books task", "Backlog", Some("c-1")).unwrap_err().to_string();
        assert!(error.contains("belongs to lane"), "{error}");
        assert!(done(&project, "Books task", Some("c-1")).unwrap_err().to_string().contains("belongs to lane"));
        // The owning lane may move it to a shared list.
        move_task(&project, "Books task", "Backlog", Some("c-2")).unwrap();
        assert!(!read_tasks(&project).contains("## 收藏/Backlog\n- [ ] Books task"));
    }

    #[test]
    fn from_task_finds_a_task_under_a_lane_heading() {
        let (_home, project) = temp_project("demo");
        let registry = two_lanes(&project);
        let text = "# Tasks\n\n## 抢修/Backlog\n- [ ] Rotate the leaked signing key (me)\n  Key id 42.\n";
        let found = find_in(text, "Rotate the leaked signing key", Some(&registry), Some("c-1")).unwrap();
        assert_eq!((found.lane.as_deref(), found.list.as_str()), (Some("c-1"), "抢修/Backlog"));
        let brief = delegated_from(&found, "Do it.\n");
        assert!(brief.contains("Key id 42."), "{brief}");
        // The old global path still works for unique and lane headings.
        assert!(delegated(text, "rotate the leaked SIGNING key", "Do it.\n").unwrap().contains("Key id 42."));
    }


}
