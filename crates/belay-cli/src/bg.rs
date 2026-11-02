//! `belay bg`: a background session that keeps working when the user leaves (BG1).
//!
//! One supervised headless run of the agent, in a sandbox when one can be built here,
//! else in the checkout with the guard on. A detached supervisor (`belay bg run`, BG0's
//! double fork) owns it: it counts tokens, stops at the budget or the deadline at a step
//! boundary, waits out a usage limit and goes on in the same conversation, keeps the
//! machine awake, and tells the user (V3's channels) when it is done or needs them.
//! `belay bg attach` stops it and opens its conversation in the user's terminal.
//!
//! Fleets are frozen, so this is not a fleet: one prompt has no tasks, checks or
//! attempts. It takes from BG0 the detached start and the usage-limit reading.
//!
//! Files, under `<home>/bg/<id>/`: `meta.json` (what was asked), `state.json` (where it
//! is), `log.jsonl` (what the supervisor did, and why), `out.jsonl` (the agent's
//! headless output), `runner.log`, and `stop` while a stop is asked for.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write as _};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};

use crate::connector::{self, Connector};
use crate::daemon::attention;
use crate::{brief, config, hub, keepawake, paths, sandbox, store, watch};

#[cfg(test)]
mod tests;

#[derive(Subcommand)]
pub enum BgCmd {
    /// Start a background session on `prompt`. It runs detached: close the terminal and
    /// it goes on.
    Start {
        prompt: String,
        /// Start from a copy of this conversation; the original stays yours.
        #[arg(long)]
        resume: Option<String>,
        /// Run in a sandbox (the default when one can be built here).
        #[arg(long, conflicts_with = "here")]
        sandbox: bool,
        /// Run in this checkout, with the guard on.
        #[arg(long)]
        here: bool,
        /// Stop past this many tokens (`2M`, `500K`) or dollars (`$5`).
        #[arg(long)]
        budget: Option<String>,
        /// Stop at this local time (`HH:MM`) or after this long (`45m`, `2h`).
        #[arg(long)]
        until: Option<String>,
        /// Let the machine sleep as it would.
        #[arg(long)]
        no_keep_awake: bool,
        /// The model the agent uses.
        #[arg(long)]
        model: Option<String>,
    },
    /// The background sessions on this machine.
    List,
    /// One background session: phase, tokens and cost, the last notice.
    Status { id: String },
    /// Follow a background session's notices as they happen (`belay watch`).
    Watch {
        id: String,
        #[arg(long)]
        verbose: bool,
    },
    /// Stop a background session at the next step. Run it yourself, in your own terminal.
    Stop {
        id: String,
        /// Stop it now, not at the next step.
        #[arg(long)]
        now: bool,
    },
    /// Stop a background session and open its conversation here. Run it yourself.
    Attach {
        id: String,
        /// Stop it now, not at the next step.
        #[arg(long)]
        now: bool,
    },
    /// The supervisor of a background session (started by `belay bg start`).
    #[command(hide = true)]
    Run { id: String },
}

// ---- What a run is, and where it is.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Place {
    /// The user's checkout; the guard is on.
    Here,
    /// A sandbox of the repo; the work lands on `belay/sandbox/<id>`.
    Sandbox { id: String },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    pub tokens: Option<u64>,
    pub usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    pub prompt: String,
    pub cwd: PathBuf,
    pub place: Place,
    /// The user's conversation it started from (a copy: `--fork-session`).
    pub resume: Option<String>,
    pub model: Option<String>,
    pub budget: Budget,
    pub until_ms: Option<u64>,
    pub keep_awake: bool,
    /// `user`, or the belay session of the agent that started it.
    pub by: String,
    pub created_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Starting,
    Working,
    /// A usage limit: the run goes on by itself at `until_ms`.
    Waiting {
        until_ms: u64,
    },
    Done,
    NeedsYou {
        reason: String,
    },
    Stopped {
        reason: String,
    },
}

impl Phase {
    fn ended(&self) -> bool {
        matches!(
            self,
            Phase::Done | Phase::NeedsYou { .. } | Phase::Stopped { .. }
        )
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(flatten)]
    pub phase: Phase,
    /// The supervisor's pid while it runs.
    pub runner: Option<i32>,
    /// The belay session of the latest run.
    pub session: Option<String>,
    /// The agent's conversation (each run after a limit forks the last: the newest).
    pub conversation: Option<String>,
    pub tokens: u64,
    pub cost_usd: f64,
    pub runs: u32,
    /// The agent's final message.
    pub result: Option<String>,
    pub updated_ms: u64,
}

// ---- Pure parts.

/// `--budget`: `$5` or `5$` is dollars; anything else a token count (`2M`, `500K`).
pub fn parse_budget(s: &str) -> Result<Budget> {
    let t = s.trim();
    if let Some(d) = t.strip_prefix('$').or_else(|| t.strip_suffix('$')) {
        let usd: f64 = d
            .trim()
            .parse()
            .ok()
            .filter(|v: &f64| v.is_finite() && *v > 0.0)
            .ok_or_else(|| anyhow!("`{s}` is not a budget like $5 or 2M"))?;
        return Ok(Budget {
            usd: Some(usd),
            tokens: None,
        });
    }
    let n = sandbox::limits::parse_count(t)
        .map_err(|_| anyhow!("`{s}` is not a budget like $5 or 2M"))?;
    if n == 0 {
        bail!("a budget of 0 tokens stops before it starts");
    }
    Ok(Budget {
        tokens: Some(n),
        usd: None,
    })
}

const MINUTE_MS: u64 = 60_000;
const DAY_MINUTES: u64 = 1440;

/// `--until`: `HH:MM` local time (the next one; `minute` is now's local minute of the
/// day), or a duration (`45m`, `2h`, `1h30m`). Unix ms.
pub fn parse_until(s: &str, now: u64, minute: Option<u32>) -> Result<u64> {
    let t = s.trim();
    if let Some((h, m)) = t.split_once(':') {
        let (h, m): (u64, u64) = match (h.parse(), m.parse()) {
            (Ok(h), Ok(m)) if h < 24 && m < 60 => (h, m),
            _ => bail!("`{s}` is not a time like 18:30"),
        };
        let minute = u64::from(minute.context("the local time is not known here")?);
        let mut ahead = (h * 60 + m + DAY_MINUTES - minute) % DAY_MINUTES;
        if ahead == 0 {
            ahead = DAY_MINUTES;
        }
        // To the start of the minute: now is partway into the current one.
        return Ok(now / MINUTE_MS * MINUTE_MS + ahead * MINUTE_MS);
    }
    let d = keepawake::parse_duration(t)
        .map_err(|_| anyhow!("`{s}` is not a time like 18:30 or a duration like 2h"))?;
    Ok(now + d.as_millis() as u64)
}

/// `HH:MM` local time, or UTC when the zone is not known.
pub fn clock(ms: u64) -> String {
    match attention::local_minute(ms) {
        Some(m) => format!("{:02}:{:02}", m / 60, m % 60),
        None => belay_core::fleet::model::clock_utc(ms),
    }
}

/// Why a run is being stopped, and what the user is told.
#[derive(Debug, Clone, PartialEq)]
pub enum Stop {
    Budget(String),
    Deadline(String),
    User(String),
}

impl Stop {
    fn text(&self) -> &str {
        match self {
            Stop::Budget(t) | Stop::Deadline(t) | Stop::User(t) => t,
        }
    }
    fn kind(&self) -> &'static str {
        match self {
            Stop::Budget(_) => "budget",
            Stop::Deadline(_) => "deadline",
            Stop::User(_) => "stopped",
        }
    }
}

/// Reads one run's headless output: tokens, the conversation, the end, a usage limit;
/// and says when to stop. A stop waits for a step boundary: the next tool result (or
/// the run's end), so no tool is cut off midway and the conversation resumes cleanly.
pub struct Tracker<'a> {
    connector: &'a dyn Connector,
    budget: Budget,
    until_ms: Option<u64>,
    calls: std::collections::BTreeSet<String>,
    /// Tokens before this run (earlier runs of the same background session).
    pub base_tokens: u64,
    pub tokens: u64,
    /// The cost Claude Code reported at this run's end.
    pub cost_usd: Option<f64>,
    pub base_cost: f64,
    pub conversation: Option<String>,
    pub result: Option<String>,
    /// A usage limit ended the run: when it resets, if it says.
    pub limit: Option<Option<u64>>,
    /// Asked for (by the budget, the deadline, or the user), and not yet at a boundary.
    pending: Option<Stop>,
    /// Given to the caller: the run is being stopped.
    pub stopped: Option<Stop>,
}

impl<'a> Tracker<'a> {
    pub fn new(connector: &'a dyn Connector, meta: &Meta, state: &State) -> Self {
        Tracker {
            connector,
            budget: meta.budget.clone(),
            until_ms: meta.until_ms,
            calls: Default::default(),
            base_tokens: state.tokens,
            tokens: 0,
            cost_usd: None,
            base_cost: state.cost_usd,
            conversation: None,
            result: None,
            limit: None,
            pending: None,
            stopped: None,
        }
    }

    pub fn total_tokens(&self) -> u64 {
        self.base_tokens + self.tokens
    }

    pub fn total_cost(&self) -> f64 {
        self.base_cost + self.cost_usd.unwrap_or(0.0)
    }

    /// Ask for a stop at the next boundary (the first reason wins).
    pub fn ask(&mut self, why: Stop) {
        if self.pending.is_none() && self.stopped.is_none() {
            self.pending = Some(why);
        }
    }

    /// Why the run that just ended was stopped: at a boundary, or asked for and ended
    /// before one came (a stop now, a deadline long past with no output).
    pub fn finish(&mut self, now: u64) -> Option<Stop> {
        if let Some(u) = self.until_ms.filter(|u| now >= *u) {
            self.ask(Stop::Deadline(format!(
                "it reached the deadline, {}",
                clock(u)
            )));
        }
        if self.stopped.is_none() && self.limit.is_none() {
            self.stopped = self.pending.take();
        }
        self.stopped.clone()
    }

    /// One stdout line at `now`. Some: stop the run now.
    pub fn line(&mut self, line: &str, now: u64) -> Option<Stop> {
        let v: Option<serde_json::Value> = serde_json::from_str(line).ok();
        let kind = v
            .as_ref()
            .and_then(|v| v.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if let Some(c) = v
            .as_ref()
            .and_then(|v| v.get("session_id"))
            .and_then(|s| s.as_str())
        {
            self.conversation = Some(c.to_string());
        }
        if let Some(l) = self.connector.headless_line(line) {
            if l.call.is_none_or(|c| self.calls.insert(c)) {
                self.tokens += l.tokens;
            }
            if l.result.is_some() {
                self.result = l.result;
            }
        }
        if kind == "result" {
            if let Some(c) = v
                .as_ref()
                .and_then(|v| v.get("total_cost_usd"))
                .and_then(|c| c.as_f64())
            {
                self.cost_usd = Some(c);
            }
            if let Some(belay_core::fleet::model::FleetEvent::UsageLimit { resets_at_ms, .. }) =
                hub::limit_of(line, "bg", now)
            {
                self.limit = Some(resets_at_ms);
            }
        }
        if let Some(b) = self.budget.tokens.filter(|b| self.total_tokens() > *b) {
            self.ask(Stop::Budget(format!(
                "it used {} tokens, past the budget of {}",
                self.total_tokens(),
                sandbox::limits::fmt_count(b)
            )));
        }
        if let Some(b) = self.budget.usd.filter(|b| self.total_cost() > *b) {
            self.ask(Stop::Budget(format!(
                "it cost ${:.2}, past the budget of ${b:.2}",
                self.total_cost()
            )));
        }
        if let Some(u) = self.until_ms.filter(|u| now >= *u) {
            self.ask(Stop::Deadline(format!(
                "it reached the deadline, {}",
                clock(u)
            )));
        }
        // A tool's result is in: the step is over. The end needs no stop.
        if kind == "user" && self.stopped.is_none() {
            if let Some(s) = self.pending.take() {
                self.stopped = Some(s.clone());
                return Some(s);
            }
        }
        if kind == "result" {
            if let Some(s) = self.pending.take() {
                self.stopped = Some(s);
            }
        }
        None
    }
}

/// What the user must look at when the agent says it is done, from its brief: an edit
/// the guard blocked and nobody allowed, then a check that is failing or not run since
/// the last edit. None: done.
pub fn needs_you(b: &belay_core::brief::Brief) -> Option<String> {
    use belay_core::brief::CheckStatus;
    if let Some(l) = b
        .look_at
        .iter()
        .find(|l| l.kind == "guard" && l.text.contains("not allowed"))
    {
        return Some(format!("{} [{}]", l.text, l.evidence));
    }
    b.checked
        .iter()
        .find(|c| {
            matches!(
                c.status,
                CheckStatus::Failing | CheckStatus::NotRunSinceLastEdit
            )
        })
        .map(|c| format!("`{}` {}", c.command, c.status.text()))
}

/// The phase in words, as `status` and `list` show it.
pub fn phase_line(p: &Phase) -> String {
    match p {
        Phase::Starting => "starting".into(),
        Phase::Working => "working".into(),
        Phase::Waiting { until_ms } => format!(
            "waiting for the usage limit to reset, until {}",
            clock(*until_ms)
        ),
        Phase::Done => "done".into(),
        Phase::NeedsYou { reason } => format!("needs you: {reason}"),
        Phase::Stopped { reason } => format!("stopped: {reason}"),
    }
}

/// The prompt of a run that goes on after a usage limit.
const CONTINUE: &str = "belay: the usage limit has reset. Continue the task where you stopped. When it is done, say what you did.";

/// The agent's arguments for one run. In the checkout, the permission mode is `auto`,
/// not the sandbox's `bypassPermissions`: there, nothing contains the agent but its own
/// permissions and belay's guard. [Verify] `claude -p --permission-mode auto` runs
/// headless (Claude Code 2.1.316) without prompts it cannot answer.
pub fn argv(
    connector: &dyn Connector,
    place: &Place,
    prompt: &str,
    resume: Option<&str>,
    model: Option<&str>,
) -> Result<Vec<String>> {
    let mut a = connector.headless_argv(prompt, resume, model)?;
    if *place == Place::Here {
        if let Some(i) = a.iter().position(|x| x == "--permission-mode") {
            if let Some(m) = a.get_mut(i + 1) {
                *m = "auto".into();
            }
        }
    }
    Ok(a)
}

// ---- Files.

fn root() -> PathBuf {
    paths::home().join("bg")
}

fn dir_of(id: &str) -> PathBuf {
    root().join(id)
}

fn read_meta(id: &str) -> Result<Meta> {
    let p = dir_of(id).join("meta.json");
    let t = fs::read_to_string(&p).with_context(|| format!("no background session `{id}`"))?;
    serde_json::from_str(&t).with_context(|| format!("read {}", p.display()))
}

fn read_state(id: &str) -> State {
    fs::read_to_string(dir_of(id).join("state.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_state(id: &str, s: &mut State) {
    s.updated_ms = paths::now_ms();
    let dir = dir_of(id);
    let tmp = dir.join("state.json.t");
    if let Ok(t) = serde_json::to_string_pretty(s) {
        if fs::write(&tmp, t).is_ok() {
            let _ = fs::rename(&tmp, dir.join("state.json"));
        }
    }
}

/// What the supervisor did, with its reason (rule 4).
fn record(id: &str, what: &str, reason: &str) {
    let reason = belay_core::text::redact(reason);
    let line = serde_json::json!({"ts_ms": paths::now_ms(), "what": what, "reason": reason});
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir_of(id).join("log.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}

/// A stop the user asked for: `now` (stop at once), else at the next step.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StopAsk {
    now: bool,
    why: String,
}

fn stop_asked(id: &str) -> Option<StopAsk> {
    let t = fs::read_to_string(dir_of(id).join("stop")).ok()?;
    serde_json::from_str(&t).ok()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// The ids of the background sessions, newest first, and the one `s` names: an id, a
/// unique prefix, or `-N` (the Nth newest).
fn all_ids() -> Vec<String> {
    let mut v: Vec<(u64, String)> = fs::read_dir(root())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let id = e.file_name().to_string_lossy().to_string();
            read_meta(&id).ok().map(|m| (m.created_ms, id))
        })
        .collect();
    v.sort_by(|a, b| b.cmp(a));
    v.into_iter().map(|(_, id)| id).collect()
}

fn resolve(s: &str) -> Result<String> {
    let ids = all_ids();
    if let Some(n) = s.strip_prefix('-').and_then(|n| n.parse::<usize>().ok()) {
        return ids
            .get(n.saturating_sub(1))
            .cloned()
            .ok_or_else(|| anyhow!("no background session {s}"));
    }
    let hits: Vec<&String> = ids.iter().filter(|i| i.starts_with(s)).collect();
    match hits.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("no background session `{s}`; `belay bg list` shows them"),
        _ => bail!("`{s}` names more than one background session"),
    }
}

// ---- Commands.

pub fn cli(cmd: BgCmd) -> Result<i32> {
    match cmd {
        BgCmd::Start {
            prompt,
            resume,
            sandbox,
            here,
            budget,
            until,
            no_keep_awake,
            model,
        } => start(Start {
            prompt,
            resume,
            sandbox,
            here,
            budget,
            until,
            keep_awake: !no_keep_awake,
            model,
        }),
        BgCmd::List => {
            print!("{}", list());
            Ok(0)
        }
        BgCmd::Status { id } => {
            print!("{}", status(&resolve(&id)?)?);
            Ok(0)
        }
        BgCmd::Watch { id, verbose } => {
            let id = resolve(&id)?;
            let Some(session) = read_state(&id).session else {
                bail!("background session {id} has no belay session yet; try again in a moment");
            };
            watch::run(Some(&session), verbose)?;
            Ok(0)
        }
        BgCmd::Stop { id, now } => {
            paths::users_only("`belay bg stop`")?;
            let id = resolve(&id)?;
            match ask_stop(&id, now, "the user stopped it")? {
                false => println!("Background session {id} is not running."),
                true if now => println!("Stopping background session {id} now."),
                true => println!("Background session {id} stops at the next step."),
            }
            Ok(0)
        }
        BgCmd::Attach { id, now } => attach(&id, now),
        BgCmd::Run { id } => {
            run(&id)?;
            Ok(0)
        }
    }
}

struct Start {
    prompt: String,
    resume: Option<String>,
    sandbox: bool,
    here: bool,
    budget: Option<String>,
    until: Option<String>,
    keep_awake: bool,
    model: Option<String>,
}

fn new_id(seed: &str) -> String {
    let s = format!("{seed}:{}:{}", paths::now_ms(), std::process::id());
    format!("bg-{}", &belay_core::text::sha256_hex(&s)[..6])
}

/// `belay bg start`. An agent (not the user's terminal) gets a sandbox and a budget
/// always, within the sandbox task limits.
fn start(a: Start) -> Result<i32> {
    if std::env::var_os("BELAY_SANDBOX").is_some() {
        bail!("belay: refused: this is a sandbox; start background sessions from outside it");
    }
    if a.prompt.trim().is_empty() {
        bail!("give the background session a prompt");
    }
    let cfg = config::load();
    let by_agent = !paths::users_terminal();
    let now = paths::now_ms();
    let mut budget = a
        .budget
        .as_deref()
        .map(parse_budget)
        .transpose()?
        .unwrap_or_default();
    let until_ms = a
        .until
        .as_deref()
        .map(|u| parse_until(u, now, attention::local_minute(now)))
        .transpose()?;
    let cwd = std::env::current_dir().context("current directory")?;
    let in_repo = crate::audit::repo_root(&cwd).is_ok();
    let id = new_id(&cwd.to_string_lossy());
    let place = if by_agent {
        // Contained and budgeted: an agent's background session never runs in the
        // checkout, and never past the sandbox task budget.
        if a.here {
            bail!("belay: refused: an agent's background session runs in a sandbox; only the user runs one here");
        }
        sandbox::backend()?;
        if !in_repo {
            bail!(
                "belay: refused: a sandbox needs a git repo, and {} is not in one",
                cwd.display()
            );
        }
        let cap = cfg.sandbox_task_tokens;
        budget.tokens = Some(budget.tokens.map_or(cap, |t| t.min(cap)));
        Place::Sandbox { id: id.clone() }
    } else if a.here {
        Place::Here
    } else if a.sandbox {
        sandbox::backend()?;
        if !in_repo {
            bail!(
                "belay: refused: a sandbox needs a git repo, and {} is not in one",
                cwd.display()
            );
        }
        Place::Sandbox { id: id.clone() }
    } else {
        let why_not = match sandbox::backend() {
            Err(e) => Some(format!("{e:#}")),
            Ok(_) if !in_repo => Some("this is not a git repo".into()),
            Ok(_) => None,
        };
        match why_not {
            None => Place::Sandbox { id: id.clone() },
            Some(why) => {
                let why = why.trim_start_matches("belay: refused: ");
                eprintln!(
                    "belay bg: no sandbox ({why}); it runs in this checkout, with the guard on."
                );
                Place::Here
            }
        }
    };
    let by = if let Place::Sandbox { .. } = place {
        let by = sandbox::session::admit(&cfg, by_agent)?;
        sandbox::session::record_start(&by, &id)?;
        by
    } else {
        "user".into()
    };
    let meta = Meta {
        id: id.clone(),
        prompt: a.prompt,
        cwd: cwd.clone(),
        place,
        resume: a.resume,
        model: a.model,
        budget,
        until_ms,
        keep_awake: a.keep_awake,
        by,
        created_ms: now,
    };
    let dir = dir_of(&id);
    paths::ensure_private_dir(&root())?;
    paths::ensure_private_dir(&dir)?;
    fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
    write_state(&id, &mut State::default());
    if meta.keep_awake {
        let a = keepawake::assess();
        eprintln!("belay bg: {}", lid_line(&a));
    }
    let args: Vec<&std::ffi::OsStr> = vec!["bg".as_ref(), "run".as_ref(), id.as_ref()];
    // A session's own variables would make the supervisor's agent a part of it.
    let unset = [
        "CLAUDECODE",
        "BELAY_SESSION_DIR",
        "BELAY_SOCKET",
        "BELAY_FLEET",
        "BELAY_FLEET_SLOT",
        "BELAY_FLEET_RULES",
    ];
    hub::supervise::detached(&args, &dir.join("runner.log"), &unset)
        .context("start the background supervisor")?;
    let place = match &meta.place {
        Place::Here => format!("in {}", cwd.display()),
        Place::Sandbox { id } => format!("in sandbox {id} (branch belay/sandbox/{id})"),
    };
    println!("belay bg {id}: started {place}.");
    let mut limits = Vec::new();
    if let Some(t) = meta.budget.tokens {
        limits.push(format!("budget {} tokens", sandbox::limits::fmt_count(t)));
    }
    if let Some(d) = meta.budget.usd {
        limits.push(format!("budget ${d:.2}"));
    }
    if let Some(u) = meta.until_ms {
        limits.push(format!("until {}", clock(u)));
    }
    if !limits.is_empty() {
        println!("It stops at the next step past its {}.", limits.join(", "));
    }
    println!("`belay bg status {id}` · `belay bg watch {id}` · `belay bg attach {id}`");
    Ok(0)
}

/// The closed-lid line of the keep-awake assessment: held, or what happens and the fix.
pub fn lid_line(a: &keepawake::Assessment) -> String {
    match a.lid_closed {
        keepawake::Guarantee::Held => format!("lid closed: held ({}).", a.mechanism),
        _ => {
            let mut s = format!("lid closed: not held. {}", a.note);
            if let Some(f) = &a.fix {
                s.push_str(&format!(" To change: {f}"));
            }
            s
        }
    }
}

/// Ask the supervisor to stop. False: it is not running.
fn ask_stop(id: &str, now: bool, why: &str) -> Result<bool> {
    let st = read_state(id);
    if !st.runner.is_some_and(alive) || st.phase.ended() {
        return Ok(false);
    }
    let ask = StopAsk {
        now,
        why: why.into(),
    };
    fs::write(dir_of(id).join("stop"), serde_json::to_string(&ask)?)?;
    Ok(true)
}

fn list() -> String {
    let ids = all_ids();
    if ids.is_empty() {
        return "No background sessions. Start one with `belay bg start \"<prompt>\"`.\n".into();
    }
    let mut s = String::new();
    for id in ids {
        let Ok(m) = read_meta(&id) else { continue };
        let st = shown_state(&id);
        let prompt = belay_core::text::truncate_chars(m.prompt.lines().next().unwrap_or(""), 50);
        s.push_str(&format!(
            "{id}  {}  {} tokens  {}\n",
            phase_line(&st.phase),
            crate::report::k(st.tokens),
            prompt
        ));
    }
    s
}

/// The state, with a supervisor that is gone without saying so shown as stopped.
fn shown_state(id: &str) -> State {
    let mut st = read_state(id);
    let gone = st.runner.is_none_or(|p| !alive(p));
    if !st.phase.ended() && gone && st.updated_ms + 10_000 < paths::now_ms() {
        st.phase = Phase::Stopped {
            reason: "its supervisor is gone (the machine restarted, or it was killed)".into(),
        };
    }
    st
}

fn status(id: &str) -> Result<String> {
    let m = read_meta(id)?;
    let st = shown_state(id);
    let mut s = format!("{id} · {}\n", phase_line(&st.phase));
    let cost = if st.cost_usd > 0.0 {
        format!(" · ${:.2}", st.cost_usd)
    } else {
        String::new()
    };
    s.push_str(&format!(
        "  used: {} tokens{cost} in {} run{}\n",
        crate::report::k(st.tokens),
        st.runs,
        if st.runs == 1 { "" } else { "s" }
    ));
    s.push_str(&format!(
        "  prompt: {}\n",
        belay_core::text::truncate_chars(m.prompt.trim(), 200)
    ));
    match &m.place {
        Place::Here => s.push_str(&format!("  where: {} (the guard is on)\n", m.cwd.display())),
        Place::Sandbox { id: sb } => s.push_str(&format!(
            "  where: sandbox {sb}, branch belay/sandbox/{sb}\n"
        )),
    }
    if let Some(c) = &st.conversation {
        s.push_str(&format!("  conversation: {c}\n"));
    }
    if let Some(sess) = &st.session {
        s.push_str(&format!("  belay session: {sess}\n"));
        if let Some(n) = last_notice(&paths::sessions_dir().join(sess)) {
            s.push_str(&format!("  last notice: {n}\n"));
        }
    }
    if let Some(r) = st.result.as_deref().filter(|r| !r.trim().is_empty()) {
        s.push_str(&format!(
            "  last message: {}\n",
            belay_core::text::truncate_chars(r.trim(), 300)
        ));
    }
    let next = if st.phase.ended() {
        format!("`belay bg attach {id}` opens its conversation here")
    } else {
        format!("`belay bg watch {id}` · `belay bg attach {id}` · `belay bg stop {id}`")
    };
    s.push_str(&format!("  {next}\n"));
    Ok(s)
}

/// The newest notice the session showed the user, as `belay watch` prints it.
fn last_notice(dir: &Path) -> Option<String> {
    let entries = store::read_entries(dir).ok()?;
    entries.iter().rev().find_map(|e| watch::line(e, true))
}

/// `belay bg attach`: stop the run (at the next step, or now), then open its
/// conversation through the launcher, so belay goes on tracking it (U8).
fn attach(id: &str, now: bool) -> Result<i32> {
    paths::users_only("`belay bg attach`")?;
    let id = resolve(id)?;
    let m = read_meta(&id)?;
    if ask_stop(&id, now, "the user attached")? {
        eprintln!("belay bg: stopping {id} at the next step…");
        while read_state(&id).runner.is_some_and(alive) {
            thread::sleep(Duration::from_millis(200));
        }
    }
    let st = read_state(&id);
    let Some(conv) = st.conversation else {
        bail!("background session {id} has no conversation to open");
    };
    let exe = std::env::current_exe().context("find belay executable")?;
    let mut cmd = Command::new(exe);
    match &m.place {
        Place::Here => cmd.args(["claude", "--resume", &conv]),
        Place::Sandbox { id: sb } => cmd.args(["sandbox", "claude", "--id", sb, "--resume", &conv]),
    };
    eprintln!("belay bg: opening conversation {conv}.");
    let err = cmd.current_dir(&m.cwd).exec();
    Err(err).context("start belay claude")
}

// ---- The supervisor.

/// Tell the user (V3's channels: desktop, `notify_command`; never the agent).
fn notify(cfg: &config::Config, m: &Meta, st: &State, kind: &str, text: &str) {
    let now = paths::now_ms();
    let mut gate = attention::Attention::new(cfg.notify, cfg.notify_quiet);
    let k = match kind {
        "done" => attention::Kind::Done,
        "needs_you" => attention::Kind::VerifyUnmet,
        _ => attention::Kind::Waiting,
    };
    if !gate.admit(k, now, attention::local_minute(now)) {
        return;
    }
    attention::desktop(&format!(
        "belay bg {}: {}",
        m.id,
        text.lines().next().unwrap_or("")
    ));
    if let Some(cmd) = &cfg.notify_command {
        let json = serde_json::json!({
            "session": st.session.clone().unwrap_or_default(),
            "repo": m.cwd.display().to_string(),
            "kind": kind,
            "text": text,
            "action": null,
            "bg": m.id,
            "ts_ms": now,
        });
        attention::run_command(cmd, &json, attention::COMMAND_TIMEOUT);
    }
}

/// How one run ended.
struct Ended {
    code: i32,
    stop: Option<Stop>,
    limit: Option<Option<u64>>,
}

/// `belay bg run`: the detached supervisor.
fn run(id: &str) -> Result<()> {
    let m = read_meta(id)?;
    let cfg = config::load();
    let connector = connector::for_agent(connector::DEFAULT_AGENT)?;
    let mut st = read_state(id);
    st.runner = Some(std::process::id() as i32);
    st.phase = Phase::Working;
    write_state(id, &mut st);
    record(id, "started", &format!("started by {}", m.by));
    let _awake = if m.keep_awake {
        match keepawake::hold(&format!("belay bg {id}"), std::process::id()) {
            Ok(g) => {
                record(
                    id,
                    "keep_awake",
                    &format!("holding sleep off with {}", g.mechanism),
                );
                Some(g)
            }
            Err(e) => {
                record(id, "keep_awake", &format!("cannot hold sleep off: {e:#}"));
                None
            }
        }
    } else {
        None
    };
    std::env::set_current_dir(&m.cwd).with_context(|| format!("cd {}", m.cwd.display()))?;
    let mut prompt = m.prompt.clone();
    // The first run forks the user's conversation; later runs fork the last run's.
    let mut resume = m.resume.clone();
    let mut copy = m.resume.clone();
    loop {
        let args = argv(
            &*connector,
            &m.place,
            &prompt,
            resume.as_deref(),
            m.model.as_deref(),
        )?;
        st.runs += 1;
        st.phase = Phase::Working;
        write_state(id, &mut st);
        let ended = match &m.place {
            Place::Here => run_here(id, &m, &mut st, &*connector, &args),
            Place::Sandbox { id: sb } => run_sandbox(
                id,
                &m,
                &mut st,
                &*connector,
                sb,
                args,
                copy.take().as_deref(),
            ),
        };
        let ended = match ended {
            Ok(e) => e,
            Err(e) => {
                let reason = format!("the run could not start: {e:#}");
                record(id, "needs_you", &reason);
                st.phase = Phase::NeedsYou {
                    reason: reason.clone(),
                };
                break notify(&cfg, &m, &st, "needs_you", &reason);
            }
        };
        if let Some(s) = ended.stop {
            let reason = s.text().to_string();
            record(id, s.kind(), &reason);
            st.phase = Phase::Stopped {
                reason: reason.clone(),
            };
            if !matches!(s, Stop::User(_)) {
                let text =
                    format!("Stopped: {reason}. `belay bg attach {id}` picks it up where it is.");
                write_state(id, &mut st);
                notify(&cfg, &m, &st, s.kind(), &text);
            }
            break;
        }
        if let Some(resets) = ended.limit {
            let Some(at) = resets.map(|r| r + belay_core::fleet::model::RESUME_MARGIN_MS) else {
                let reason = "the usage limit was hit, and its message gives no reset time; `belay bg attach` when it resets".to_string();
                record(id, "needs_you", &reason);
                st.phase = Phase::NeedsYou {
                    reason: reason.clone(),
                };
                break notify(&cfg, &m, &st, "usage_limit", &reason);
            };
            if m.until_ms.is_some_and(|u| at >= u) {
                let reason = format!(
                    "the usage limit resets after its deadline ({})",
                    clock(m.until_ms.unwrap_or(at))
                );
                record(id, "deadline", &reason);
                st.phase = Phase::Stopped {
                    reason: reason.clone(),
                };
                break notify(&cfg, &m, &st, "deadline", &format!("Stopped: {reason}."));
            }
            let why = format!(
                "the usage limit was hit; it goes on by itself at {} ({} after the reset)",
                clock(at),
                "2 min"
            );
            record(id, "waiting", &why);
            st.phase = Phase::Waiting { until_ms: at };
            write_state(id, &mut st);
            notify(
                &cfg,
                &m,
                &st,
                "usage_limit",
                &format!("Usage limit: {why}."),
            );
            if let Some(s) = wait_until(id, at) {
                record(id, "stopped", &s);
                st.phase = Phase::Stopped { reason: s };
                break;
            }
            record(id, "resumed", "the usage limit has reset");
            prompt = CONTINUE.into();
            resume = st.conversation.clone();
            continue;
        }
        let brief = st
            .session
            .as_ref()
            .and_then(|s| brief::of_session(&paths::sessions_dir().join(s)).ok());
        let head = brief
            .as_ref()
            .map(|b| b.headline().join("\n"))
            .unwrap_or_default();
        let need = brief.as_ref().and_then(needs_you);
        if ended.code != 0 {
            let reason = format!("the agent exited with code {}", ended.code);
            record(id, "needs_you", &reason);
            st.phase = Phase::NeedsYou {
                reason: reason.clone(),
            };
            break notify(
                &cfg,
                &m,
                &st,
                "needs_you",
                &format!("Needs you: {reason}.\n{head}"),
            );
        }
        if let Some(reason) = need {
            record(id, "needs_you", &reason);
            st.phase = Phase::NeedsYou {
                reason: reason.clone(),
            };
            break notify(
                &cfg,
                &m,
                &st,
                "needs_you",
                &format!("Needs you: {reason}.\n{head}"),
            );
        }
        record(id, "done", "the agent finished its run");
        st.phase = Phase::Done;
        write_state(id, &mut st);
        break notify(&cfg, &m, &st, "done", &format!("Done.\n{head}"));
    }
    st.runner = None;
    write_state(id, &mut st);
    let _ = fs::remove_file(dir_of(id).join("stop"));
    Ok(())
}

/// Sleep until `at`, unless the user stops it first (the reason) or the test that
/// started it is gone.
fn wait_until(id: &str, at: u64) -> Option<String> {
    while paths::now_ms() < at {
        if let Some(s) = stop_asked(id) {
            return Some(s.why);
        }
        if hub::test_parent_gone() {
            return Some("its test is gone".into());
        }
        thread::sleep(Duration::from_secs(1));
    }
    None
}

/// Append a line of the agent's output to `out.jsonl`.
fn keep_line(out: &mut Option<fs::File>, line: &str) {
    if let Some(f) = out {
        let _ = writeln!(f, "{line}");
    }
}

/// Fold what a run's tracker saw into the state.
fn absorb(st: &mut State, t: &Tracker) {
    st.tokens = t.total_tokens();
    st.cost_usd = t.total_cost();
    if t.conversation.is_some() {
        st.conversation = t.conversation.clone();
    }
    if t.result.is_some() {
        st.result = t
            .result
            .as_deref()
            .map(|r| belay_core::text::redact(r).into_owned());
    }
}

/// The belay session `belay claude` (pid `launcher`) made: its dir name ends in its pid.
fn session_of(launcher: i32) -> Option<String> {
    let suffix = format!("-{launcher}");
    fs::read_dir(paths::sessions_dir())
        .ok()?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .find(|n| n.ends_with(&suffix))
}

const TERM_GRACE: Duration = Duration::from_secs(10);

/// One run in the checkout: `belay claude <args>`, so the guard and the ledger are on.
fn run_here(
    id: &str,
    m: &Meta,
    st: &mut State,
    connector: &dyn Connector,
    args: &[String],
) -> Result<Ended> {
    let exe = std::env::current_exe().context("find belay executable")?;
    let err = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir_of(id).join("runner.log"))?;
    let mut child = Command::new(exe)
        .arg("claude")
        .args(args)
        .current_dir(&m.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(err)
        .process_group(0)
        .spawn()
        .context("start belay claude")?;
    let pid = child.id() as i32;
    let finished = Arc::new(AtomicBool::new(false));
    // Signals the run: SIGTERM to the launcher (it passes it to the agent), and the
    // whole group after a grace.
    let term = {
        let finished = Arc::clone(&finished);
        move || {
            // SAFETY: kill has no memory effects; `finished` guards against a reused pid.
            unsafe { libc::kill(pid, libc::SIGTERM) };
            let finished = Arc::clone(&finished);
            thread::spawn(move || {
                thread::sleep(TERM_GRACE);
                if !finished.load(Ordering::SeqCst) {
                    // SAFETY: as above, for the group the launcher leads.
                    unsafe { libc::kill(-pid, libc::SIGKILL) };
                }
            });
        }
    };
    // A stop the user asks for now, a test that is gone, or a deadline with no output
    // to read: these do not wait for a line.
    let watcher = {
        let (finished, term, id, until) = (
            Arc::clone(&finished),
            term.clone(),
            id.to_string(),
            m.until_ms,
        );
        thread::spawn(move || {
            let mut sent = false;
            while !finished.load(Ordering::SeqCst) {
                let now_stop = stop_asked(&id).is_some_and(|s| s.now);
                let late =
                    until.is_some_and(|u| paths::now_ms() >= u + TERM_GRACE.as_millis() as u64 * 6);
                if !sent && (now_stop || hub::test_parent_gone() || late) {
                    sent = true;
                    term();
                }
                thread::sleep(Duration::from_millis(200));
            }
        })
    };
    let mut t = Tracker::new(connector, m, st);
    let mut out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir_of(id).join("out.jsonl"))
        .ok();
    let mut last_write = 0u64;
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            keep_line(&mut out, &line);
            let now = paths::now_ms();
            if let Some(s) = stop_asked(id) {
                t.ask(Stop::User(s.why));
            }
            if t.line(&line, now).is_some() {
                term();
            }
            if st.session.is_none() {
                st.session = session_of(pid);
            }
            absorb(st, &t);
            if now.saturating_sub(last_write) > 2_000 {
                last_write = now;
                write_state(id, st);
            }
        }
    }
    let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(1);
    finished.store(true, Ordering::SeqCst);
    let _ = watcher.join();
    if st.session.is_none() {
        st.session = session_of(pid);
    }
    absorb(st, &t);
    // A stop the user asked for at once ends the run with no line to stop it on.
    if let Some(s) = stop_asked(id) {
        t.ask(Stop::User(s.why));
    }
    let stop = t.finish(paths::now_ms());
    write_state(id, st);
    Ok(Ended {
        code,
        stop,
        limit: t.limit,
    })
}

/// One run in sandbox `sb`; the work lands on its branch.
fn run_sandbox(
    id: &str,
    m: &Meta,
    st: &mut State,
    connector: &dyn Connector,
    sb: &str,
    args: Vec<String>,
    copy: Option<&str>,
) -> Result<Ended> {
    let mut t = Tracker::new(connector, m, st);
    let mut out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir_of(id).join("out.jsonl"))
        .ok();
    let finished = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (finished, id, sb) = (Arc::clone(&finished), id.to_string(), sb.to_string());
        thread::spawn(move || {
            let mut sent = false;
            while !finished.load(Ordering::SeqCst) {
                if !sent && (stop_asked(&id).is_some_and(|s| s.now) || hub::test_parent_gone()) {
                    sent = true;
                    let _ = sandbox::cli(sandbox::SandboxCmd::Stop { id: sb.clone() });
                }
                thread::sleep(Duration::from_millis(200));
            }
        })
    };
    let message = belay_core::text::truncate_chars(&m.prompt, 200);
    let session = std::cell::RefCell::new(None::<String>);
    let mut started = |s: &str| {
        *session.borrow_mut() = Some(s.to_string());
    };
    let mut last_write = 0u64;
    let shared = std::cell::RefCell::new(st.clone());
    let mut each = |line: &str| -> Option<String> {
        keep_line(&mut out, line);
        let now = paths::now_ms();
        if let Some(s) = stop_asked(id) {
            t.ask(Stop::User(s.why));
        }
        let stop = t.line(line, now).map(|s| s.text().to_string());
        let mut st = shared.borrow_mut();
        st.session = session.borrow().clone();
        absorb(&mut st, &t);
        if now.saturating_sub(last_write) > 2_000 {
            last_write = now;
            write_state(id, &mut st);
        }
        stop
    };
    let ran = sandbox::session::headless(sb, args, copy, &message, &mut started, &mut each);
    finished.store(true, Ordering::SeqCst);
    let _ = watcher.join();
    *st = shared.into_inner();
    let ran = ran?;
    st.session = Some(ran.session);
    absorb(st, &t);
    match &ran.landed {
        Ok(f) => record(
            id,
            "landed",
            &format!("{} file(s) on belay/sandbox/{sb}", f.len()),
        ),
        Err(e) => record(id, "landed", &format!("the work could not land: {e:#}")),
    }
    if let Some(s) = stop_asked(id) {
        t.ask(Stop::User(s.why));
    }
    // A sandbox limit (memory, wall time) stopped it: the user's to look at.
    let stop = t
        .finish(paths::now_ms())
        .or_else(|| ran.outcome.stopped.clone().map(Stop::User));
    write_state(id, st);
    Ok(Ended {
        code: ran.outcome.code,
        stop,
        limit: t.limit,
    })
}
