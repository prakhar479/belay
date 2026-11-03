//! The command line: every subcommand and its arguments, as clap types.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::{bg, install, sandbox, usage};

#[derive(Parser)]
#[command(
    name = "belay",
    version,
    about = "Your coding agent, with everything it can't give itself.",
    before_help = "Start here: `belay install` · `belay claude` · `belay doctor` · `belay brief`"
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Start Claude Code under belay
    ///
    /// Other arguments, `--help` included, go to `claude`.
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
    /// Start an agent under belay (`belay claude` is the usual way)
    ///
    /// `belay launch claude-code [args…]` is `belay claude`.
    ///
    /// Arguments after the agent go to it, `--help` included; belay's own flags work as
    /// for `belay claude`. Agents: claude-code. `belay help launch` shows this text.
    #[command(disable_help_flag = true)]
    #[command(hide = true)]
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
    /// Pause belay's help in a live session (yours to run)
    ///
    /// Pass one feature, or none to pause them all. Run it yourself; the agent may not.
    /// Features: guard, loops, shape, wrap_tests, verify, memory, awareness, commons.
    Pause {
        feature: Option<String>,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// Turn back on what `belay pause` paused
    ///
    /// One feature, or all of them.
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
    /// Let through one edit the test guard blocked (yours to run)
    ///
    /// The latest one, or action a-N. Run it yourself, for example with `! belay allow`
    /// in Claude Code; the agent may not.
    Allow {
        id: Option<String>,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// Show what happened in a session
    ///
    /// The default is the newest session started here.
    Report {
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
        /// Every section, including empty ones.
        #[arg(long)]
        full: bool,
    },
    /// What a session changed and checked, before you commit
    ///
    /// Built from what actually happened, never from the agent's own summary.
    /// Shows what changed, what was checked, and what to look at before a commit or merge.
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
    /// Print the full original of a shortened output (o-N)
    Show {
        id: String,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// Show why belay did something (a-N, o-N, or an entry number)
    ///
    /// Shows the chain of causes behind an action, an output, or an entry.
    Why {
        id: String,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// Say belay got something wrong
    ///
    /// Name an action (a-N), an output (o-N), an entry number, or `session`;
    /// with no id, the newest warning. The note is optional. A flag lifts nothing.
    /// `belay review` lists flags.
    Flag {
        /// `[TARGET] [NOTE…]`: a first word that is not an id starts the note.
        words: Vec<String>,
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
    },
    /// List what you flagged, newest first
    ///
    /// Each flag shows what it points at, and commands to look closer and to save it
    /// as a fixture.
    Review,
    /// Set belay up with Claude Code
    ///
    /// By default: the plugin, the status line (unless you have your own), and permission
    /// rules for belay's everyday commands. Or name the parts, or the launcher alias.
    /// Shows each change and asks first.
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
    /// Undo `belay install`
    ///
    /// Removes the plugin and belay's status line by default. A status line that is
    /// not belay's is left alone.
    Uninstall {
        #[command(flatten)]
        parts: install::Parts,
        /// Do not ask before changing anything.
        #[arg(long, short)]
        yes: bool,
    },
    /// Check that belay will work here
    ///
    /// Checks the install, Claude Code, the plugin, the sandbox, skills, config, and data.
    /// Each line is ok, warn, or fail; exit 1 only on a fail.
    Doctor {
        /// The same checks as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Keep this machine awake while belay works
    ///
    /// For a time, while a process runs, or while a command runs (default: until Ctrl-C).
    /// It says first what it can hold off here. It records nothing.
    ///
    /// A closed lid can still put a machine to sleep, and background sessions pause then.
    /// On WSL, in Windows run `powercfg /setacvalueindex SCHEME_CURRENT SUB_BUTTONS LIDACTION 0`,
    /// `powercfg /setdcvalueindex SCHEME_CURRENT SUB_BUTTONS LIDACTION 0`, then
    /// `powercfg /setactive SCHEME_CURRENT` (set 1 to put sleep back). On macOS, run
    /// `sudo pmset -a disablesleep 1` (`0` undoes it). On Linux, set `HandleLidSwitch=ignore`
    /// and `HandleLidSwitchExternalPower=ignore` in /etc/systemd/logind.conf.
    /// `belay keep-awake` prints the exact fix for this machine.
    KeepAwake {
        /// How long: `90s`, `45m`, `2h`, `1h30m`.
        #[arg(long = "for", value_name = "DURATION", conflicts_with_all = ["pid", "cmd"])]
        for_: Option<String>,
        /// Hold while this process lives.
        #[arg(long, conflicts_with = "cmd")]
        pid: Option<u32>,
        /// Run this command and hold while it runs; exits with its code.
        #[arg(last = true)]
        cmd: Vec<String>,
    },
    /// Save one session as a cleaned JSON file for a bug report
    ///
    /// Paths, home, user name, and secrets are replaced.
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
    /// Delete recorded sessions
    ///
    /// Removes their records and stored outputs. Live sessions are kept.
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
    /// (Experimental) What a diff changed in tests and checks
    ///
    /// Reads a git diff, or a belay session: assertions and tests, snapshots,
    /// suppressions, check config, dependencies.
    #[command(hide = true)]
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
    /// Print one line for the Claude Code status line
    ///
    /// Says what belay did so far. Prints nothing outside a belay session.
    Status,
    /// Where the model's tokens went, and what it cost
    ///
    /// Totals and an estimated cost from Claude Code's transcripts, tokens spent in loops
    /// and failing runs, and the 5-hour limit.
    Usage {
        /// Since local midnight today.
        #[arg(long, conflicts_with_all = ["week", "since"])]
        today: bool,
        /// The last 7 days, today included (the default).
        #[arg(long, conflicts_with = "since")]
        week: bool,
        /// Since this local date (YYYY-MM-DD).
        #[arg(long)]
        since: Option<String>,
        /// Group the totals by project, session, or day.
        #[arg(long, value_enum, default_value = "day")]
        by: usage::By,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Watch what belay does in a session, live
    ///
    /// The newest live session, or the one named. Ctrl-C stops.
    /// With no live session it says so and exits 1.
    Watch {
        /// A session id or prefix, `-1` (newest), `-2`, …, or a path.
        #[arg(long, allow_hyphen_values = true)]
        session: Option<String>,
        /// Also print minor notices (wrapped runs, retries, summary savings).
        #[arg(long)]
        verbose: bool,
    },
    /// A fleet's task board
    ///
    /// Add, give out, take, finish, or block tasks. In a fleet session, the agent uses
    /// these; belay knows which session asks.
    #[command(hide = true)]
    Task {
        #[command(subcommand)]
        cmd: TaskCmd,
        /// A fleet id or prefix (default: this session's fleet, or the newest up).
        #[arg(long, global = true)]
        fleet: Option<String>,
    },
    /// Send a message to another session or to you
    ///
    /// To any belay session by its card name, to `topic:<t>` (sessions that joined it),
    /// to `repo` (every session in this repo), or to `user`. In a fleet, also `@role`,
    /// `#team`, or a slot. Without --kind: an answer to whoever asked, a question if the
    /// text asks, else a notice (in a fleet, its channels decide).
    #[command(hide = true)]
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
    /// Read messages for this session, or for you
    ///
    /// This session's (shared and fleet), or, for you, yours and the fleet's.
    #[command(hide = true)]
    Inbox {
        /// The fleet's id (default: this session's fleet, else the newest live one)
        #[arg(long)]
        fleet: Option<String>,
    },
    /// Keep a note for this project
    ///
    /// What sessions should know (`belay notes` lists them). A note typed in your
    /// terminal is kept; one from an agent waits for your review.
    Note {
        /// What every new session of this project should know
        text: String,
    },
    /// List this project's notes
    ///
    /// `approve <id>` or `drop <id>` reviews one (yours to run).
    Notes {
        /// approve or drop
        verb: Option<String>,
        /// The note's id (`n-3`)
        id: Option<String>,
        /// Every project's notes.
        #[arg(long)]
        all: bool,
    },
    /// List fixes sessions learned in this project
    ///
    /// Each is a failure, the change, and the run that then passed.
    /// `forget` drops them (yours to run).
    Fixes {
        /// forget
        verb: Option<String>,
        /// Every project's fixes.
        #[arg(long)]
        all: bool,
    },
    /// Publish a finding other sessions can pull with `belay ask`
    ///
    /// A gotcha, a decision, how something works. Text reaches other projects;
    /// `--code-file` only this one.
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
    /// Search what other sessions published
    ///
    /// Also searches fixes they learned. Look here before you dig in.
    Ask {
        /// What you want to know, in words.
        query: String,
        /// This project's findings only.
        #[arg(long)]
        here: bool,
        #[arg(long)]
        json: bool,
    },
    /// List this project's findings
    ///
    /// `drop f-N…` removes them (yours to run).
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
    /// Show the settings belay is using
    ///
    /// Each key, its value, and where it came from (the file or the default).
    /// `belay config migrate` rewrites 0.1 keys in the file.
    Config {
        #[command(subcommand)]
        action: Option<ConfigCmd>,
    },
    /// What belay remembers for this project and you
    ///
    /// List, search, add, show; approve or drop (yours to run); migrate copies notes,
    /// fixes, findings, and handoffs in once.
    Memory {
        #[command(subcommand)]
        action: Option<MemoryCmd>,
    },
    /// Add skills from a URL, safely
    ///
    /// Preview, add, list, update; approve, reject, remove (yours to run).
    /// An agent can only suggest a skill; you approve it.
    Skill {
        #[command(subcommand)]
        action: Option<SkillCmd>,
    },
    /// Let other tools read belay's memory (advanced)
    ///
    /// Runs an MCP server over stdio: memory search and add, the handoff, outputs, and why.
    #[command(hide = true)]
    Mcp,
    /// Leave a note for the next session in this repo
    ///
    /// Use "done: …; open: …; next: …", from inside a session. With no text, shows
    /// what the next session here would get.
    Handoff {
        /// What is done, what is open, what comes next.
        text: Option<String>,
    },
    /// Say what this session is working on
    ///
    /// From inside a belay session. With no flags, shows the card.
    #[command(hide = true)]
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
    /// Run several agents together (advanced)
    ///
    /// `belay fleet up` runs one from a fleet file. Alone, it lists the fleets and the
    /// live belay sessions on this machine, grouped by repo.
    #[command(hide = true)]
    Fleet {
        #[command(subcommand)]
        cmd: Option<FleetCmd>,
        /// Also list fleets that are down and pruned.
        #[arg(long)]
        all: bool,
    },
    /// Roll files back to the end of an earlier agent turn
    ///
    /// Pass `t-N` (`t-0` is the start). Alone, it lists the snapshots. Your index, HEAD,
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
    /// Run a command or agent on a private copy of this repo
    ///
    /// Runs on a clone with a private home and limits; its changes land on branch
    /// `belay/sandbox/<id>` for you to merge.
    Sandbox {
        #[command(subcommand)]
        cmd: sandbox::SandboxCmd,
    },
    /// Keep an agent working after you leave
    ///
    /// An agent keeps working on a prompt in the background, in a sandbox when it can,
    /// and tells you when it is done or needs you.
    Bg {
        #[command(subcommand)]
        cmd: bg::BgCmd,
    },
    /// See what belay would have done in an old transcript
    ///
    /// Reads an existing Claude Code transcript, or all of them.
    #[command(hide = true)]
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
    /// List the live belay sessions on this machine
    ///
    /// What each is doing and how it is going (their cards). Agents may run it too.
    /// `--history` lists every recorded session.
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
pub(crate) enum FleetCmd {
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
pub(crate) enum TaskCmd {
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
pub(crate) enum AuditFormat {
    Text,
    Markdown,
    Json,
}

/// `belay pause` and `belay resume`: the user's kill switch, sent to the live daemon.
#[derive(Subcommand)]
pub(crate) enum ConfigCmd {
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
pub(crate) enum MemoryCmd {
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
pub(crate) enum SkillCmd {
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
