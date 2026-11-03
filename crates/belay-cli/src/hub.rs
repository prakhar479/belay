//! The fleet hub (F3a): one process per fleet. It owns the fleet model (belay-core
//! `fleet::model`), records every event and command in the fleet's ledger, and runs the
//! commands: it starts sessions under `belay claude` in stream-json mode, holds their
//! stdin, watches their output and exits, and makes and keeps their worktrees.
//!
//! Layout: <home>/fleets/<id>/
//!   meta.json         the resolved fleet file, repo, base commit
//!   ledger.jsonl      fleet records (events and commands)
//!   inbox.jsonl       what the fleet tells the user
//!   slots/<slot>.jsonl, .err   each slot's agent output
//!   worktrees/<slot>  each slot's worktree (branch belay/<id>/<slot>)
//!   hub.log, belayd.sock

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use belay_core::fleet::model::{
    Actor, Address, Command as Cmd, FleetBody, FleetEvent, FleetRecord, Model, SlotState, Work,
};
use belay_core::fleet::spec::{self, Preset, Run, Spec, SpecFile, Workspace};
use serde::{Deserialize, Serialize};

use crate::ipc::Reply;
use crate::paths::{self, now_ms, socket_path};

mod client;
mod merge;
mod prune;
mod slots;
pub(crate) mod supervise;
mod work;

pub use self::client::*;
pub use self::merge::*;
pub use self::prune::*;
pub(crate) use self::slots::limit_of;
use self::slots::*;

/// The hub sends the model a Tick this often.
const TICK: Duration = Duration::from_secs(2);
/// Activity events per slot, at most one this often (turn ends always go).
const ACTIVITY_EVERY_MS: u64 = 1_000;
/// After `stop`: the session's stdin is closed; then SIGTERM, then SIGKILL.
const TERM_AFTER: Duration = Duration::from_secs(5);
const KILL_AFTER: Duration = Duration::from_secs(10);
/// Memory one agent session takes (F-S2: ~300 MiB), with room.
const AGENT_BYTES: u64 = 350 << 20;

pub fn fleets_dir() -> PathBuf {
    paths::home().join("fleets")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    /// The worktree `up` ran in; worktrees branch from `base` there. None outside git.
    pub repo: Option<String>,
    pub base: Option<String>,
    pub cwd: String,
    /// The 0.1 mode of the fleet's sessions, read from old fleets; never written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    pub created_ms: u64,
    pub spec: Spec,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubRequest {
    Ping,
    /// Wake an idle stream slot for a commons message (C2); the model rate-limits it.
    Wake {
        slot: String,
        from: String,
    },
    Status,
    Stop {
        slot: String,
    },
    Down,
    Spawn {
        role: String,
    },
    Scale {
        role: String,
        n: u32,
    },
    Pause,
    Resume,
    /// A session in the user's terminal joins (F4): `pid` is its launcher's, so its
    /// tools' requests are told apart like a spawned session's.
    Join {
        role: Option<String>,
        name: Option<String>,
        pid: i32,
        cwd: String,
    },
    Leave {
        slot: String,
    },
    /// `belay msg`: `to` is `user`, `@role`, or a slot; no `kind` means the first the
    /// channel allows.
    Msg {
        to: String,
        kind: Option<String>,
        text: String,
        #[serde(default)]
        claimed_slot: Option<String>,
    },
    /// `belay inbox`: the caller's messages (a session's), or the fleet's (the user's).
    Inbox {
        #[serde(default)]
        claimed_slot: Option<String>,
    },
    /// A `belay task` verb (F5a). `claimed_slot` is `BELAY_FLEET_SLOT`; it counts only
    /// where the hub cannot see the caller's process tree.
    Task {
        verb: TaskVerb,
        #[serde(default)]
        claimed_slot: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum TaskVerb {
    Add {
        title: String,
        body: String,
        depends_on: Vec<String>,
        scope: Vec<String>,
        role: Option<String>,
        #[serde(default)]
        check: Option<String>,
    },
    Assign {
        task: String,
        to: String,
    },
    Claim {
        task: Option<String>,
    },
    Done {
        task: String,
        summary: String,
    },
    Blocked {
        task: String,
        reason: String,
    },
    Reject {
        task: String,
        reason: String,
    },
    Drop {
        task: String,
        reason: String,
    },
    Show {
        task: String,
    },
    List,
}

// --- belay fleet up ---------------------------------------------------------------

/// Read a fleet file (TOML), or take a preset alone, and resolve it.
pub fn load_spec(file: Option<&Path>, preset: Option<Preset>) -> Result<(Spec, SpecFile)> {
    let mut f: SpecFile = match file {
        Some(p) => {
            let text = fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
            toml::from_str(&text).map_err(|e| anyhow!("{}: {}", p.display(), e.message()))?
        }
        None => SpecFile::default(),
    };
    if preset.is_some() {
        f.preset = preset;
    }
    if f.preset.is_none() && f.role.is_empty() {
        bail!("give a fleet file or `--preset`, for example `belay fleet up --preset lead`");
    }
    let spec = spec::resolve(&f).map_err(|errs| {
        anyhow!(
            "the fleet file has {} problem{}:\n  {}",
            errs.len(),
            if errs.len() == 1 { "" } else { "s" },
            errs.join("\n  ")
        )
    })?;
    Ok((spec, f))
}

/// Sessions that fit in free memory (Linux), at least what the fleet starts with.
fn fit_sessions(spec: &Spec) -> Option<u32> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = text
        .lines()
        .find(|l| l.starts_with("MemAvailable:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    let fit = (kb * 1024 / AGENT_BYTES).max(1) as u32;
    let start: u32 = spec
        .roles
        .values()
        .filter(|r| r.run == Run::Stream)
        .map(|r| r.min)
        .sum();
    Some(fit.max(start))
}

/// Start a fleet: write its dir, start its hub, wait for it. Returns the fleet id.
pub fn up(file: Option<&Path>, preset: Option<Preset>) -> Result<String> {
    let (mut spec, f) = load_spec(file, preset)?;
    let mut notes = Vec::new();
    let bare = spec::roles_without_mode(&spec);
    if !bare.is_empty() {
        let n = format!(
            "No permission_mode for {}: their headless sessions cannot answer a permission prompt, so most edits and commands will be refused. Set `permission_mode` for each, or once at the top of the fleet file.",
            bare.join(", ")
        );
        eprintln!("belay: {n}");
        notes.push(n);
    }
    if f.limits.as_ref().and_then(|l| l.sessions).is_none() {
        if let Some(fit) = fit_sessions(&spec) {
            if fit < spec.limits.sessions {
                notes.push(format!(
                    "Free memory fits about {fit} agent sessions, so the fleet runs at most {fit} at once (set limits.sessions to change it)."
                ));
                spec.limits.sessions = fit;
            }
        }
    }
    let cwd = std::env::current_dir().context("current directory")?;
    let (repo, base) = match git_out(&cwd, &["rev-parse", "--show-toplevel"]) {
        Some(top) => (Some(top), git_out(&cwd, &["rev-parse", "HEAD"])),
        None => (None, None),
    };
    if repo.is_none() {
        notes.push("This is not a git repo: every session works in this directory.".into());
    } else if base.is_none() {
        notes.push("The repo has no commit yet: every session works in this checkout.".into());
    }
    let id = paths::new_session_id();
    let dir = fleets_dir().join(&id);
    paths::ensure_private_dir(&fleets_dir())?;
    paths::ensure_private_dir(&dir)?;
    let meta = Meta {
        id: id.clone(),
        repo,
        base,
        cwd: cwd.display().to_string(),
        mode: None,
        created_ms: now_ms(),
        spec,
    };
    fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
    for n in notes {
        inbox(&dir, &n);
    }
    supervise::start_hub(&dir, false)?;
    for _ in 0..200 {
        if send(&dir, &HubRequest::Ping).is_ok() {
            return Ok(id);
        }
        thread::sleep(Duration::from_millis(25));
    }
    bail!(
        "the fleet hub did not start; see {}",
        dir.join("hub.log").display()
    )
}

// --- The hub process ---------------------------------------------------------------

enum Msg {
    Event(FleetEvent),
    /// The slot's turn ended (a `result` line); close a oneshot's stdin.
    TurnEnded(String),
    /// A request, and the pid of the process that sent it (None: not known here).
    Request(HubRequest, UnixStream, Option<i32>),
    /// A task's worktree tree, taken just before its check ran.
    Tree(String, String),
}

struct Proc {
    pid: i32,
    /// Lines for the session's stdin, written by its own thread: a session that stops
    /// reading must never block the hub. Dropping it closes the stdin.
    stdin: Option<Sender<String>>,
    turns: u32,
    exited: Arc<AtomicBool>,
    worktree: PathBuf,
}

struct Hub {
    dir: PathBuf,
    meta: Meta,
    model: Model,
    ledger: File,
    procs: BTreeMap<String, Proc>,
    tx: Sender<Msg>,
    /// Events the hub itself caused while running commands (a spawn's start).
    queue: VecDeque<FleetEvent>,
    /// A monotonic instant: the wall clock may go back (rule: waits never use it).
    down_since: Option<std::time::Instant>,
    /// Each slot's working directory, kept after its session ends (checks run there).
    worktrees: BTreeMap<String, PathBuf>,
    /// The worktree's tree when each task was delivered, for the audit at check time
    /// (only with `experimental_audit = true`): task → (root, tree).
    bases: BTreeMap<String, (PathBuf, String)>,
    /// Checks run one at a time: a team's sessions may share a worktree.
    checking: Arc<std::sync::Mutex<()>>,
    audit: bool,
    /// What claims.json holds now.
    claims_text: Option<String>,
    /// Slots of sessions in the user's terminal (F4): their launcher pid is watched.
    attached: std::collections::BTreeSet<String>,
    /// Each slot's last session id: a stream slot that starts again resumes it (G1).
    last_session: BTreeMap<String, String>,
    /// (slot, task): that task's work is already in the slot's worktree. Per task, not
    /// per slot: a sub-lead's later task may depend on another area (dogfood D3).
    integrated: std::collections::BTreeSet<(String, String)>,
    /// Done parts whose work went up to their parent's worktree.
    lifted: std::collections::BTreeSet<String>,
    /// Each done task's worktree tree, as its check judged it, and the seq of the work
    /// record that keeps it: its dependents get this, not what the worktree holds later.
    done_trees: BTreeMap<String, (String, u64)>,
    /// Trees taken before a check ran, until the check's result is applied.
    pending_trees: BTreeMap<String, String>,
    /// `build_cache`, and what the hub's environment already says (P0.4).
    build: (crate::disk::BuildCache, crate::disk::Env),
    /// The seq of the command being run: the cause of the work records it leads to.
    running: u64,
}

pub fn run_hub(dir: &Path, resume: bool) -> Result<()> {
    let meta: Meta = serde_json::from_str(&fs::read_to_string(dir.join("meta.json"))?)?;
    let sock = socket_path(dir);
    if let Some(parent) = sock.parent() {
        paths::ensure_private_dir(parent)?;
    }
    let tmp = sock.with_file_name(format!("{}.t", std::process::id() % 10_000_000));
    let _ = fs::remove_file(&tmp);
    let listener = UnixListener::bind(&tmp).with_context(|| format!("bind {}", tmp.display()))?;
    fs::rename(&tmp, &sock)?;
    let (tx, rx) = mpsc::channel();
    {
        let tx = tx.clone();
        thread::spawn(move || listen(listener, tx));
    }
    let ledger = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("ledger.jsonl"))?;
    // A hub killed mid-write leaves a torn last line: end it, so the next record is
    // not glued to it and lost.
    end_torn_line(&dir.join("ledger.jsonl"), &ledger);
    let mut hub = Hub {
        dir: dir.to_path_buf(),
        model: Model::new(meta.spec.clone()),
        meta,
        ledger,
        procs: BTreeMap::new(),
        tx,
        queue: VecDeque::new(),
        down_since: None,
        worktrees: BTreeMap::new(),
        bases: BTreeMap::new(),
        checking: Arc::new(std::sync::Mutex::new(())),
        audit: crate::config::load().experimental_audit,
        claims_text: None,
        attached: std::collections::BTreeSet::new(),
        last_session: BTreeMap::new(),
        integrated: std::collections::BTreeSet::new(),
        lifted: std::collections::BTreeSet::new(),
        done_trees: BTreeMap::new(),
        pending_trees: BTreeMap::new(),
        build: (
            crate::config::load().build_cache,
            crate::disk::Env::current(),
        ),
        running: 0,
    };
    let _ = fs::write(dir.join("hub.pid"), std::process::id().to_string());
    if resume {
        // G1: the state the ledger describes, then the restart. The replayed records are
        // in the ledger already; only what comes after is written.
        for r in read_ledger(dir) {
            match r.body {
                FleetBody::Event { event } => {
                    if let FleetEvent::Started { slot, session } = &event {
                        hub.last_session.insert(slot.clone(), session.clone());
                    }
                    hub.model.step(event, r.ts_ms);
                }
                FleetBody::Work { .. } => {
                    hub.model.note_record(&r);
                    restore_work(&r, &mut hub.done_trees, &mut hub.integrated);
                }
                FleetBody::Command { .. } => {}
            }
        }
        let slots: Vec<String> = hub.model.slots().keys().cloned().collect();
        for slot in slots {
            if let Some(dir) = hub.slot_dir(&slot) {
                hub.worktrees.insert(slot, dir);
            }
        }
        // Done parts went up before the hub stopped; their starts are gone with it.
        let done: Vec<String> = hub
            .model
            .tasks()
            .filter(|t| t.state == belay_core::fleet::model::TaskState::Done)
            .map(|t| t.id.clone())
            .collect();
        hub.lifted.extend(done);
        // `integrated` starts empty: a slot that never got a done task's work gets it
        // when it needs it, and a 3-way merge of work already there changes nothing.
        // Sessions of the old hub may still finish a turn: stop them before new ones start.
        stop_old_sessions(dir);
        eprintln!("belay fleet hub {}: restarted", hub.meta.id);
        hub.apply(FleetEvent::HubRestarted);
    } else {
        eprintln!("belay fleet hub {}: up", hub.meta.id);
        hub.apply(FleetEvent::Up);
    }
    hub.serve(rx);
    let _ = fs::remove_file(&sock);
    eprintln!("belay fleet hub {}: down", hub.meta.id);
    Ok(())
}

/// A restarted hub takes back what its work records say: each done task's tree, and
/// which slots already have which tasks' work (so it is not brought, or told, twice).
fn restore_work(
    r: &FleetRecord,
    done_trees: &mut BTreeMap<String, (String, u64)>,
    integrated: &mut std::collections::BTreeSet<(String, String)>,
) {
    match &r.body {
        FleetBody::Work {
            work: Work::Snapshot { task, tree },
            ..
        } => {
            done_trees.insert(task.clone(), (tree.clone(), r.seq));
        }
        FleetBody::Work {
            work: Work::Brought { task, slot, .. },
            ..
        } => {
            integrated.insert((slot.clone(), task.clone()));
        }
        _ => {}
    }
}

fn end_torn_line(path: &Path, file: &File) {
    if fs::read(path).is_ok_and(|b| b.last().is_some_and(|c| *c != b'\n')) {
        let _ = (&*file).write_all(b"\n");
    }
}

/// Debug builds only: the e2e tests set `BELAY_TEST_PARENT` to their own pid. A test run
/// that is killed runs no `Drop`, so its hubs (and their sessions) would run on for hours.
pub(crate) fn test_parent_gone() -> bool {
    if !cfg!(debug_assertions) {
        return false;
    }
    let Some(pid) = std::env::var("BELAY_TEST_PARENT")
        .ok()
        .and_then(|p| p.parse::<i32>().ok())
    else {
        return false;
    };
    // SAFETY: kill with signal 0 only checks that the process exists.
    let alive = unsafe { libc::kill(pid, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
    !alive
}

/// How long to wait for a message before the next tick, `since_tick` after the last.
fn tick_wait(since_tick: Duration) -> Duration {
    TICK.saturating_sub(since_tick)
}

fn listen(listener: UnixListener, tx: Sender<Msg>) {
    // Each connection's line is read on its own thread: a client that is slow to write
    // (or never does) must not hold up the others for the read timeout. Requests still
    // reach the hub's loop one at a time, in order of arrival.
    for conn in listener.incoming().flatten() {
        let tx = tx.clone();
        thread::spawn(move || read_request(conn, tx));
    }
}

fn read_request(conn: UnixStream, tx: Sender<Msg>) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
    let mut line = String::new();
    let Ok(reader) = conn.try_clone() else {
        return;
    };
    if BufReader::new(reader)
        .take(1 << 20)
        .read_line(&mut line)
        .is_err()
        || line.is_empty()
    {
        return;
    }
    match serde_json::from_str::<HubRequest>(&line) {
        Ok(HubRequest::Ping) => reply(conn, "pong"),
        Ok(req) => {
            let pid = peer_pid(&conn);
            let _ = tx.send(Msg::Request(req, conn, pid));
        }
        Err(e) => reply(conn, &format!("bad request: {e}")),
    }
}

fn reply(mut conn: UnixStream, message: &str) {
    let r = Reply {
        output: None,
        message: Some(message.to_string()),
    };
    if let Ok(mut line) = serde_json::to_string(&r) {
        line.push('\n');
        let _ = conn.write_all(line.as_bytes());
    }
}

impl Hub {
    fn serve(&mut self, rx: Receiver<Msg>) {
        // Tick on elapsed time: a busy hub (status polls, output) must still tick.
        let mut last_tick = std::time::Instant::now();
        loop {
            match rx.recv_timeout(tick_wait(last_tick.elapsed())) {
                Ok(Msg::Event(e)) => {
                    self.apply(e);
                }
                Ok(Msg::TurnEnded(slot)) => {
                    // The model says when a oneshot ends (`release`): not while its
                    // task's check runs, so a failure can come back to it (F5b).
                    if let Some(p) = self.procs.get_mut(&slot) {
                        p.turns += 1;
                    }
                    self.apply(FleetEvent::Activity {
                        slot,
                        turn_ended: true,
                    });
                }
                Ok(Msg::Request(req, conn, pid)) => {
                    let text = self.request(req, pid);
                    reply(conn, &text);
                }
                Ok(Msg::Tree(task, tree)) => {
                    self.pending_trees.insert(task, tree);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if last_tick.elapsed() >= TICK {
                last_tick = std::time::Instant::now();
                self.apply(FleetEvent::Tick);
                // A terminal session that went away without `leave` (F4).
                let gone: Vec<String> = self
                    .attached
                    .iter()
                    .filter(|s| {
                        self.procs
                            .get(*s)
                            .is_none_or(|p| unsafe { libc::kill(p.pid, 0) } != 0)
                    })
                    .cloned()
                    .collect();
                for slot in gone {
                    self.attached.remove(&slot);
                    self.apply(FleetEvent::Exited { slot, code: None });
                }
                if self.down_since.is_none() && test_parent_gone() {
                    eprintln!(
                        "belay fleet hub {}: its test is gone; going down",
                        self.meta.id
                    );
                    self.apply(FleetEvent::Down);
                }
            }
            if let Some(since) = self.down_since {
                let live = self
                    .model
                    .slots()
                    .values()
                    .any(|s| matches!(s.state, SlotState::Starting | SlotState::Running));
                if !live {
                    return;
                }
                // Sessions that ignore every signal: do not wait forever.
                if since.elapsed() > Duration::from_secs(30) {
                    for p in self.procs.values() {
                        kill(-p.pid, libc::SIGKILL);
                    }
                    return;
                }
            }
        }
    }

    /// Who sent a request: the slot whose session's process tree it came from, or the
    /// user. Without the caller's pid (an OS where the hub cannot see it), the slot the
    /// caller names.
    fn actor(&self, pid: Option<i32>, claimed: Option<&str>) -> Actor {
        match pid {
            Some(pid) => {
                let tree = ancestors(pid);
                self.procs
                    .iter()
                    .find(|(_, p)| tree.contains(&p.pid))
                    .map_or(Actor::User, |(id, _)| Actor::Slot { id: id.clone() })
            }
            None => match claimed {
                Some(id) if self.procs.contains_key(id) => Actor::Slot { id: id.into() },
                _ => Actor::User,
            },
        }
    }

    fn request(&mut self, req: HubRequest, pid: Option<i32>) -> String {
        let claimed = match &req {
            HubRequest::Task { claimed_slot, .. }
            | HubRequest::Msg { claimed_slot, .. }
            | HubRequest::Inbox { claimed_slot } => claimed_slot.clone(),
            _ => None,
        };
        let by = self.actor(pid, claimed.as_deref());
        let user = by == Actor::User;
        let event = match req {
            HubRequest::Ping => return "pong".into(),
            HubRequest::Status => return self.status(),
            // Fleet control is the user's; a session asks through its role's verbs.
            HubRequest::Down
            | HubRequest::Scale { .. }
            | HubRequest::Pause
            | HubRequest::Resume
                if !user =>
            {
                return "belay: refused. Fleet control is yours to run: use your own terminal, not a belay session.".into();
            }
            HubRequest::Stop { slot } => FleetEvent::StopRequest { by, slot },
            HubRequest::Down => FleetEvent::Down,
            HubRequest::Spawn { role } => FleetEvent::SpawnRequest { by, role },
            HubRequest::Scale { role, n } => FleetEvent::Scale { role, n },
            HubRequest::Pause => FleetEvent::Pause,
            HubRequest::Resume => FleetEvent::Resume,
            HubRequest::Join { .. } if !user => {
                return "belay: refused. A fleet session cannot join another.".into();
            }
            HubRequest::Join {
                role,
                name,
                pid,
                cwd,
            } => return self.join(role, name, pid, cwd),
            HubRequest::Wake { slot, from } => FleetEvent::Wake { slot, from },
            HubRequest::Leave { slot } => {
                // A session may end only its own watch (dogfood review): the user's
                // launcher, or the slot's own process tree.
                if matches!(&by, Actor::Slot { id } if *id != slot) {
                    return format!("belay: refused. Only {slot} or the user can end its slot.");
                }
                if !self.attached.remove(&slot) {
                    return format!("belay: refused. {slot} did not join from a terminal.");
                }
                FleetEvent::Exited {
                    slot,
                    code: Some(0),
                }
            }
            HubRequest::Inbox { .. } => return self.inbox_of(&by),
            HubRequest::Msg { to, kind, text, .. } => {
                let to = match to.as_str() {
                    "user" => Address::User,
                    t => match (t.strip_prefix('@'), t.strip_prefix('#')) {
                        (Some(r), _) => Address::Role { name: r.into() },
                        (_, Some(team)) => Address::Team { name: team.into() },
                        _ => Address::Slot { id: t.into() },
                    },
                };
                let kind = match kind {
                    Some(k) => match serde_json::from_value(serde_json::Value::String(k.clone())) {
                        Ok(k) => k,
                        Err(_) => return format!("belay: refused. No message kind `{k}` (question, answer, report, notice, review, handoff)."),
                    },
                    None => match self.model.default_kind(&by, &to) {
                        Some(k) => k,
                        None => return "belay: refused. No channel of this fleet lets this sender reach that receiver.".into(),
                    },
                };
                FleetEvent::Message {
                    from: by,
                    to,
                    kind,
                    text,
                }
            }
            HubRequest::Task { verb, .. } => match verb {
                TaskVerb::List => return render_tasks(&self.model),
                TaskVerb::Show { task } => return render_task(&self.model, &task),
                TaskVerb::Add {
                    title,
                    body,
                    depends_on,
                    scope,
                    role,
                    check,
                } => FleetEvent::TaskAdd {
                    by,
                    title,
                    body,
                    depends_on,
                    scope,
                    role,
                    check,
                },
                TaskVerb::Assign { task, to } => FleetEvent::TaskAssign { by, task, to },
                TaskVerb::Claim { task } => FleetEvent::TaskClaim { by, task },
                TaskVerb::Done { task, summary } => FleetEvent::TaskDone { by, task, summary },
                TaskVerb::Blocked { task, reason } => FleetEvent::TaskBlocked { by, task, reason },
                TaskVerb::Reject { task, reason } => FleetEvent::TaskReject { by, task, reason },
                TaskVerb::Drop { task, reason } => FleetEvent::TaskDrop { by, task, reason },
            },
        };
        let claimer = match &event {
            FleetEvent::TaskClaim {
                by: Actor::Slot { id },
                ..
            } => Some(id.clone()),
            _ => None,
        };
        let recs = self.apply(event);
        // A claimed task is not delivered: bring its dependencies' work in here, and
        // record where it starts, as a delivery does.
        let mut extra = None;
        if let Some(sid) = claimer {
            if let Some(tid) = self.model.slots().get(&sid).and_then(|s| s.task.clone()) {
                let text = format!("Task {tid}:");
                extra = self.bring_in_dependencies(&sid, &text);
                self.record_base(&sid, &text);
            }
        }
        // The answer to the asker, if the model gave one; else what happened.
        let mut answers: Vec<String> = recs
            .iter()
            .filter_map(|r| match &r.body {
                FleetBody::Command {
                    command: Cmd::Answer { text, .. },
                    ..
                } => Some(text.clone()),
                _ => None,
            })
            .collect();
        answers.extend(extra);
        if !answers.is_empty() {
            return answers.join("\n");
        }
        let said: Vec<String> = recs
            .iter()
            .filter_map(|r| match &r.body {
                FleetBody::Command {
                    command: Cmd::Notify { text },
                    ..
                } => Some(text.clone()),
                FleetBody::Command { reason, .. } => Some(reason.clone()),
                _ => None,
            })
            .collect();
        if said.is_empty() {
            "Nothing to do.".into()
        } else {
            said.join("\n")
        }
    }

    /// Step the model with `event` and with what running its commands caused; record
    /// and run every command. Returns the records of `event` itself.
    fn apply(&mut self, event: FleetEvent) -> Vec<FleetRecord> {
        if matches!(event, FleetEvent::Down) && self.down_since.is_none() {
            self.down_since = Some(std::time::Instant::now());
        }
        let mut first = None;
        self.queue.push_back(event);
        while let Some(e) = self.queue.pop_front() {
            if let FleetEvent::Exited { slot, .. } = &e {
                self.procs.remove(slot);
            }
            let recs = self.model.step(e, now_ms());
            for r in &recs {
                if let Ok(line) = serde_json::to_string(r) {
                    let _ = writeln!(self.ledger, "{line}");
                }
            }
            if let Some(cause) = recs.first().map(|r| r.seq) {
                self.snapshot_done(cause);
            }
            // A done part's work goes up before its lead hears "is done": a lead that
            // looks at once must find it (the e2e test hid this with a sleep).
            self.bring_up_done_parts();
            for r in &recs {
                if let FleetBody::Command { command, .. } = &r.body {
                    self.running = r.seq;
                    self.run(command);
                }
            }
            if first.is_none() {
                first = Some(recs);
            }
        }
        self.write_claims();
        self.bring_up_done_parts();
        first.unwrap_or_default()
    }

    fn run(&mut self, c: &Cmd) {
        match c {
            Cmd::Spawn {
                slot,
                workspace,
                model,
                permission_mode,
                brief,
                worktree_of,
                ..
            } => match self.spawn(slot, workspace, worktree_of.as_deref(), model.as_deref(), permission_mode.as_deref(), brief) {
                Ok(session) => {
                    self.last_session.insert(slot.clone(), session.clone());
                    self.queue.push_back(FleetEvent::Started {
                        slot: slot.clone(),
                        session,
                    })
                }
                Err(e) => {
                    eprintln!("belay fleet hub: start {slot}: {e:#}");
                    inbox(&self.dir, &format!("Could not start {slot}: {e:#}"));
                    self.queue.push_back(FleetEvent::Exited {
                        slot: slot.clone(),
                        code: Some(127),
                    });
                }
            },
            // A terminal session is the user's: the fleet lets go of it, it goes on alone.
            Cmd::Stop { slot } if self.attached.contains(slot) => {
                self.attached.remove(slot);
                inbox(&self.dir, &format!("{slot} left the fleet; its terminal session goes on without it."));
                self.queue.push_back(FleetEvent::Exited {
                    slot: slot.clone(),
                    code: Some(0),
                });
            }
            Cmd::Stop { slot } => {
                if let Some(p) = self.procs.get_mut(slot) {
                    // A stream session ends cleanly when its stdin closes (F-S1).
                    p.stdin = None;
                    let (pid, exited) = (p.pid, Arc::clone(&p.exited));
                    thread::spawn(move || {
                        for (wait, sig) in [(TERM_AFTER, libc::SIGTERM), (KILL_AFTER, libc::SIGKILL)] {
                            thread::sleep(wait);
                            if exited.load(Ordering::SeqCst) {
                                return;
                            }
                            kill(-pid, sig);
                        }
                    });
                }
            }
            Cmd::Interrupt { slot } => {
                let line = serde_json::json!({
                    "type": "control_request",
                    "request_id": format!("belay-{}", now_ms()),
                    "request": {"subtype": "interrupt"},
                });
                self.write_line(slot, &line);
            }
            Cmd::Deliver { to, from, kind, text } => {
                let mut text = text.clone();
                if let (belay_core::fleet::spec::MsgKind::Task, Address::Slot { id }) = (kind, to) {
                    // Its dependencies' work first, then the base the audit diffs from.
                    if let Some(note) = self.left_work_note(id, &text) {
                        text = format!("{text}\n\n{note}");
                    }
                    if let Some(note) = self.bring_in_dependencies(id, &text) {
                        text = format!("{text}\n\n{note}");
                    }
                    self.record_base(id, &text);
                }
                let text = &text;
                let from = match from {
                    Actor::User => "the user".to_string(),
                    Actor::Belay => "belay".to_string(),
                    Actor::Slot { id } => id.clone(),
                };
                let kind = format!("{kind:?}").to_lowercase();
                match to {
                    // A message (not a task) for a session in a terminal waits in its
                    // inbox; its daemon shows it at the next hook (F4). A headless session
                    // gets it as its next turn, as 0.1's default (look) fleets did: fleets
                    // are frozen.
                    Address::Slot { id }
                        if kind != "task"
                            && self.procs.get(id).is_none_or(|p| p.stdin.is_none()) =>
                    {
                        let line = serde_json::json!({"ts_ms": now_ms(), "from": from, "kind": kind, "text": text});
                        let path = self.dir.join("slots").join(format!("{}.inbox.jsonl", file_name(id)));
                        let _ = paths::ensure_private_dir(&self.dir.join("slots"));
                        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                            let _ = writeln!(f, "{line}");
                        }
                    }
                    Address::Slot { id } => {
                        let line = serde_json::json!({
                            "type": "user",
                            "message": {"role": "user", "content": format!("[belay fleet: {kind} from {from}]\n{text}")},
                        });
                        if !self.write_line(id, &line) {
                            inbox(&self.dir, &format!("Undelivered {kind} for {id} from {from}: {text}"));
                        }
                    }
                    _ => inbox(&self.dir, &format!("{kind} from {from}: {text}")),
                }
            }
            Cmd::Notify { text } => inbox(&self.dir, text),
            Cmd::Attach { role } => inbox(
                &self.dir,
                &format!("Role {role} runs in your terminal: start it in this repo with `belay claude --belay-fleet {} --belay-role {role}`.", self.meta.id),
            ),
            // Answers go back with the request's reply; waits need nothing done.
            Cmd::Release { slot } => {
                if let Some(p) = self.procs.get_mut(slot) {
                    p.stdin = None;
                }
            }
            Cmd::RunCheck { task, slot, command } => self.run_check(task, slot, command),
            Cmd::Backoff { .. } | Cmd::TaskRetry { .. } | Cmd::ResumeAt { .. } | Cmd::Answer { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests;
