use std::io::IsTerminal as _;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::coordinator::{self, OpenOptions};
use crate::paths::{self, Ctx, Env, SessionFlags};
use crate::project::{self, Project, Status};
use crate::runner::RealRunner;
use crate::threads::{self, ResolveArgs, StartArgs};
use crate::{adopt, doctor, inbox, lanes, lifecycle, overview, routine, ticker};

#[derive(Parser)]
#[command(name = "hp", version = crate::VERSION, about = "Projects for herdr-core")]
struct Cli {
    /// Projects root (default: $HERDR_PROJECTS_ROOT, then config.toml, then ~/.herdr-projects)
    #[arg(long, global = true, value_name = "DIR")]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone, Default)]
pub struct SessionArgs {
    /// herdr session name (default: `HC_SESSION`, else the default core)
    #[arg(long, value_name = "NAME")]
    session: Option<String>,
}

impl From<SessionArgs> for SessionFlags {
    fn from(args: SessionArgs) -> Self {
        SessionFlags { session: args.session }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Create a project folder with its skeleton files
    New {
        name: String,
        #[arg(long, default_value = "")]
        goal: String,
        /// A repository, as PATH or PATH@MACHINE; repeatable
        #[arg(long = "repo", value_name = "PATH[@MACHINE]")]
        repos: Vec<String>,
        /// The project's default thread profile (default: thread_profile in [defaults] of config.toml, else claude)
        #[arg(long, value_name = "NAME")]
        thread_profile: Option<String>,
        /// The project's default coordinator profile (default: coordinator_profile in [defaults], else claude)
        #[arg(long, value_name = "NAME")]
        coordinator_profile: Option<String>,
    },
    /// List projects, archived ones marked `archived`
    List,
    /// Open a project: start a coordinator agent in its folder — in this terminal when no agent runs there, else in a new terminal of the project's Space
    Open {
        slug: String,
        /// The coordinator's profile, one the project allows (default: coordinator_profile in PROJECT.md); `profile list` shows them
        #[arg(long, alias = "agent", value_name = "NAME")]
        profile: Option<String>,
        /// Start another coordinator even though one is running
        #[arg(long)]
        new: bool,
        /// Start the coordinator in a new tab even when run from a free core
        /// terminal (by default it starts in that terminal)
        #[arg(long)]
        tab: bool,
        /// Move the project to this session when the session it was opened in no longer runs
        #[arg(long)]
        rebind: bool,
        /// Open or focus this lane's coordinator (default: the default lane).
        /// Without a name (`--lane`) it reopens the default lane when that is
        /// idle, else the lowest idle `coordinator<N>` lane, and only when every
        /// one of them is running does it create the next one
        #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
        lane: Option<String>,
        /// The display label when --lane creates a new lane (default: the name)
        #[arg(long, value_name = "LABEL", requires = "lane")]
        lane_name: Option<String>,
        #[command(flatten)]
        session: SessionArgs,
    },
    /// The coordinator: send it a sentence
    Coordinator {
        #[command(subcommand)]
        command: CoordinatorCommand,
    },
    /// Print the digest the coordinator reads at the start of every turn
    Context {
        slug: String,
        /// Print without recording the inbox items as seen
        #[arg(long)]
        peek: bool,
    },
    /// Who TASKS.md tasks may be assigned to: this project's thread profiles here, and `me`
    Assignable {
        slug: String,
        /// Accepted and ignored: it used to look up other machines' profiles again
        #[arg(long, hide = true)]
        refresh: bool,
        /// Exit with an error unless OWNER (me or PROFILE) is valid
        #[arg(long, value_name = "OWNER")]
        check: Option<String>,
    },
    /// Print threads grouped by what needs you
    Overview {
        slug: Option<String>,
        /// Wait for Enter before exiting (only when on a terminal; used by the popup)
        #[arg(long)]
        wait: bool,
    },
    /// Inbox items
    Inbox {
        #[command(subcommand)]
        command: InboxCommand,
    },
    /// TASKS.md: add, move, tick, delete or thread a task without touching any other byte
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Lanes: the project's lines of work and who coordinates each
    Lane {
        #[command(subcommand)]
        command: LaneCommand,
    },
    /// Threads: the project's worker agents
    Thread {
        #[command(subcommand)]
        command: ThreadCommand,
    },
    /// Routines: scheduled prompts and watched commands
    Routine {
        #[command(subcommand)]
        command: RoutineCommand,
    },
    /// Pause a project: the ticker skips it and `thread start` is refused
    Pause { slug: String },
    /// Make a paused project active again
    Resume { slug: String },
    /// Archive a project: paused, hidden, labels cleared, `open` refused
    Archive { slug: String },
    /// Make an archived project active again
    Unarchive { slug: String },
    /// Move a project folder to the trash (no worktree, branch or PR is touched)
    Delete {
        slug: String,
        /// Delete even though coordinator or thread panes are alive
        #[arg(long)]
        force: bool,
    },
    /// Give a project a new slug (its folder name), and with --name a new display name.
    /// Refused while a thread is not resolved. While agents run in the project folder
    /// (its coordinator too) the ticker does it once they are idle: it closes their
    /// panes, renames, and reopens the coordinator in the new folder
    Rename {
        slug: String,
        new_slug: String,
        /// The new display name (default: keep it)
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Print the plan and change nothing
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        session: SessionArgs,
    },
    /// Agent profiles: named launch setups (harness, model, effort, flags) and which ones threads and coordinators may use
    Profile {
        #[command(subcommand)]
        command: ProfileCommand,
    },
    /// Safety settings
    Safety {
        #[command(subcommand)]
        command: SafetyCommand,
    },
    /// Print the coordinator skill
    Skill,
    /// Check the setup: versions, tools, root, ticker and each project's files and session
    Doctor {
        /// Repair what can be repaired: priming files, uploads/, stale binary paths
        #[arg(long)]
        fix: bool,
        #[command(flatten)]
        session: SessionArgs,
    },
    /// List what a project left behind (orphan worktrees, merged branches, tabs, old folders) and remove it
    Sweep {
        slug: String,
        /// Only list
        #[arg(long)]
        dry_run: bool,
        /// Remove without asking
        #[arg(long)]
        yes: bool,
    },
    /// Change one setting in PROJECT.md (name, goal, coordinator_profile, thread_profile, max_parallel_threads, auto_resolve_days, nudge, repos.add, repos.remove)
    Set { slug: String, key: String, value: String },
    /// Open a file: text in a new Herdr tab running $EDITOR, anything else with the system opener
    OpenFile {
        path: PathBuf,
        /// The workspace to add the tab to (default: the current one)
        #[arg(long, value_name = "ID")]
        workspace: Option<String>,
    },
    /// Open a URL (a pull request) in the browser
    OpenUrl { url: String },
    /// Report what the agent is doing (run by an agent in its own terminal)
    Report {
        /// The whole requested outcome and its checks are finished
        #[arg(long)]
        done: bool,
        /// Two to four words; `Waiting for you` before asking the user something
        #[arg(long, default_value = "")]
        activity: String,
    },
    /// Print the progress record of this pane, or of --pane
    Progress {
        #[arg(long, value_name = "ID")]
        pane: Option<String>,
    },
    /// Say what this build is and how to move it (it comes from a checkout; there is no release feed)
    Update {
        /// Print this build's version and how to rebuild it; change nothing
        #[arg(long)]
        check: bool,
    },
    /// The background ticker
    Ticker {
        #[command(subcommand)]
        command: TickerCommand,
    },
}

#[derive(Subcommand)]
enum TaskCommand {
    /// Append a task to a list (creates the list and, when needed, the file skeleton)
    Add {
        slug: String,
        /// The list: a shared `## <list>` heading, or with --lane the part after `<lane>/`
        list: String,
        title: String,
        /// The owner, in TASKS.md's existing syntax: me, PROFILE, @MACHINE or PROFILE@MACHINE
        #[arg(long, value_name = "OWNER")]
        owner: Option<String>,
        /// Write into this lane's list (`## <label>/<list>`) instead of the shared one
        #[arg(long, value_name = "LANE")]
        lane: Option<String>,
        /// Indented notes under the task line (a literal \n starts another line)
        #[arg(long, value_name = "TEXT")]
        notes: Option<String>,
    },
    /// Move a uniquely titled task line and its notes to another list
    Move {
        slug: String,
        title: String,
        /// The target list: a shared `## <list>` heading or `<lane>/<list>` for your own lane
        list: String,
    },
    /// Tick a task's checkbox (`- [ ]` -> `- [x]`); the line stays
    Done { slug: String, title: String },
    /// Delete a task and its indented notes; an empty list heading stays
    Delete {
        slug: String,
        title: String,
    },
    /// Record the thread that runs a task, optionally setting its owner in the same write
    Thread {
        slug: String,
        title: String,
        /// The thread id, for example t-0012 (omit with --clear)
        thread: Option<String>,
        /// Remove the thread id instead of setting one
        #[arg(long, conflicts_with = "thread")]
        clear: bool,
        /// Set or replace the task's owner in the same write
        #[arg(long, value_name = "OWNER")]
        owner: Option<String>,
    },
}

#[derive(Subcommand)]
enum InboxCommand {
    /// Move handled items to inbox/done/
    Done {
        slug: String,
        #[arg(value_name = "ITEM_ID", required_unless_present = "all")]
        ids: Vec<String>,
        #[arg(long, conflicts_with = "ids")]
        all: bool,
    },
}

#[derive(Subcommand)]
enum LaneCommand {
    /// Every lane with its status, default, last activity and the rule each
    /// live pane matched (the same fact `context` prints on its
    /// `Coordinators:` line)
    List {
        slug: String,
        #[arg(long)]
        json: bool,
    },
    /// Claim the current pane as LANE's coordinator (run it in that pane)
    Adopt {
        /// The lane name (or id)
        lane: String,
        /// The project; default: the project whose home this pane works in
        #[arg(long, value_name = "SLUG")]
        project: Option<String>,
    },
    /// Point a lane (and its threads) at another coordinator pane: the explicit handover
    Assign {
        /// The lane name (or id)
        lane: String,
        /// The pane that becomes its coordinator
        pane: String,
        #[arg(long, value_name = "SLUG")]
        project: Option<String>,
    },
    /// Change a lane's display label, and optionally its CLI name
    Rename {
        /// The lane name (or id)
        lane: String,
        /// The new display label
        label: String,
        /// The new CLI handle
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        #[arg(long, value_name = "SLUG")]
        project: Option<String>,
    },
    /// Retire a lane once none of its threads is open
    Close {
        /// The lane name (or id)
        lane: String,
        #[arg(long, value_name = "SLUG")]
        project: Option<String>,
    },
    /// Reopen a closed lane (starts no agent)
    Open {
        /// The lane name (or id)
        lane: String,
        #[arg(long, value_name = "SLUG")]
        project: Option<String>,
    },
}

#[derive(Subcommand)]
enum CoordinatorCommand {
    /// Send a sentence to the project's coordinator (the popup's task keys use this)
    Prompt {
        slug: String,
        /// The lane to send to (by id, name or label); default: the caller's own
        #[arg(long, value_name = "LANE")]
        lane: Option<String>,
        /// The text; `-` reads standard input
        #[arg(long, value_name = "FILE")]
        text_file: String,
    },
}

#[derive(Subcommand)]
enum ThreadCommand {
    /// Start a thread: a worktree workspace for --repo, else a tab of the project's own workspace
    Start {
        slug: String,
        #[arg(long)]
        title: String,
        #[arg(long, value_name = "PATH")]
        repo: Option<String>,
        /// The agent's profile (harness, model, effort, flags), one the project allows (default: thread_profile in PROJECT.md); `context` lists them
        #[arg(long, alias = "agent", value_name = "NAME")]
        profile: Option<String>,
        /// Placement, not the agent: worktree (default with --repo: its own worktree and branch), tab (a Space of its own on threads/<id>/), checkout (a Space of its own on the repo's main checkout), or home (default without a repo: a tab of the project's own workspace on threads/<id>/)
        #[arg(long, value_name = "worktree|tab|checkout|home")]
        kind: Option<String>,
        #[arg(long, value_name = "REF")]
        base: Option<String>,
        /// The task; `-` reads standard input
        #[arg(long, value_name = "FILE")]
        task_file: String,
        /// Delegating this TASKS.md task (its title as written there): its notes are added to the task, and its owner gives --profile
        #[arg(long, value_name = "TITLE")]
        from_task: Option<String>,
    },
    /// Bring back a thread whose pane is gone or whose start failed
    Restart {
        slug: String,
        id: String,
        /// Restart with another profile the project allows (default: the thread's own)
        #[arg(long, alias = "agent", value_name = "NAME")]
        profile: Option<String>,
    },
    /// Send a follow-up to a thread's agent (recorded in its task file)
    Prompt {
        slug: String,
        id: String,
        /// The text; `-` reads standard input
        #[arg(long, value_name = "FILE")]
        text_file: String,
        /// The text is the user's own words, forwarded unchanged: its entry in the task file says so, instead of standing as the coordinator's own summary
        #[arg(long)]
        verbatim: bool,
    },
    /// A thread's Next list: print it, forward line N as a prompt, or add a line
    Next {
        slug: String,
        id: String,
        /// Forward this line (1-based) to the thread as a prompt
        #[arg(long, value_name = "N", conflicts_with = "add")]
        line: Option<usize>,
        /// Add a line to the list
        #[arg(long, value_name = "TEXT")]
        add: Option<String>,
    },
    /// Send Escape to a thread's pane (the harness's own interrupt)
    Stop { slug: String, id: String },
    /// Deliver a thread's brief now, when its agent is ready but the ticker has not sent it yet
    Brief { slug: String, id: String },
    /// Print what a thread's pane shows now (a trust dialog, a question menu, a permission prompt)
    Read {
        slug: String,
        id: String,
        /// Read the last N lines of scrollback instead of the visible screen
        #[arg(long, value_name = "N")]
        lines: Option<usize>,
    },
    /// Answer what a thread's pane shows: type --text (no Enter), then press the keys (enter, esc, up, down, tab, 1, y, ctrl+c)
    Keys {
        slug: String,
        id: String,
        #[arg(value_name = "KEY")]
        keys: Vec<String>,
        /// Literal text typed before the keys
        #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
        text: Option<String>,
    },
    /// List threads with live state and group
    List {
        slug: String,
        #[arg(long)]
        json: bool,
    },
    /// Show one thread's record, group, note and Next list
    Show {
        slug: String,
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Cross-check thread records against herdr's agents and panes (read only)
    Audit {
        /// The project; default: the project of the current herdr workspace
        slug: Option<String>,
        /// Machine-readable findings
        #[arg(long)]
        json: bool,
    },
    /// Record an existing local agent pane as a thread of this project
    Adopt {
        slug: String,
        #[arg(long, value_name = "ID")]
        pane: String,
        #[arg(long)]
        title: String,
        /// Optional task; `-` reads standard input
        #[arg(long, value_name = "FILE")]
        task_file: Option<String>,
    },
    /// Record that the user has seen the current report
    Ack { slug: String, id: String },
    /// Resolve a thread: final copy home, then its worktree, merged branch and tab are cleaned up (reports and library are kept)
    Resolve {
        slug: String,
        id: String,
        #[arg(long, conflicts_with_all = ["keep_worktree", "skip_copy", "discard_uncopied"])]
        reopen: bool,
        /// Keep the worktree and branch
        #[arg(long)]
        keep_worktree: bool,
        /// Resolve even though the final copy cannot be made (the worktree is then kept)
        #[arg(long)]
        skip_copy: bool,
        /// Remove the worktree even though not everything in it was copied home
        #[arg(long)]
        discard_uncopied: bool,
    },
}

/// `-` is standard input; a relative path is relative to the caller's directory.
fn read_text(file: &str) -> Result<String> {
    use std::io::Read;
    if file == "-" {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        Ok(text)
    } else {
        std::fs::read_to_string(file).map_err(|e| anyhow::anyhow!("could not read {file}: {e}"))
    }
}

#[derive(Subcommand)]
enum RoutineCommand {
    /// Enable or disable a routine
    Toggle {
        slug: String,
        name: String,
        #[arg(long, conflicts_with = "off")]
        on: bool,
        #[arg(long)]
        off: bool,
    },
    /// Approve a routine's command (a person at a terminal only)
    Approve { slug: String, name: String },
    /// List routines with their approval status
    List { slug: String },
}

/// The typed fields of a profile, for `add` and `edit`.
#[derive(Args)]
struct ProfileFields {
    /// Model name, passed as --model (empty: the harness's default)
    #[arg(long, value_name = "NAME")]
    model: Option<String>,
    /// Reasoning effort: claude low..max, codex none..ultra, copilot none..max, pi/omp off..max (others: put it in --arg or the model id)
    #[arg(long, value_name = "LEVEL")]
    effort: Option<String>,
    /// One line on when to use it; the coordinator reads it when choosing
    #[arg(long, value_name = "TEXT")]
    description: Option<String>,
    /// An extra argument for the agent CLI, repeatable (--arg --config --arg ~/.omp/agent/luna.yml)
    #[arg(long = "arg", value_name = "ARG", allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Subcommand)]
enum ProfileCommand {
    /// List the profiles (yours and the installed, signed-in harnesses), and with --project what it allows
    List {
        #[arg(long, value_name = "SLUG")]
        project: Option<String>,
        /// Only the names, one per line
        #[arg(long, conflicts_with = "project")]
        names: bool,
    },
    /// Print a profile's launch setup here as JSON (default: [defaults] thread_profile)
    Resolve {
        name: Option<String>,
    },
    /// Add a profile (a person at a terminal only)
    Add {
        name: String,
        /// The harness: a Herdr agent kind (claude, codex, cursor, gemini, opencode, copilot, omp, pi, ...)
        #[arg(long, value_name = "KIND")]
        agent: String,
        #[command(flatten)]
        fields: ProfileFields,
    },
    /// Change a profile's fields; --arg replaces its arguments (a person at a terminal only)
    Edit {
        name: String,
        #[arg(long, value_name = "KIND")]
        agent: Option<String>,
        #[command(flatten)]
        fields: ProfileFields,
        /// Remove all its extra arguments
        #[arg(long, conflicts_with = "args")]
        clear_args: bool,
    },
    /// Remove a profile (a person at a terminal only)
    Remove { name: String },
    /// Set which profiles threads or coordinators may use, for one project or as the default for all (a person at a terminal only)
    Allow {
        #[arg(value_parser = ["threads", "coordinator"])]
        role: String,
        #[arg(required_unless_present = "all")]
        names: Vec<String>,
        /// Only this project (default: every project without its own list)
        #[arg(long, value_name = "SLUG")]
        project: Option<String>,
        /// Allow every profile (removes the list)
        #[arg(long, conflicts_with = "names")]
        all: bool,
    },
    /// The profile new projects start with for threads or coordinators; one project's own is `set <slug> thread_profile NAME` (a person at a terminal only)
    Default {
        #[arg(value_parser = ["threads", "coordinator"])]
        role: String,
        name: String,
    },
}

#[derive(Subcommand)]
enum SafetyCommand {
    /// Print the effective safety settings, where each comes from, and how to change them
    Show {
        /// A project, or --global for the all-projects defaults
        #[arg(allow_hyphen_values = true)]
        target: String,
    },
    /// Turn yolo mode on or off for a project or, with --global, for all projects (a person at a terminal only)
    Yolo {
        /// A project, or --global for the all-projects default
        #[arg(allow_hyphen_values = true)]
        target: String,
        /// on, off, or default (use the all-projects value)
        #[arg(value_parser = ["on", "off", "default"])]
        state: String,
    },
    /// Change one safety setting for a project or, with --global, for all projects (a person at a terminal only)
    Set {
        /// A project, or --global for the all-projects defaults
        #[arg(allow_hyphen_values = true)]
        target: String,
        /// yolo, start_threads, trust_screens, coordinator_agent_args, thread_agent_args or routine_commands
        key: String,
        /// The value (arguments for *_agent_args, none for an empty list), or `default`
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        value: Vec<String>,
    },
}

#[derive(Subcommand)]
enum TickerCommand {
    /// Start the ticker if it is not running (does nothing when there are no projects)
    Start,
    /// Run the ticker loop in the foreground
    Run,
    /// Ask the running ticker to exit and wait for it
    Stop,
    /// Show the running ticker's version, root and tool resolution
    Status,
}

fn profile_change(ctx: &Ctx, command: ProfileCommand) -> Result<crate::profiles::Change> {
    use crate::profiles::{Change, Entry, Role};
    Ok(match command {
        ProfileCommand::List { .. } | ProfileCommand::Resolve { .. } => bail!("`profile list` and `profile resolve` change nothing"),
        ProfileCommand::Add { name, agent, fields } => Change::Add {
            name,
            entry: Entry { agent, model: fields.model.unwrap_or_default(), effort: fields.effort.unwrap_or_default(), args: fields.args, description: fields.description.unwrap_or_default() },
        },
        ProfileCommand::Edit { name, agent, fields, clear_args } => Change::Edit {
            name,
            agent,
            model: fields.model,
            effort: fields.effort,
            description: fields.description,
            args: if clear_args { Some(Vec::new()) } else { (!fields.args.is_empty()).then_some(fields.args) },
        },
        ProfileCommand::Remove { name } => Change::Remove { name },
        ProfileCommand::Allow { role, names, project, all } => Change::Allow {
            role: Role::parse(&role)?,
            project: project.map(|slug| Project::load(&ctx.root, &slug).map(|p| p.canonical_dir())).transpose()?,
            names: (!all).then_some(names),
        },
        ProfileCommand::Default { role, name } => Change::Default { role: Role::parse(&role)?, name },
    })
}

/// The lane `hp task move` / `hp task done` run as, for the cross-lane guard.
/// A project without a registry has no lanes, so there is nothing to guard; a
/// pane that resolves to no lane reads as the default lane (§10).
fn task_caller_lane(ctx: &Ctx, project: &Project) -> Option<String> {
    if !lanes::split_in_use(project) {
        return None;
    }
    let lane = lanes::lane_of_initiator(ctx, project, &[]);
    Some(if lane.is_empty() { lanes::default_lane_id(project) } else { lane })
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let env = Env::from_process()?;
    let config_dir = env.config_dir();
    let root = paths::resolve_root(cli.root.as_deref(), &env, &config_dir)?;
    let runner = RealRunner;
    let ctx = Ctx {
        env: &env,
        root,
        config_dir,
        runner: &runner,
        detached_ticker: true,
    };

    match cli.command {
        Command::New { name, goal, repos, thread_profile, coordinator_profile } => {
            let repos = repos.into_iter().map(|path| project::Repo { path }).collect();
            let config = crate::profiles::load(&ctx.config_dir)?;
            let thread = thread_profile.unwrap_or_else(|| config.new_project_default(crate::profiles::Role::Thread));
            let coordinator = coordinator_profile.unwrap_or_else(|| config.new_project_default(crate::profiles::Role::Coordinator));
            for name in [&thread, &coordinator] {
                if config.get(name).is_none() {
                    bail!("there is no profile `{name}`; `profile list` shows them");
                }
            }
            let project = project::create(&ctx.root, &name, &goal, repos)?;
            crate::profiles::write_project_defaults(&project, &thread, &coordinator)?;
            let prefix = coordinator::current_prefix(&ctx.root)?;
            project::write_priming(&project, &prefix, "new")?;
            println!("created `{}` at {}", project.slug, project.dir().display());
            println!("next: {prefix} open {}", project.slug);
            Ok(())
        }
        Command::List => {
            for slug in project::list_slugs(&ctx.root) {
                let project = Project::load(&ctx.root, &slug)?;
                let status = project.status();
                let mut counts = std::collections::BTreeMap::new();
                for row in threads::rows(&ctx, &project) {
                    *counts.entry(row.group.rank()).or_insert((row.group.label(), 0)) = (row.group.label(), counts.get(&row.group.rank()).map_or(0, |c: &(&str, usize)| c.1) + 1);
                }
                let summary: Vec<String> = counts.values().map(|(label, n)| format!("{label}: {n}")).collect();
                println!("{slug}\t{status}\t{}", if summary.is_empty() { "no threads".to_string() } else { summary.join(", ") });
            }
            Ok(())
        }
        Command::Open { slug, profile, new, tab, rebind, lane, lane_name, session } => {
            let person = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
            coordinator::open(
                &ctx,
                &slug,
                &OpenOptions {
                    session: session.into(),
                    rebind,
                    profile,
                    new,
                    // Only a person at a terminal gets the agent in place; the
                    // popup and agents' shell tools run `open` without one.
                    here: !tab && person,
                    person,
                    lane,
                    lane_name,
                },
            )
        }
        Command::Coordinator { command } => match command {
            CoordinatorCommand::Prompt { slug, lane, text_file } => {
                let text = read_text(&text_file)?;
                match lane.as_deref() {
                    Some(lane) => coordinator::prompt_to(&ctx, &slug, &text, Some(lane)),
                    None => coordinator::prompt(&ctx, &slug, &text),
                }
            }
        },
        Command::Context { slug, peek } => coordinator::context(&ctx, &slug, peek),
        Command::Assignable { slug, refresh, check } => crate::assign::run(&ctx, &slug, refresh, check.as_deref()),
        Command::Overview { slug, wait } => overview::run(&ctx, slug.as_deref(), wait),
        Command::Inbox { command } => match command {
            InboxCommand::Done { slug, ids, all } => {
                let project = Project::load(&ctx.root, &slug)?;
                let moved = inbox::done(&project, &ids, all)?;
                println!("{moved} item(s) moved to inbox/done");
                Ok(())
            }
        },
        Command::Task { command } => match command {
            TaskCommand::Add { slug, list, title, owner, lane, notes } => {
                let project = Project::load(&ctx.root, &slug)?;
                let options = crate::tasks::AddOptions { owner: owner.as_deref(), lane: lane.as_deref(), notes: notes.as_deref() };
                println!("{}", crate::tasks::add(&project, &list, &title, &options)?);
                Ok(())
            }
            TaskCommand::Move { slug, title, list } => {
                let project = Project::load(&ctx.root, &slug)?;
                let lane = task_caller_lane(&ctx, &project);
                println!("{}", crate::tasks::move_task(&project, &title, &list, lane.as_deref())?);
                Ok(())
            }
            TaskCommand::Done { slug, title } => {
                let project = Project::load(&ctx.root, &slug)?;
                let lane = task_caller_lane(&ctx, &project);
                println!("{}", crate::tasks::done(&project, &title, lane.as_deref())?);
                Ok(())
            }
            TaskCommand::Delete { slug, title } => {
                let project = Project::load(&ctx.root, &slug)?;
                let lane = task_caller_lane(&ctx, &project);
                println!("{}", crate::tasks::delete(&project, &title, lane.as_deref())?);
                Ok(())
            }
            TaskCommand::Thread { slug, title, thread, clear, owner } => {
                let project = Project::load(&ctx.root, &slug)?;
                let lane = task_caller_lane(&ctx, &project);
                let options = crate::tasks::ThreadOptions { thread: thread.as_deref(), owner: owner.as_deref(), clear };
                println!("{}", crate::tasks::record_thread(&project, &title, &options, lane.as_deref())?);
                Ok(())
            }
        },
        Command::Lane { command } => match command {
            LaneCommand::List { slug, json } => lanes::lane_list(&ctx, &slug, json),
            LaneCommand::Adopt { lane, project } => lanes::lane_adopt(&ctx, project.as_deref(), &lane),
            LaneCommand::Assign { lane, pane, project } => {
                let project = lanes::project_for_command(&ctx, &lane, project.as_deref())?;
                lanes::lane_assign(&ctx, &project, &lane, &pane)
            }
            LaneCommand::Rename { lane, label, name, project } => {
                let project = lanes::project_for_command(&ctx, &lane, project.as_deref())?;
                lanes::lane_rename(&ctx, &project, &lane, &label, name.as_deref())
            }
            LaneCommand::Close { lane, project } => {
                let project = lanes::project_for_command(&ctx, &lane, project.as_deref())?;
                lanes::lane_close(&ctx, &project, &lane)
            }
            LaneCommand::Open { lane, project } => {
                let project = lanes::project_for_command(&ctx, &lane, project.as_deref())?;
                lanes::lane_open(&ctx, &project, &lane)
            }
        },
        Command::Thread { command } => match command {
            ThreadCommand::Start { slug, title, repo, mut profile, kind, base, task_file, from_task } => {
                let mut task = read_text(&task_file)?;
                if let Some(from) = from_task {
                    // The task's owner picks the profile.
                    let project = Project::load(&ctx.root, &slug)?;
                    let tasks_md = crate::tasks::read(&project.dir());
                    let registry = lanes::Registry::load(&project);
                    let lane = task_caller_lane(&ctx, &project);
                    let found = crate::tasks::find_in(&tasks_md, &from, registry.as_ref(), lane.as_deref())?;
                    let config = crate::profiles::load(&ctx.config_dir)?;
                    if found.owner.is_person(|p| config.get(p).is_some()) {
                        bail!("\"{}\" belongs to {}, a person; people's tasks are never delegated. Change its owner in TASKS.md first if the user asks", found.title, found.owner);
                    }
                    profile = crate::tasks::launch_for(&found, profile)?;
                    task = if registry.is_some() {
                        crate::tasks::delegated_from(&found, &task)
                    } else {
                        // The pre-lane path, byte for byte: `find` is exactly
                        // what this command used before lanes existed.
                        crate::tasks::delegated(&tasks_md, &from, &task)?
                    };
                }
                let kind = kind.as_deref().map(crate::thread::Kind::parse).transpose()?;
                let thread = threads::start(&ctx, &slug, StartArgs { title, repo, profile, kind, base, task })?;
                println!("{}", serde_json::json!({ "id": thread.id, "kind": thread.kind, "profile": thread.profile, "agent": thread.agent, "branch": thread.branch, "pane_id": thread.pane_id }));
                Ok(())
            }
            ThreadCommand::Restart { slug, id, profile } => {
                let thread = threads::restart(&ctx, &slug, &id, profile.as_deref())?;
                println!("{} is back in pane {}; the ticker launches its {} agent", thread.id, thread.pane_id, if thread.profile.is_empty() { &thread.agent } else { &thread.profile });
                Ok(())
            }
            ThreadCommand::Next { slug, id, line, add } => threads::next(&ctx, &slug, &id, line, add.as_deref()),
            ThreadCommand::Stop { slug, id } => threads::stop(&ctx, &slug, &id),
            ThreadCommand::Brief { slug, id } => threads::brief(&ctx, &slug, &id),
            ThreadCommand::Read { slug, id, lines } => threads::read(&ctx, &slug, &id, lines),
            ThreadCommand::Keys { slug, id, keys, text } => threads::keys(&ctx, &slug, &id, &keys, text.as_deref()),
            ThreadCommand::Prompt { slug, id, text_file, verbatim } => {
                let text = read_text(&text_file)?;
                let recorded = if verbatim { crate::thread::Recorded::User } else { crate::thread::Recorded::Coordinator };
                let state = threads::prompt(&ctx, &slug, &id, &text, recorded)?;
                let mark = if verbatim { format!("; recorded as {}", crate::thread::VERBATIM_MARK) } else { String::new() };
                println!("sent to {id} (agent was {state}){mark}");
                Ok(())
            }
            ThreadCommand::Adopt { slug, pane, title, task_file } => {
                let task = task_file.map(|file| read_text(&file)).transpose()?;
                let thread = adopt::adopt(&ctx, &slug, &pane, &title, task)?;
                println!("{}", serde_json::json!({ "id": thread.id, "kind": thread.kind, "pane_id": thread.pane_id, "prompt_pending": thread.prompt_pending }));
                Ok(())
            }
            ThreadCommand::List { slug, json } => threads::print_list(&ctx, &slug, json),
            ThreadCommand::Show { slug, id, json } => threads::print_show(&ctx, &slug, &id, json),
            ThreadCommand::Audit { slug, json } => crate::audit::run(&ctx, slug.as_deref(), json),
            ThreadCommand::Ack { slug, id } => threads::ack(&ctx, &slug, &id),
            ThreadCommand::Resolve { slug, id, reopen, keep_worktree, skip_copy, discard_uncopied } => {
                threads::resolve(&ctx, &slug, &id, &ResolveArgs { reopen, keep_worktree, skip_copy, discard_uncopied })
            }
        },
        Command::Sweep { slug, dry_run, yes } => crate::sweep::run(&ctx, &slug, dry_run, yes),
        Command::Set { slug, key, value } => crate::settings::set(&ctx, &slug, &key, &value),
        Command::OpenFile { path, workspace } => crate::settings::open_file(&ctx, &path, workspace.as_deref()),
        Command::OpenUrl { url } => {
            if !url.starts_with("https://") {
                bail!("only https URLs are opened");
            }
            crate::settings::system_open(&ctx, &url)
        }
        Command::Routine { command } => match command {
            RoutineCommand::Toggle { slug, name, on, off } => crate::settings::routine_toggle(&ctx, &slug, &name, if on { Some(true) } else if off { Some(false) } else { None }),
            RoutineCommand::Approve { slug, name } => {
                let project = Project::load(&ctx.root, &slug)?;
                routine::approve(&ctx.config_dir, &project, &name)
            }
            RoutineCommand::List { slug } => {
                let project = Project::load(&ctx.root, &slug)?;
                let commands = project.safety(&ctx.config_dir)?.routine_commands;
                routine::print_list(&ctx.config_dir, &project, commands);
                Ok(())
            }
        },
        Command::Pause { slug } => lifecycle::set_status(&ctx, &slug, Status::Paused),
        Command::Resume { slug } => {
            if Project::load(&ctx.root, &slug)?.status() == Status::Archived {
                bail!("`{slug}` is archived; use `unarchive`");
            }
            lifecycle::set_status(&ctx, &slug, Status::Active)
        }
        Command::Archive { slug } => lifecycle::set_status(&ctx, &slug, Status::Archived),
        Command::Unarchive { slug } => lifecycle::set_status(&ctx, &slug, Status::Active),
        Command::Delete { slug, force } => lifecycle::delete(&ctx, &slug, force),
        Command::Rename { slug, new_slug, name, dry_run, session } => {
            crate::rename::cli(&ctx, &crate::rename::Args { from: &slug, to: &new_slug, name: name.as_deref(), dry_run, by_ticker: false }, &session.into())
        }
        Command::Profile { command } => {
            if let ProfileCommand::Resolve { name } = &command {
                println!("{}", crate::profiles::resolve_json(&ctx, name.as_deref())?);
                return Ok(());
            }
            if let ProfileCommand::List { project, names } = command {
                if names {
                    let config = crate::profiles::load(&ctx.config_dir)?;
                    for p in config.listed(&crate::profiles::detect(ctx.env)) {
                        println!("{}", p.name);
                    }
                    return Ok(());
                }
                let project = project.map(|slug| Project::load(&ctx.root, &slug)).transpose()?;
                print!("{}", crate::profiles::list_text(&ctx, project.as_ref())?);
                return Ok(());
            }
            let change = profile_change(&ctx, command)?;
            crate::profiles::require_person()?;
            println!("{}", crate::profiles::apply(&ctx.config_dir, &change)?);
            Ok(())
        }
        Command::Safety { command } => match command {
            SafetyCommand::Show { target } => {
                print!("{}", crate::safety::show_text(&ctx, &crate::safety::Target::parse(&ctx, &target)?)?);
                Ok(())
            }
            SafetyCommand::Yolo { target, state } => crate::safety::set_cli(&ctx, &target, "yolo", &[state]),
            SafetyCommand::Set { target, key, value } => crate::safety::set_cli(&ctx, &target, &key, &value),
        },
        Command::Skill => {
            print!("{}", include_str!("../skill/COORDINATOR.md"));
            // Every harness learns to report from this text; nothing is installed.
            let pane = crate::progress::current(ctx.env, ctx.runner).map(|p| p.pane_id);
            println!("\n## Progress\n\n{}", crate::progress::guidance(&crate::coordinator::current_prefix(&ctx.root)?, pane.as_deref()));
            Ok(())
        }
        Command::Doctor { fix, session } => {
            if !doctor::run(&ctx, &session.into(), fix)? {
                bail!("some checks failed");
            }
            Ok(())
        }
        Command::Report { done, activity } => crate::progress::report(&ctx, done, &activity),
        Command::Progress { pane } => crate::progress::print(&ctx, pane.as_deref()),
        Command::Update { check } => crate::update::run(&ctx, check),
        Command::Ticker { command } => match command {
            TickerCommand::Start => ticker::start(&ctx),
            TickerCommand::Run => ticker::run(&ctx),
            TickerCommand::Stop => ticker::stop(&ctx.root),
            TickerCommand::Status => ticker::status(&ctx.root),
        },
    }
}
