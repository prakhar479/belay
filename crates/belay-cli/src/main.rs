//! belay CLI: launcher, hook shim, session daemon, and inspection commands.

// Unix sockets, process groups, and signals throughout: say so instead of a page of
// errors (I5, X1).
#[cfg(not(unix))]
compile_error!("belay supports Unix (Linux, macOS) only for now");

mod audit;
mod brief;
mod commons;
mod config;
mod connector;
mod daemon;
mod doctor;
mod export;
mod findings;
mod flags;
mod fleet;
mod hub;
mod install;
mod ipc;
mod launcher;
mod learnstore;
mod mcp;
mod memorystore;
mod paths;
mod plugin;
mod report;
mod retro;
mod run;
mod sandbox;
mod sessions;
mod shim;
mod skillstore;
mod store;
mod undo;
mod watch;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "belay",
    version,
    about = "Local supervisor for command-line coding agents"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start Claude Code under belay. Other arguments, `--help` included, go to `claude`.
    ///
    /// belay's own flags may come anywhere: to join a fleet from this terminal,
    /// `--belay-fleet [ID]` with `--belay-role R` and `--belay-name N`.
    /// The same as `belay launch claude-code`. `belay help claude` shows this text.
    #[command(disable_help_flag = true)]
    Claude {
        /// 0.1's mode: accepted and ignored, with a warning.
        #[arg(long = "belay-mode", hide = true)]
        mode: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Start an agent under belay: `belay launch claude-code [args…]` is `belay claude`.
    ///
    /// Arguments after the agent go to it, `--help` included; belay's own flags work as
    /// for `belay claude`. Agents: claude-code. `belay help launch` shows this text.
    #[command(disable_help_flag = true)]
    Launch {
        agent: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Start Codex under belay (not built yet).
    #[command(hide = true)]
    Codex {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Install stable hooks for an agent once (not built yet).
    #[command(hide = true)]
    Init { agent: String },
    /// Pause belay in a live session: one feature, or all of them. Run it yourself;
    /// the agent may not. Features: guard, loops, shape, wrap_tests, verify, memory, awareness,
    /// commons.
    Pause {
        feature: Option<String>,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// Resume what `belay pause` paused: one feature, or all of them.
    Resume {
        feature: Option<String>,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// 0.1: modes are gone. Says how to pause instead.
    #[command(hide = true)]
    Mode {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Allow one edit the test guard blocked (the latest, or action a-N). Run it yourself,
    /// for example with `! belay allow` in Claude Code; the agent may not.
    Allow {
        id: Option<String>,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// Show what happened in a session (default: the newest one started here).
    Report {
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
        /// Every section, including empty ones.
        #[arg(long)]
        full: bool,
    },
    /// What a session changed, what it checked, and what to look at before a commit or
    /// merge. From the ledger and git, never the agent's own summary.
    Brief {
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
        /// A pull request section (## Summary by belay).
        #[arg(long, conflicts_with = "json")]
        markdown: bool,
        /// Stable keys, for scripts.
        #[arg(long)]
        json: bool,
    },
    /// Print the full original of a summarized output (o-N).
    Show {
        id: String,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// Show the cause chain of an action (a-N), output (o-N), or entry number.
    Why {
        id: String,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// Say belay got something wrong: an action (a-N), an output (o-N), an entry number,
    /// or `session`; with no id, the newest warning. The note is optional. A flag lifts
    /// nothing. `belay review` lists flags.
    Flag {
        /// `[TARGET] [NOTE…]`: a first word that is not an id starts the note.
        words: Vec<String>,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// List flags across all sessions by rule, newest first: what each points at, and
    /// commands to look closer and to save it as a fixture. The agent's flags come apart.
    Review,
    /// Install belay into Claude Code: the plugin (default), the status line, or the
    /// launcher alias. Shows each change and asks first.
    Install {
        #[command(flatten)]
        parts: install::Parts,
        /// Marketplace source for the plugin: a GitHub repo or a local clone
        /// (default: prakhar479/belay).
        #[arg(long)]
        from: Option<String>,
        /// Replace a status line that is not belay's.
        #[arg(long)]
        force: bool,
        /// Do not ask before changing anything.
        #[arg(long, short)]
        yes: bool,
    },
    /// Undo `belay install`: the plugin and belay's status line by default. A status line
    /// that is not belay's is left alone.
    Uninstall {
        #[command(flatten)]
        parts: install::Parts,
        /// Do not ask before changing anything.
        #[arg(long, short)]
        yes: bool,
    },
    /// Will belay work here? Checks the install, Claude Code, the plugin, the sandbox,
    /// skills, config, and data. Each line is ok, warn, or fail; exit 1 only on a fail.
    Doctor {
        /// The same checks as JSON.
        #[arg(long)]
        json: bool,
    },
    /// One session as a sanitized JSON bundle (paths, home, user name, and secrets
    /// replaced), to attach to a bug report.
    Export {
        /// An id, a prefix, `-N`, or a path (default: the newest session started here).
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
        /// Leave out the stored tool outputs (keep only the ledger).
        #[arg(long)]
        no_outputs: bool,
        /// One action (a-N): write what led to it into the `--fixture` directory.
        #[arg(long, requires = "fixture")]
        action: Option<String>,
        /// A new directory for the action's fixture: the edit before and after, or the
        /// outputs, cleaned like the bundle.
        #[arg(long, requires = "action")]
        fixture: Option<PathBuf>,
    },
    /// Delete recorded sessions (their ledgers and stored outputs). Live sessions are
    /// kept.
    Purge {
        /// One session: an id, a prefix, `-N`, or a path.
        #[arg(long, allow_hyphen_values = true, conflicts_with_all = ["all", "older_than"])]
        session: Option<String>,
        /// Every session.
        #[arg(long)]
        all: bool,
        /// Sessions with no activity for this many days.
        #[arg(long, value_name = "DAYS")]
        older_than: Option<u64>,
    },
    /// (Experimental) What a git diff, or a belay session, changed in tests and checks:
    /// assertions and tests, snapshots, suppressions, check config, dependencies.
    Audit {
        /// `A..B`, `A...B` (from the merge base), or `A` (to the working tree).
        /// Default: HEAD to the working tree.
        range: Option<String>,
        #[arg(long, value_enum, default_value = "text")]
        format: AuditFormat,
        /// The audit of a belay session (with `experimental_audit = true`): an id, a
        /// prefix, or `-N`; alone, the newest.
        #[arg(long, num_args = 0..=1, default_missing_value = "-1", allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// One line for the Claude Code status line: what belay did so far.
    /// Prints nothing outside a belay session.
    Status,
    /// Print what belay does in a session as it happens: the newest live session, or
    /// the one named. Ctrl-C stops.
    Watch {
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
        /// Also print minor notices (wrapped runs, retries, summary savings).
        #[arg(long)]
        verbose: bool,
    },
    /// A fleet's task board: add, give out, take, finish, or block tasks. In a fleet
    /// session, the agent uses these; belay knows which session asks.
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
        /// A fleet id or prefix (default: this session's fleet, or the newest up).
        #[arg(long, global = true)]
        fleet: Option<String>,
    },
    /// Send a message: to any belay session by its card name, to `topic:<t>` (sessions
    /// that joined it), to `repo` (every session in this repo), or to `user`. In a fleet,
    /// also `@role`, `#team`, or a slot. Without --kind: an answer to whoever asked, a
    /// question if the text asks, else a notice (in a fleet, its channels decide).
    Msg {
        /// A card name, `topic:<t>`, `repo`, `user`; in a fleet also `@role`, `#team`
        to: String,
        /// The message
        text: String,
        /// question, answer, notice, request, handoff (a fleet also has task, report)
        #[arg(long)]
        kind: Option<String>,
        /// The fleet's id (default: this session's fleet, else the newest live one)
        #[arg(long)]
        fleet: Option<String>,
    },
    /// Messages: this session's (commons and fleet), or, for you, yours and the fleet's.
    Inbox {
        /// The fleet's id (default: this session's fleet, else the newest live one)
        #[arg(long)]
        fleet: Option<String>,
    },
    /// Keep a note for this project: what sessions should know (`belay notes` lists them).
    /// A note typed in your terminal is kept; one from an agent waits for your review.
    Note {
        /// What every new session of this project should know
        text: String,
    },
    /// This project's notes; `approve <id>` or `drop <id>` reviews one (yours to run).
    Notes {
        /// approve or drop
        verb: Option<String>,
        /// The note's id (`n-3`)
        id: Option<String>,
        /// Every project's notes.
        #[arg(long)]
        all: bool,
    },
    /// Fixes sessions learned in this project (a failure, the change, the run that then
    /// passed); `forget` drops them (yours to run).
    Fixes {
        /// forget
        verb: Option<String>,
        /// Every project's fixes.
        #[arg(long)]
        all: bool,
    },
    /// Publish a finding others can pull with `belay ask`: a gotcha, a decision, how
    /// something works. Text reaches other projects; `--code-file` only this one.
    Share {
        /// The finding, in a few sentences.
        text: String,
        /// gotcha, decision, how, fix, or note.
        #[arg(long)]
        kind: Option<String>,
        /// Tags, comma-separated (pytest,asyncio).
        #[arg(long, value_delimiter = ',')]
        tag: Vec<String>,
        /// Files it is about (repeat for more).
        #[arg(long)]
        file: Vec<String>,
        /// A short code snippet from a file (`-`: stdin); shown only in this project.
        #[arg(long)]
        code_file: Option<PathBuf>,
    },
    /// Search what other sessions published (and fixes they learned) before you dig in.
    Ask {
        /// What you want to know, in words.
        query: String,
        /// This project's findings only.
        #[arg(long)]
        here: bool,
        #[arg(long)]
        json: bool,
    },
    /// This project's findings; `drop f-N…` removes them (yours to run).
    Findings {
        /// drop
        verb: Option<String>,
        /// The findings to drop (`f-3 f-4`).
        ids: Vec<String>,
        /// Every project's findings.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// The effective config: each key, its value, and where it came from (the file or
    /// the default). `belay config migrate` rewrites 0.1 keys in the file.
    Config {
        #[command(subcommand)]
        action: Option<ConfigCmd>,
    },
    /// Memories for this project and you: list, search, add, show; approve or drop
    /// (yours to run); migrate copies notes, fixes, findings, and handoffs in once.
    Memory {
        #[command(subcommand)]
        action: Option<MemoryCmd>,
    },
    /// Skills from URLs: preview, add (an agent's add only stages), list, update;
    /// approve, reject, remove (yours to run).
    Skill {
        #[command(subcommand)]
        action: Option<SkillCmd>,
    },
    /// An MCP server over stdio: memory search and add, the handoff, outputs, and why.
    Mcp,
    /// Leave a handoff for the next session in this repo ("done: …; open: …; next: …"),
    /// from inside a session. With no text, show what the next session here would get.
    Handoff {
        /// What is done, what is open, what comes next.
        text: Option<String>,
    },
    /// This session's card: say what you are working on (from inside a belay session).
    /// With no flags, show it.
    Card {
        /// What this session is working on, in one line.
        #[arg(long)]
        task: Option<String>,
        /// Anything else others should know (one line).
        #[arg(long)]
        note: Option<String>,
        /// A name others can address (lowercase letters, digits, - _ .).
        #[arg(long)]
        name: Option<String>,
        /// What this session can help with, comma-separated (review, pytest, …).
        #[arg(long, value_delimiter = ',')]
        offers: Option<Vec<String>>,
        /// Join topics, comma-separated: messages to `topic:<t>` reach this session.
        #[arg(long, value_delimiter = ',')]
        join: Option<Vec<String>>,
        /// Leave topics, comma-separated.
        #[arg(long, value_delimiter = ',')]
        leave: Option<Vec<String>>,
    },
    /// Fleets of agents: `belay fleet up` runs one from a fleet file. Alone, it lists the
    /// fleets and the live belay sessions on this machine, grouped by repo.
    Fleet {
        #[command(subcommand)]
        cmd: Option<FleetCmd>,
        /// Also list fleets that are down and pruned.
        #[arg(long)]
        all: bool,
    },
    /// Roll the files back to a snapshot belay took at the end of an agent turn
    /// (`t-N`; `t-0` is the start). Alone, it lists the snapshots. Your index, HEAD,
    /// stash and branches are not touched. Run it yourself, in your own terminal.
    Undo {
        /// A snapshot: `t-N`, or `u-N` (the files as they were before undo N).
        id: Option<String>,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
        /// Do not ask before writing the files.
        #[arg(long)]
        yes: bool,
    },
    /// Sandboxes: run a command, an agent session, or a headless task on a clone of this
    /// repo with a private home and limits; its changes land on branch
    /// `belay/sandbox/<id>` for you to merge.
    Sandbox {
        #[command(subcommand)]
        cmd: sandbox::SandboxCmd,
    },
    /// What belay would have done in an existing Claude Code transcript, or in all of them.
    Retro {
        #[arg(required_unless_present = "all")]
        transcript: Option<PathBuf>,
        /// Every session transcript in the Claude Code projects dir.
        #[arg(long, conflicts_with = "transcript")]
        all: bool,
        /// Projects dir for --all (default: $CLAUDE_CONFIG_DIR/projects or ~/.claude/projects).
        #[arg(long, requires = "all")]
        projects: Option<PathBuf>,
        /// With --all, keep projects whose working directory was under /tmp (scratch work,
        /// such as belay's own test runs). They are left out by default.
        #[arg(long, requires = "all")]
        include_tmp: bool,
        /// For one transcript: every section, including empty ones.
        #[arg(long)]
        full: bool,
        /// With --all: totals only, no paths or session names, safe to share.
        #[arg(long, requires = "all")]
        summary: bool,
    },
    /// The live belay sessions on this machine: what each is doing and how it is going
    /// (their cards). Agents may run it too. `--history` lists every recorded session.
    Sessions {
        /// Also sessions that ended in the last day.
        #[arg(long)]
        all: bool,
        /// Cards as JSON, for scripts and agents.
        #[arg(long)]
        json: bool,
        /// Every recorded session: start, duration, mode, calls, project.
        #[arg(long, conflicts_with_all = ["all", "json"])]
        history: bool,
        /// Delete sessions with no activity for this many days (live sessions are kept).
        #[arg(long, value_name = "DAYS")]
        prune: Option<u64>,
    },
    /// Wrapper belay puts around a test command (see run.rs). Keeps the exit code.
    #[command(hide = true)]
    Run {
        #[arg(long)]
        id: String,
        /// The daemon's socket, when the agent's shell has no BELAY_SOCKET (plugin).
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        sh: String,
    },
    /// Hook entry point. The agent calls this. Always exits 0.
    #[command(hide = true)]
    Hook { event: String },
    /// Session daemon. The launcher starts this.
    #[command(hide = true)]
    Daemon {
        #[arg(long)]
        session_dir: PathBuf,
        #[arg(long)]
        parent_pid: Option<i32>,
        #[arg(long, default_value = "claude-code")]
        agent: String,
        #[arg(long)]
        idle_exit_minutes: Option<u64>,
        /// Continue the conversation an earlier session ran (U8): its directory.
        #[arg(long)]
        resume_from: Option<PathBuf>,
        /// The resumed conversation's id, with `--resume-from`.
        #[arg(long)]
        resume_conversation: Option<String>,
        /// The agent's picker chooses the conversation: resolve it at its start.
        #[arg(long)]
        resume_picker: bool,
    },
}

#[derive(Subcommand)]
enum FleetCmd {
    /// Start a fleet in this repo, from a fleet file (TOML) or a preset.
    Up {
        /// The fleet file (TOML). See docs/guide.md, Fleets.
        file: Option<PathBuf>,
        /// solo, peers, lead, pipeline, review, best-of, specialists, pool, tree.
        #[arg(long)]
        preset: Option<String>,
        /// 0.1's mode: accepted and ignored, with a warning.
        #[arg(long, hide = true)]
        mode: Option<String>,
    },
    /// A fleet's sessions: state, pid, turns, restarts, worktree.
    Status {
        /// A fleet id or prefix (default: the newest one that is up).
        fleet: Option<String>,
    },
    /// Stop one session of a fleet (it is not restarted).
    Stop {
        slot: String,
        #[arg(long)]
        fleet: Option<String>,
    },
    /// Start another session of a role (within its maximum).
    Spawn {
        role: String,
        #[arg(long)]
        fleet: Option<String>,
    },
    /// Run `n` sessions of a stream role.
    Scale {
        role: String,
        n: u32,
        #[arg(long)]
        fleet: Option<String>,
    },
    /// Stop every session and the hub.
    Down { fleet: Option<String> },
    /// What a fleet did: tasks, sessions, spend, and the merge plan.
    Report { fleet: Option<String> },
    /// The order to merge finished worktrees in, and where they conflict. With `--yes`
    /// (fleet down), commit each finished worktree's changes on its own branch.
    Merge {
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        fleet: Option<String>,
    },
    /// Stop handing out tasks and starting sessions; running turns finish.
    Pause { fleet: Option<String> },
    /// Hand out work again (after `pause`, a budget, or the usage limit).
    Resume { fleet: Option<String> },
    /// Remove worktrees of fleets that are down and have nothing unsaved in them.
    /// Without `--yes`, only lists them. Run it yourself, in your own terminal.
    Prune {
        #[arg(long)]
        yes: bool,
        /// Also delete those fleets' branches that are merged into HEAD (`git branch
        /// -d`). Branches with commits HEAD lacks are listed and kept.
        #[arg(long)]
        branches: bool,
    },
    /// The hub process (started by `up`).
    #[command(hide = true)]
    Hub {
        #[arg(long)]
        dir: PathBuf,
        /// Start again from the ledger (`belay fleet restart`).
        #[arg(long)]
        resume: bool,
    },
    /// Start a fleet's hub again after it stopped: stream sessions resume their
    /// conversations, and their tasks go back to the queue.
    Restart { fleet: Option<String> },
    /// Replay a fleet's recorded events: with its own file (do the records match?),
    /// or with another (what would it have decided?).
    Replay {
        fleet: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Put a task on the board. With one role that may take it, it goes to that role.
    Add {
        title: String,
        /// More detail for the session that does it.
        #[arg(long, default_value = "")]
        body: String,
        /// Read the body from a file (`-`: stdin): no shell quoting to get wrong.
        #[arg(long, conflicts_with = "body")]
        body_file: Option<PathBuf>,
        /// Tasks that must be done first, comma-separated (t-1,t-2).
        #[arg(long, value_delimiter = ',')]
        depends: Vec<String>,
        /// Paths the task expects to touch (repeat for more).
        #[arg(long)]
        scope: Vec<String>,
        /// The role that should do it.
        #[arg(long)]
        role: Option<String>,
        /// The command that decides it is done (default: the fleet file's `check`).
        #[arg(long)]
        check: Option<String>,
    },
    /// Give a task to a role or a session.
    Assign { task: String, to: String },
    /// Take a ready task: the given one, or the next one for this session's role.
    Claim { task: Option<String> },
    /// Finish a task, with a one-line summary.
    Done { task: String, summary: String },
    /// Say a task cannot be done, and why. Its creator is told.
    Blocked { task: String, reason: String },
    /// Send a task under review back to work, and say why (its creator or the user).
    Reject { task: String, reason: String },
    /// Withdraw a task and its parts that are not done: a duplicate, a garbled body,
    /// work done elsewhere (its creator, the creator of a task it is part of, or the
    /// user). Tasks that wait for it are blocked.
    Drop { task: String, reason: String },
    /// One task in full: title, body, state, and outcome.
    Show { task: String },
    /// The board (one line per task; `belay task show` for the rest).
    List,
}

#[derive(Clone, Copy, ValueEnum)]
enum AuditFormat {
    Text,
    Markdown,
    Json,
}

/// `belay pause` and `belay resume`: the user's kill switch, sent to the live daemon.
fn pause(feature: Option<String>, session: Option<String>, resume: bool) -> Result<i32> {
    let dir = sessions::resolve(session.as_deref(), true)?;
    let req = ipc::Request::Pause {
        feature,
        resume,
        token: paths::read_token(&dir),
    };
    let reply = ipc::send(&paths::socket_path(&dir), &req, Duration::from_secs(1))
        .with_context(|| format!("session {} is not live", dir.display()))?;
    let said = reply.message.unwrap_or_default();
    println!("{said}");
    Ok(i32::from(
        !(said.starts_with("Paused") || said.starts_with("Resumed")),
    ))
}

fn fleet_cmd(c: FleetCmd) -> Result<i32> {
    let text = match c {
        FleetCmd::Up { file, preset, mode } => {
            if mode.is_some() {
                eprintln!("belay: --mode is ignored: belay has no modes now. Fleet sessions run with guard = deny and commons = on.");
            }
            let preset = match preset {
                Some(p) => Some(
                    serde_json::from_value(serde_json::Value::String(p.clone()))
                        .map_err(|_| anyhow::anyhow!("no preset `{p}`"))?,
                ),
                None => None,
            };
            let id = hub::up(file.as_deref(), preset)?;
            format!(
                "fleet {id} is up.\n{}",
                hub::status(Some(&id)).unwrap_or_default()
            )
        }
        FleetCmd::Status { fleet } => hub::status(fleet.as_deref())?,
        FleetCmd::Stop { slot, fleet } => {
            hub::ask(fleet.as_deref(), hub::HubRequest::Stop { slot })?
        }
        FleetCmd::Spawn { role, fleet } => {
            hub::ask(fleet.as_deref(), hub::HubRequest::Spawn { role })?
        }
        FleetCmd::Scale { role, n, fleet } => {
            hub::ask(fleet.as_deref(), hub::HubRequest::Scale { role, n })?
        }
        FleetCmd::Down { fleet } => hub::down(fleet.as_deref())?,
        FleetCmd::Report { fleet } => hub::report(fleet.as_deref())?,
        FleetCmd::Merge { yes, fleet } => hub::merge(fleet.as_deref(), yes)?,
        FleetCmd::Pause { fleet } => hub::ask(fleet.as_deref(), hub::HubRequest::Pause)?,
        FleetCmd::Resume { fleet } => hub::ask(fleet.as_deref(), hub::HubRequest::Resume)?,
        FleetCmd::Prune { yes, branches } => {
            if yes {
                paths::users_only("`belay fleet prune --yes`")?;
            }
            hub::prune(yes, branches)?
        }
        FleetCmd::Hub { dir, resume } => {
            hub::run_hub(&dir, resume)?;
            return Ok(0);
        }
        FleetCmd::Restart { fleet } => hub::restart(fleet.as_deref())?,
        FleetCmd::Replay { fleet, file } => hub::replay_cmd(fleet.as_deref(), file.as_deref())?,
    };
    println!("{}", text.trim_end());
    Ok(0)
}

/// `belay config` actions.
#[derive(Subcommand)]
enum ConfigCmd {
    /// Rewrite 0.1 keys and values in the config file (`fleet`, `learn`, `plugin_mode`,
    /// `look`), keeping comments and order. Shows the diff and keeps a backup.
    Migrate {
        /// Write without asking.
        #[arg(long)]
        yes: bool,
    },
}

/// `belay memory` actions.
#[derive(Subcommand)]
enum MemoryCmd {
    /// This project's memories and yours, newest first (the default).
    List {
        /// Every project's.
        #[arg(long)]
        all: bool,
    },
    /// The memories that best match some words.
    Search { query: String },
    /// Keep a memory. From an agent, it is the agent's until you approve it.
    Add {
        text: String,
        /// fact, decision, gotcha, fix, handoff, or note (default).
        #[arg(long)]
        kind: Option<String>,
        /// A tag (repeat for more).
        #[arg(long)]
        tag: Vec<String>,
        /// A file it is about (repeat for more).
        #[arg(long)]
        file: Vec<String>,
        /// project (default) or user (every project).
        #[arg(long)]
        scope: Option<String>,
    },
    /// Every field of one memory.
    Show { id: String },
    /// Retire a memory (yours to run).
    Drop { id: String },
    /// Vouch for an agent's memory (yours to run).
    Approve { id: String },
    /// Copy notes, fixes, findings, and handoffs into the memory store, once.
    Migrate,
}

/// `belay skill` actions.
#[derive(Subcommand)]
enum SkillCmd {
    /// Fetch a skill and show its name, files, warnings, and digest. Installs nothing.
    Preview { url: String },
    /// Install a skill after a preview. From an agent, it is staged for you to approve.
    Add {
        /// A GitHub tree or blob URL, a raw SKILL.md URL, a .zip or .tar.gz, or a path.
        url: String,
        /// user (default: every project) or project (this repo's .claude/skills).
        #[arg(long)]
        scope: Option<String>,
        /// Install under this name instead of the one in SKILL.md.
        #[arg(long)]
        name: Option<String>,
        /// Do not ask before installing.
        #[arg(long)]
        yes: bool,
    },
    /// Install a staged skill (yours to run).
    Approve {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// Drop a staged skill (yours to run).
    Reject { name: String },
    /// Uninstall a skill (yours to run).
    Remove {
        name: String,
        #[arg(long)]
        scope: Option<String>,
    },
    /// Installed and staged skills, with drift from what was approved (the default).
    List,
    /// Fetch installed skills again; a change waits for your approval.
    Update { name: Option<String> },
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    // The shim must never fail the agent: handle it before clap, whose errors exit 2, and
    // exit 2 from a PreToolUse hook blocks the tool call. Extra or missing arguments (a
    // newer settings file, an older binary) still exit 0 (rule 1).
    if argv.get(1).map(String::as_str) == Some("hook") {
        let event = argv.get(2).cloned().unwrap_or_default();
        // `--plugin`: called by the Claude Code plugin, with no launcher (plugin.rs).
        let from_plugin = argv.iter().skip(3).any(|a| a == "--plugin");
        config::quiet();
        // Exit 0 even if something panics.
        let _ = std::panic::catch_unwind(|| {
            if from_plugin {
                plugin::run(&event)
            } else {
                shim::run(&event)
            }
        });
        std::process::exit(0);
    }
    // `belay reap <pgid>`: the watchdog `belay run` starts (see run.rs). No clap: it must
    // start fast and never fail loudly.
    if argv.get(1).map(String::as_str) == Some("reap") {
        std::process::exit(run::reap(argv.get(2).map_or("", String::as_str)));
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // `belay run` stands in for the agent's command: if its other arguments do not
            // parse, still run the command, unfiltered.
            if argv.get(1).map(String::as_str) == Some("run") {
                if let Some(sh) = arg_value(&argv, "--sh") {
                    std::process::exit(run::run("", None, &sh));
                }
            }
            e.exit()
        }
    };
    if let Cmd::Run { id, socket, sh } = &cli.cmd {
        std::process::exit(run::run(id, socket.as_deref(), sh));
    }
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("belay: {e:#}");
            std::process::exit(1);
        }
    }
}

/// Whether `dir` is a direct child of the sessions dir (`--session` also takes a path;
/// purge must not delete an arbitrary directory).
/// The session this shell belongs to (`belay claude` sets `BELAY_SESSION_DIR` in the
/// agent's shell): commons requests say who sends them.
fn my_session() -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("BELAY_SESSION_DIR")?);
    Some(dir.file_name()?.to_string_lossy().to_string())
}

fn in_sessions_dir(dir: &std::path::Path, root: &std::path::Path) -> bool {
    match (std::fs::canonicalize(dir), std::fs::canonicalize(root)) {
        (Ok(d), Ok(r)) => d.parent() == Some(r.as_path()),
        _ => false,
    }
}

/// The value of `--name value` or `--name=value` in raw arguments.
fn arg_value(argv: &[String], name: &str) -> Option<String> {
    let eq = format!("{name}=");
    argv.iter().enumerate().find_map(|(i, a)| {
        if a == name {
            argv.get(i + 1).cloned()
        } else {
            a.strip_prefix(&eq).map(str::to_string)
        }
    })
}

fn run(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Cmd::Claude { mode, args } => {
            if mode.is_some() {
                launcher::warn_mode_flag();
            }
            launcher::run_claude(args)
        }
        Cmd::Launch { agent, args } => launcher::launch(&agent, args),
        Cmd::Codex { .. } => bail!("`belay codex` is not built yet"),
        Cmd::Init { agent } => bail!("`belay init {agent}` is not built yet"),
        Cmd::Sandbox { cmd } => sandbox::cli(cmd),
        Cmd::Undo { id, session, yes } => {
            let dir = sessions::resolve(session.as_deref(), false)?;
            undo::run(&dir, id.as_deref(), yes)
        }
        Cmd::Mode { .. } => {
            println!("belay has no modes now: it acts within what the agent supports. To stop it, run `belay pause` (or `belay pause guard` for one feature); `belay resume` undoes it.");
            Ok(1)
        }
        Cmd::Pause { feature, session } => pause(feature, session, false),
        Cmd::Resume { feature, session } => pause(feature, session, true),
        Cmd::Allow { id, session } => {
            let dir = sessions::resolve(session.as_deref(), true)?;
            let req = ipc::Request::Allow {
                action: id,
                token: paths::read_token(&dir),
            };
            let reply = ipc::send(&paths::socket_path(&dir), &req, Duration::from_secs(1))
                .with_context(|| format!("session {} is not live", dir.display()))?;
            let said = reply.message.unwrap_or_default();
            println!("{said}");
            Ok(if said.starts_with("belay: refused") {
                1
            } else {
                0
            })
        }
        Cmd::Report { session, full } => {
            let dir = sessions::resolve(session.as_deref(), false)?;
            // A resumed session: each earlier run first, one header each (U8).
            let runs = store::read_chain(&dir)?;
            for (n, run) in runs.iter().enumerate() {
                let label = run
                    .dir
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                if runs.len() > 1 {
                    if n > 0 {
                        println!();
                    }
                    println!("Run {} of {} of this conversation:", n + 1, runs.len());
                }
                print!("{}", report::render(&label, &run.entries, true, full));
            }
            if let Some(a) = std::fs::read_to_string(dir.join("audit.json"))
                .ok()
                .and_then(|t| serde_json::from_str::<belay_core::audit::Report>(&t).ok())
            {
                println!(
                    "{}: {} (`belay audit --session`)",
                    audit::LABEL,
                    a.one_line()
                );
            }
            let entries: Vec<_> = runs.into_iter().flat_map(|r| r.entries).collect();
            print!(
                "{}",
                flags::report_lines(&entries, &store::read_flags(&dir), full)
            );
            Ok(0)
        }
        Cmd::Brief {
            session,
            markdown,
            json,
        } => {
            let format = match (markdown, json) {
                (true, _) => brief::Format::Markdown,
                (_, true) => brief::Format::Json,
                _ => brief::Format::Text,
            };
            brief::run(session.as_deref(), format)
        }
        Cmd::Show { id, session } => {
            let dir = sessions::resolve(session.as_deref(), false)?;
            // Output ids go on across resumed runs (U8): the newest run that has it.
            let runs = store::read_chain(&dir)?;
            let (run, hash) = runs
                .iter()
                .rev()
                .find_map(|r| Some((r, report::find_output_blob(&r.entries, &id)?)))
                .ok_or_else(|| anyhow::anyhow!("no output {id} in {}", dir.display()))?;
            print!("{}", store::read_blob(&run.dir, &hash)?);
            Ok(0)
        }
        Cmd::Flag { words, session } => {
            println!("{}", flags::flag(words, session.as_deref())?);
            Ok(0)
        }
        Cmd::Watch { session, verbose } => {
            watch::run(session.as_deref(), verbose)?;
            Ok(0)
        }
        Cmd::Status => {
            config::quiet();
            // A status line must never show an error: print nothing instead. Under the
            // launcher the session is in the environment; under the plugin, Claude Code
            // passes the session id on stdin.
            let dir = std::env::var_os("BELAY_SESSION_DIR")
                .map(PathBuf::from)
                .or_else(|| {
                    use std::io::{IsTerminal, Read};
                    if std::io::stdin().is_terminal() {
                        return None;
                    }
                    let mut input = String::new();
                    std::io::stdin().read_to_string(&mut input).ok()?;
                    let v: serde_json::Value = serde_json::from_str(&input).ok()?;
                    paths::conversation_dir(v.get("session_id")?.as_str()?)
                });
            if let Some(dir) = dir {
                if let Ok(entries) = store::read_entries(&dir) {
                    let mut line = report::status_line(&entries);
                    // In a fleet session: messages waiting for the next hook (G6).
                    let inbox = std::env::var("BELAY_FLEET_RULES")
                        .ok()
                        .and_then(|r| {
                            serde_json::from_str::<belay_core::fleet::scope::RulesFile>(&r).ok()
                        })
                        .and_then(|r| r.inbox);
                    if let Some(inbox) = inbox {
                        let n = hub::unread_for_session(std::path::Path::new(&inbox), &entries);
                        if n > 0 {
                            line.push_str(&format!(
                                " · {n} new fleet message{}",
                                if n == 1 { "" } else { "s" }
                            ));
                        }
                    }
                    println!("{line}");
                }
            }
            Ok(0)
        }
        Cmd::Audit {
            range,
            format,
            session,
        } => {
            let (report, title) = match session {
                Some(spec) => {
                    let dir = sessions::resolve(Some(&spec), false)?;
                    let text = std::fs::read_to_string(dir.join("audit.json")).with_context(|| {
                        format!(
                            "no audit in session {} (set `experimental_audit = true` in the config)",
                            dir.display()
                        )
                    })?;
                    let id = dir.file_name().map(|n| n.to_string_lossy().to_string());
                    (
                        serde_json::from_str(&text)?,
                        format!("session {}", id.unwrap_or_default()),
                    )
                }
                None => {
                    let root = audit::repo_root(&std::env::current_dir()?)?;
                    let scratch = std::env::temp_dir();
                    let (from, to) = audit::resolve_range(&root, range.as_deref(), &scratch)?;
                    let title = range.unwrap_or_else(|| "HEAD..working tree".into());
                    let policy = config::load_policy(&root).0;
                    (
                        audit::run(&root, &from, &to, &|p| policy.audit_ignores(p))?,
                        title,
                    )
                }
            };
            match format {
                AuditFormat::Text => print!("{}", audit::render_text(&report, &title)),
                AuditFormat::Markdown => print!("{}", audit::render_markdown(&report, &title)),
                AuditFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
            }
            Ok(0)
        }
        Cmd::Export {
            session,
            no_outputs,
            action,
            fixture,
        } => {
            let dir = sessions::resolve(session.as_deref(), false)?;
            if let (Some(id), Some(out)) = (action, fixture) {
                for f in export::fixture(&dir, &id, &out)? {
                    println!("wrote {}", f.display());
                }
                println!("{}", export::FIXTURE_WARNING);
            } else {
                println!("{}", export::export(&dir, !no_outputs)?);
            }
            Ok(0)
        }
        Cmd::Purge {
            session,
            all,
            older_than,
        } => {
            let root = paths::sessions_dir();
            let removed = match (session, all, older_than) {
                (Some(spec), _, _) => {
                    let dir = sessions::resolve(Some(&spec), false)?;
                    if !in_sessions_dir(&dir, &root) {
                        bail!(
                            "{} is not a stored belay session; not deleting it",
                            dir.display()
                        );
                    }
                    if sessions::is_live(&dir) {
                        bail!("session {} is live; end it first", dir.display());
                    }
                    undo::delete_refs(&dir);
                    std::fs::remove_dir_all(&dir)
                        .with_context(|| format!("remove {}", dir.display()))?;
                    vec![dir]
                }
                (None, true, _) => sessions::prune(&root, 0, u64::MAX / 2)?,
                (None, false, Some(days)) => sessions::prune(&root, days, paths::now_ms() / 1000)?,
                (None, false, None) => {
                    bail!("say what to purge: --session ID, --older-than DAYS, or --all")
                }
            };
            println!("{} sessions removed", removed.len());
            Ok(0)
        }
        Cmd::Install {
            parts,
            from,
            force,
            yes,
        } => install::install(&install::Opts {
            parts: parts.or(install::Parts {
                plugin: true,
                ..Default::default()
            }),
            from,
            force,
            yes,
        }),
        Cmd::Uninstall { parts, yes } => install::uninstall(&install::Opts {
            parts: parts.or(install::Parts {
                plugin: true,
                statusline: true,
                launcher: false,
            }),
            from: None,
            force: false,
            yes,
        }),
        Cmd::Doctor { json } => {
            let (report, code) = doctor::run(json);
            print!("{report}");
            Ok(code)
        }
        Cmd::Review => {
            print!("{}", flags::review(&paths::list_sessions()));
            Ok(0)
        }
        Cmd::Why { id, session } => {
            let dir = sessions::resolve(session.as_deref(), false)?;
            let entries = store::chain_entries(&store::read_chain(&dir)?);
            print!("{}", report::why_across(&entries, &id, &fleet::peer_entry)?);
            Ok(0)
        }
        Cmd::Card {
            task,
            note,
            name,
            offers,
            join,
            leave,
        } => {
            let Some(sock) = std::env::var_os("BELAY_SOCKET").map(PathBuf::from) else {
                println!(
                    "belay card works inside a belay session; `belay sessions` shows every card."
                );
                return Ok(1);
            };
            let req = ipc::Request::Card {
                name,
                task,
                note,
                offers,
                join,
                leave,
                handoff: None,
                from: my_session(),
            };
            let reply = ipc::send(&sock, &req, Duration::from_secs(2))?;
            let text = reply.message.unwrap_or_default();
            println!("{}", text.trim_end());
            Ok(if text.starts_with("belay: refused") {
                1
            } else {
                0
            })
        }
        Cmd::Fleet { cmd: None, all } => {
            print!("{}{}", hub::list(all), fleet::view_default());
            Ok(0)
        }
        Cmd::Fleet { cmd: Some(c), .. } => fleet_cmd(c),
        Cmd::Msg {
            to,
            text,
            kind,
            fleet,
        } => {
            let said = send_message(fleet, to, kind, text)?;
            println!("{}", said.trim_end());
            Ok(if said.starts_with("belay: refused") {
                1
            } else {
                0
            })
        }
        Cmd::Note { text } => {
            println!("{}", learnstore::note(&text)?);
            Ok(0)
        }
        Cmd::Notes { verb, id, all } => {
            let action = match (verb.as_deref(), id.as_deref()) {
                (None, _) => None,
                (Some(v @ ("approve" | "drop")), Some(id)) => Some((v, id)),
                _ => anyhow::bail!("use `belay notes approve <id>` or `belay notes drop <id>`"),
            };
            let text = learnstore::notes(action, all)?;
            println!("{}", text.trim_end());
            Ok(0)
        }
        Cmd::Fixes { verb, all } => {
            match verb.as_deref() {
                None => print!("{}", learnstore::fixes(all)?),
                Some("forget") => println!("{}", learnstore::forget(all)?),
                Some(v) => anyhow::bail!("unknown `belay fixes {v}`; use `belay fixes forget`"),
            }
            Ok(0)
        }
        Cmd::Inbox { fleet } => {
            println!("{}", show_inbox(fleet)?.trim_end());
            Ok(0)
        }
        Cmd::Task { cmd, fleet } => {
            let verb = match cmd {
                TaskCmd::Add {
                    title,
                    body,
                    body_file,
                    depends,
                    scope,
                    role,
                    check,
                } => hub::TaskVerb::Add {
                    title,
                    body: match body_file {
                        Some(p) if p.as_os_str() == "-" => {
                            let mut b = String::new();
                            std::io::Read::read_to_string(&mut std::io::stdin(), &mut b)?;
                            b
                        }
                        Some(p) => std::fs::read_to_string(&p)
                            .with_context(|| format!("read {}", p.display()))?,
                        None => body,
                    },
                    depends_on: depends,
                    scope,
                    role,
                    check,
                },
                TaskCmd::Assign { task, to } => hub::TaskVerb::Assign { task, to },
                TaskCmd::Claim { task } => hub::TaskVerb::Claim { task },
                TaskCmd::Done { task, summary } => hub::TaskVerb::Done { task, summary },
                TaskCmd::Blocked { task, reason } => hub::TaskVerb::Blocked { task, reason },
                TaskCmd::Reject { task, reason } => hub::TaskVerb::Reject { task, reason },
                TaskCmd::Drop { task, reason } => hub::TaskVerb::Drop { task, reason },
                TaskCmd::Show { task } => hub::TaskVerb::Show { task },
                TaskCmd::List => hub::TaskVerb::List,
            };
            let text = hub::task(fleet.as_deref(), verb)?;
            println!("{}", text.trim_end());
            // A refusal is an error to the caller (an agent's script, or the user).
            Ok(if text.starts_with("belay: refused") {
                1
            } else {
                0
            })
        }
        Cmd::Retro {
            transcript,
            all,
            projects,
            include_tmp,
            full,
            summary,
        } => {
            match transcript {
                Some(t) if !all => print!("{}", retro::run(&t, full)?),
                _ => {
                    let root = projects.unwrap_or_else(paths::claude_projects_dir);
                    print!("{}", retro::run_all(&root, 5, include_tmp, summary)?);
                }
            }
            Ok(0)
        }
        Cmd::Sessions {
            prune: Some(days), ..
        } => {
            let now = paths::now_ms() / 1000;
            let removed = sessions::prune(&paths::sessions_dir(), days, now)?;
            for d in &removed {
                println!("removed {}", d.display());
            }
            println!("{} sessions removed", removed.len());
            Ok(0)
        }
        Cmd::Handoff { text } => {
            if let Some(text) = text {
                let Some(sock) = std::env::var_os("BELAY_SOCKET").map(PathBuf::from) else {
                    println!("belay handoff \"…\" works inside a belay session. For every session here, use `belay note`.");
                    return Ok(1);
                };
                let req = ipc::Request::Card {
                    name: None,
                    task: None,
                    note: None,
                    offers: None,
                    join: None,
                    leave: None,
                    handoff: Some(text),
                    from: my_session(),
                };
                let reply = ipc::send(&sock, &req, Duration::from_secs(2))?;
                let said = reply.message.unwrap_or_default();
                if said.starts_with("belay: refused") {
                    println!("{}", said.trim_end());
                    return Ok(1);
                }
                println!("Left for the next session in this repo (it sees it at start).");
                return Ok(0);
            }
            let cards = commons::read_cards(&paths::sessions_dir());
            let project = learnstore::project_of(&std::env::current_dir()?);
            let me = std::env::var("BELAY_SESSION_DIR")
                .ok()
                .and_then(|d| {
                    std::path::Path::new(&d)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                })
                .unwrap_or_default();
            match commons::latest_handoff(&cards, &me, &project, paths::now_ms()) {
                Some(h) => println!("{}", belay_core::commons::handoff_text(&h, paths::now_ms())),
                None => println!(
                    "No handoff for this repo: no session here ended in the last week with one."
                ),
            }
            Ok(0)
        }
        Cmd::Share {
            text,
            kind,
            tag,
            file,
            code_file,
        } => {
            let code = match code_file {
                Some(p) if p.as_os_str() == "-" => {
                    let mut b = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut b)?;
                    Some(b)
                }
                Some(p) => Some(
                    std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?,
                ),
                None => None,
            };
            let said = match std::env::var_os("BELAY_SOCKET").map(PathBuf::from) {
                Some(sock) => {
                    let req = ipc::Request::Share {
                        kind,
                        text,
                        tags: tag,
                        files: file,
                        code,
                        from: my_session(),
                    };
                    ipc::send(&sock, &req, Duration::from_secs(5))?
                        .message
                        .unwrap_or_default()
                }
                None => {
                    findings::share_as_user(kind.as_deref(), &text, &tag, &file, code.as_deref())?
                }
            };
            println!("{}", said.trim_end());
            Ok(if said.starts_with("belay: refused") {
                1
            } else {
                0
            })
        }
        Cmd::Ask { query, here, json } => {
            let said = match std::env::var_os("BELAY_SOCKET").map(PathBuf::from) {
                Some(sock) => {
                    let req = ipc::Request::Ask {
                        query,
                        here,
                        json,
                        from: my_session(),
                    };
                    ipc::send(&sock, &req, Duration::from_secs(5))?
                        .message
                        .unwrap_or_default()
                }
                None => findings::ask_as_user(&query, here, json)?,
            };
            print!("{said}");
            Ok(0)
        }
        Cmd::Mcp => mcp::run(),
        Cmd::Skill { action } => {
            let said = match action.unwrap_or(SkillCmd::List) {
                SkillCmd::Preview { url } => skillstore::preview(&url)?,
                SkillCmd::Add {
                    url,
                    scope,
                    name,
                    yes,
                } => skillstore::add(&url, scope.as_deref(), name.as_deref(), yes)?,
                SkillCmd::Approve { name, yes } => skillstore::approve(&name, yes)?,
                SkillCmd::Reject { name } => skillstore::reject(&name)?,
                SkillCmd::Remove { name, scope } => skillstore::remove(&name, scope.as_deref())?,
                SkillCmd::List => skillstore::list()?,
                SkillCmd::Update { name } => skillstore::update(name.as_deref())?,
            };
            println!("{}", said.trim_end());
            Ok(0)
        }
        Cmd::Config { action: None } => {
            config::show()?;
            Ok(0)
        }
        Cmd::Config {
            action: Some(ConfigCmd::Migrate { yes }),
        } => {
            config::migrate::run(yes)?;
            Ok(0)
        }
        Cmd::Memory { action } => {
            let said = match action.unwrap_or(MemoryCmd::List { all: false }) {
                MemoryCmd::List { all } => memorystore::list(all)?,
                MemoryCmd::Search { query } => memorystore::search(&query)?,
                MemoryCmd::Add {
                    text,
                    kind,
                    tag,
                    file,
                    scope,
                } => memorystore::add(&text, kind.as_deref(), &tag, &file, scope.as_deref())?,
                MemoryCmd::Show { id } => memorystore::show(&id)?,
                MemoryCmd::Drop { id } => memorystore::drop(&id)?,
                MemoryCmd::Approve { id } => memorystore::approve(&id)?,
                MemoryCmd::Migrate => memorystore::migrate()?,
            };
            println!("{}", said.trim_end());
            Ok(0)
        }
        Cmd::Findings {
            verb,
            ids,
            all,
            json,
        } => {
            match verb.as_deref() {
                None => print!("{}", findings::list(all, json)?),
                Some("drop") if !ids.is_empty() => println!("{}", findings::drop_ids(&ids)?),
                _ => anyhow::bail!("use `belay findings` or `belay findings drop f-N`"),
            }
            Ok(0)
        }
        Cmd::Sessions { history: true, .. } => {
            print!("{}", sessions::list(&paths::sessions_dir()));
            Ok(0)
        }
        Cmd::Sessions { all, json, .. } => {
            print!("{}", commons::board_default(all, json));
            Ok(0)
        }
        Cmd::Daemon {
            session_dir,
            parent_pid,
            agent,
            idle_exit_minutes,
            resume_from,
            resume_conversation,
            resume_picker,
        } => {
            let resume = match resume_from {
                Some(dir) => daemon::Resume::From {
                    dir,
                    conversation: resume_conversation,
                },
                None if resume_picker => daemon::Resume::Picker,
                None => daemon::Resume::No,
            };
            daemon::run(session_dir, parent_pid, &agent, idle_exit_minutes, resume)?;
            Ok(0)
        }
        Cmd::Hook { .. } | Cmd::Run { .. } => unreachable!("handled in main"),
    }
}

/// `belay msg`: the fleet's hub for fleet addresses, else the commons (C2).
fn send_message(
    fleet: Option<String>,
    to: String,
    kind: Option<String>,
    text: String,
) -> Result<String> {
    let my_fleet = std::env::var("BELAY_FLEET").ok();
    if fleet.is_some() || to.starts_with('@') || to.starts_with('#') {
        return hub::msg(fleet.as_deref(), to, kind, text);
    }
    let cards = commons::read_cards(&paths::sessions_dir());
    let card = cards
        .iter()
        .find(|(c, live)| *live && c.name == to)
        .map(|(c, _)| c);
    // In a fleet session: the user and the fleet's own sessions go through its hub.
    if let Some(f) = &my_fleet {
        if to == "user" {
            return hub::msg(Some(f), to, kind, text);
        }
        if let Some(c) = card.filter(|c| c.fleet.as_deref() == Some(f.as_str())) {
            let slot = c.slot.clone().unwrap_or(to);
            return hub::msg(Some(f), slot, kind, text);
        }
    }
    // In any session: its daemon sends, names it, and records it.
    if let Some(sock) = std::env::var_os("BELAY_SOCKET").map(PathBuf::from) {
        let req = ipc::Request::Send {
            to,
            kind,
            text,
            from: my_session(),
        };
        let reply = ipc::send(&sock, &req, Duration::from_secs(5))?;
        return Ok(reply.message.unwrap_or_default());
    }
    // From your terminal: a fleet session through its hub (an idle one gets a turn).
    if let Some(c) = card {
        if let (Some(f), Some(slot)) = (&c.fleet, &c.slot) {
            return hub::msg(Some(f), slot.clone(), kind, text);
        }
    }
    if card.is_some() || to.starts_with("topic:") || to == "repo" {
        return commons::send_as_user(&to, kind.as_deref(), &text);
    }
    // A name no card has: a slot of the newest fleet, as before C2.
    match hub::msg(None, to.clone(), kind.clone(), text.clone()) {
        Ok(said) => Ok(said),
        Err(_) => commons::send_as_user(&to, kind.as_deref(), &text),
    }
}

/// `belay inbox`: in a fleet session, the fleet's (and a line for the commons); in any
/// other session, its commons inbox; from your terminal, yours (then marked read), and
/// the newest fleet's if one is up.
fn show_inbox(fleet: Option<String>) -> Result<String> {
    if fleet.is_some() || std::env::var_os("BELAY_FLEET").is_some() {
        let mut out = hub::show_inbox(fleet.as_deref())?;
        if let Some(d) = std::env::var_os("BELAY_SESSION_DIR") {
            let n = commons::unread(&commons::inbox_path(std::path::Path::new(&d)));
            if n > 0 {
                out.push_str(&format!(
                    "\n{n} commons message(s) arrive at the next hook.\n"
                ));
            }
        }
        return Ok(out);
    }
    if let Some(d) = std::env::var_os("BELAY_SESSION_DIR") {
        return Ok(hub::render_inbox_file(&commons::inbox_path(
            std::path::Path::new(&d),
        )));
    }
    let lines = commons::user_lines();
    let mut out = hub::render_inbox_file(&commons::user_inbox());
    commons::mark_user_read(lines);
    if let Ok(f) = hub::show_inbox(None) {
        out.push_str("\n\nThe newest fleet's inbox:\n");
        out.push_str(&f);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_only_deletes_inside_the_sessions_dir() {
        let t = std::env::temp_dir().join(format!("belay-purge-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let root = t.join("sessions");
        std::fs::create_dir_all(root.join("s1")).unwrap();
        std::fs::create_dir_all(t.join("other")).unwrap();
        assert!(in_sessions_dir(&root.join("s1"), &root));
        assert!(!in_sessions_dir(&t.join("other"), &root));
        assert!(!in_sessions_dir(&root, &root));
        assert!(!in_sessions_dir(&root.join("s1/.."), &root));
        let _ = std::fs::remove_dir_all(&t);
    }
}
