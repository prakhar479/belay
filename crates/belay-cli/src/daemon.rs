//! Session daemon: holds the controller, connector, and store for one session.
//! One thread per connection; one mutex around all session state.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use belay_core::{Controller, Response};

use crate::connector::{self, Connector};
use crate::ipc::{Reply, Request};
use crate::paths::{now_ms, socket_path};
use crate::store::Store;

pub(crate) mod attention;

/// Time to encode and send a reply. A hook later than its deadline minus this is late.
const REPLY_MARGIN_MS: u64 = 10;
/// A client gets this long to send its request line.
const READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Larger than any hook payload or filter request (8 MiB of output, JSON-escaped).
const MAX_REQUEST_BYTES: u64 = 64 << 20;

struct State {
    controller: Controller,
    connector: Box<dyn Connector>,
    store: Store,
    /// Where this project's learned shape levels live (E9), once the session started.
    memory: Option<PathBuf>,
    /// When to tell the user they are needed, and where (V3).
    attention: attention::Attention,
    channels: attention::Channels,
    /// The session's working directory, once it started (for the fleet watcher).
    cwd: Option<String>,
    /// The fleet's claims file, and when it was last read (F5c).
    claims: Option<(PathBuf, Option<std::time::SystemTime>)>,
    /// Inboxes this daemon reads at each hook, how much of each was read, and whether
    /// it is the commons one (C2) or the fleet slot's (F4).
    inbox: Vec<(PathBuf, Seen, bool)>,
    /// This session's id and project, for the machine's learned fixes (F2).
    session_id: String,
    project: Option<String>,
    /// How much of the machine's fix store this session has read, once it started:
    /// fixes other sessions learn while it runs reach it (a fleet's workers).
    fixes_seen: Option<u64>,
    /// The session's directory: snapshots build their temporary index here (U6.2).
    dir: PathBuf,
    /// A resume through the agent's picker waits for its first SessionStart (U8).
    picker: bool,
    /// A resumed conversation continues its belay state (`resume = continue`).
    resume: bool,
    /// The conversation id this session last mapped to itself in `by-session/`.
    mapped: Option<String>,
    /// The first turn number this session's snapshots may take: after the runs it
    /// continues, so `t-N` names one snapshot across them (U8).
    snapshot_floor: u64,
}

/// What a launch asks the daemon to continue (U8).
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Resume {
    #[default]
    No,
    /// The earlier session that ran the conversation, and the conversation's id.
    From {
        dir: PathBuf,
        conversation: Option<String>,
    },
    /// The agent's picker chooses: resolved at its SessionStart.
    Picker,
}

/// The turn number after the last snapshot of these runs (0 when they kept none).
fn next_turn(runs: &[crate::store::Run]) -> u64 {
    runs.iter()
        .flat_map(|r| &r.entries)
        .filter_map(|e| match &e.body {
            belay_core::Body::Snapshot {
                id, undo: false, ..
            } => id.strip_prefix("t-")?.parse::<u64>().ok().map(|n| n + 1),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

/// Replay earlier runs into the controller (U8). Returns the last run's highest seq.
fn restore_runs(controller: &mut Controller, runs: &[crate::store::Run]) -> u64 {
    let all = crate::store::chain_entries(runs);
    controller.restore(&all, &|h| crate::store::read_chain_blob(runs, h));
    all.iter().map(|e| e.seq).max().unwrap_or(0)
}

/// The earlier session that ran `conversation`, when it is not this one, with its runs.
fn earlier_runs(conversation: &str, own: &Path) -> Option<(String, Vec<crate::store::Run>)> {
    let dir = crate::paths::conversation_dir(conversation)?;
    let same = |a: &Path, b: &Path| fs::canonicalize(a).ok() == fs::canonicalize(b).ok();
    if same(&dir, own) {
        return None;
    }
    let name = dir.file_name()?.to_string_lossy().to_string();
    let runs = crate::store::read_chain(&dir).ok()?;
    Some((name, runs))
}

/// Snapshots decided but not yet recorded. Shutdown waits for them, so the last turn's
/// snapshot is kept when the agent exits right after it (U6.2).
static SNAPSHOTS_PENDING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// One snapshot at a time: each numbers itself from the refs the last one wrote.
static SNAPSHOT_LOCK: Mutex<()> = Mutex::new(());

/// This session's directory, when it keeps a commons card (C1): every exit marks the
/// card ended.
static CARD_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// The card's shape last written (`commons::shape_of`), to skip unchanged writes.
static CARD_LAST: Mutex<String> = Mutex::new(String::new());

/// Write this session's card if it changed. After the reply: never on the hook's time.
fn write_card(state: &Mutex<State>) {
    let Some(dir) = CARD_DIR.get() else { return };
    // Held from the snapshot to the write: a slower thread must not write an older card.
    let mut last = CARD_LAST.lock().unwrap_or_else(|e| e.into_inner());
    let card = {
        let st = state.lock().unwrap_or_else(|e| e.into_inner());
        st.controller.card(now_ms())
    };
    if card.agent.is_empty() || last.as_str() == ENDED {
        return; // not started yet, or ended
    }
    let shape = crate::commons::shape_of(&card);
    // Activity alone is rewritten at most every 30 s, so `active … ago` stays near.
    let stale = card
        .last_activity_ms
        .saturating_sub(LAST_CARD_ACTIVITY.load(std::sync::atomic::Ordering::Relaxed))
        > 30_000;
    if *last == shape && !stale {
        return;
    }
    if crate::commons::write_card(dir, &card).is_ok() {
        *last = shape;
        LAST_CARD_ACTIVITY.store(card.last_activity_ms, std::sync::atomic::Ordering::Relaxed);
    }
}

static LAST_CARD_ACTIVITY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// CARD_LAST after the card was marked ended: no later write may undo it.
const ENDED: &str = "\u{0}ended";

/// Keep the handoff this session leaves now in the project's memory (U4b): at its end
/// and before a compaction. A later one replaces it.
fn keep_handoff(st: &State) {
    let Some(project) = &st.project else { return };
    if let Some(m) = st.controller.handoff_memory(project, now_ms()) {
        if let Err(e) = crate::memorystore::save_handoff(m) {
            eprintln!("belayd: keep the handoff: {e:#}");
        }
    }
}

fn card_ended() {
    if let Some(dir) = CARD_DIR.get() {
        let mut last = CARD_LAST.lock().unwrap_or_else(|e| e.into_inner());
        crate::commons::mark_ended(dir);
        *last = ENDED.into();
    }
}

/// When the daemon last got a request (ms), for the idle exit.
static LAST_REQUEST_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn run(
    dir: PathBuf,
    parent_pid: Option<i32>,
    agent: &str,
    idle_exit_minutes: Option<u64>,
    resume: Resume,
) -> Result<()> {
    let sock = socket_path(&dir);
    // The socket accepts hook events and `belay allow`: keep it in a private dir.
    crate::paths::ensure_private_dir(&dir)?;
    if let Some(parent) = sock.parent() {
        crate::paths::ensure_private_dir(parent)?;
    }
    // Bind under a temporary name and link: the socket appears at its path only once
    // it listens, so a client never finds a socket that refuses (bind creates the file
    // before listen).
    // The temporary name is no longer than `belayd.sock`, so it fits the path limit too.
    let tmp = sock.with_file_name(format!("{}.t", std::process::id() % 10_000_000));
    let _ = fs::remove_file(&tmp);
    let listener = UnixListener::bind(&tmp).with_context(|| format!("bind {}", tmp.display()))?;
    // `link` fails if the path exists, so two daemons for one session cannot both take
    // it (a rename would replace a live daemon's socket and orphan it). A socket nobody
    // listens on is stale: replace it.
    let linked = fs::hard_link(&tmp, &sock).or_else(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists
            && crate::ipc::connect_now(&sock).is_err_and(|c| {
                matches!(
                    c.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                )
            })
        {
            fs::remove_file(&sock)?;
            fs::hard_link(&tmp, &sock)
        } else {
            Err(e)
        }
    });
    let _ = fs::remove_file(&tmp);
    if let Err(e) = linked {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow::bail!("a daemon is already running for {}", dir.display());
        }
        return Err(e).with_context(|| format!("link {}", sock.display()));
    }
    let mut connector = connector::for_agent(agent)?;
    // A plugin daemon (it exits when idle): the agent's shell has no BELAY_SOCKET, so a
    // wrapped run is told where this daemon is.
    if idle_exit_minutes.is_some() {
        connector.set_socket(&sock);
    }
    let mut cfg = crate::config::load().session();
    // A fleet's sessions (F5a; fleets are frozen): the guard denies, and the sessions
    // hear each other, whatever this machine's config says.
    if std::env::var_os("BELAY_FLEET_SLOT").is_some() {
        cfg.guard = belay_core::GuardSetting::Deny;
        cfg.commons = true;
    }
    let resume_on = crate::config::load().resume_continues();
    let mut controller = Controller::new(cfg, connector.capabilities());
    // A ledger from an earlier daemon of this session (a plugin daemon that exited idle,
    // or crashed), with the runs it resumed: the state goes on as if it never stopped.
    let mut resumed_now = None;
    let mut snapshot_floor = 0;
    let own = crate::store::read_chain(&dir)
        .ok()
        .filter(|runs| runs.iter().any(|r| !r.entries.is_empty()));
    if let Some(runs) = own {
        restore_runs(&mut controller, &runs);
        snapshot_floor = next_turn(&runs[..runs.len() - 1]);
    } else if let Resume::From {
        dir: from,
        conversation,
    } = &resume
    {
        // A new session for a resumed conversation: continue the earlier runs' story.
        if let Ok(runs) = crate::store::read_chain(from) {
            let seq = restore_runs(&mut controller, &runs);
            snapshot_floor = next_turn(&runs);
            let name = from.file_name().map(|n| n.to_string_lossy().to_string());
            resumed_now = name.map(|n| (n, seq, conversation.clone()));
        }
    }
    // A fleet's hub starts its sessions with BELAY_FLEET_SLOT set (F5a).
    controller.set_in_fleet(std::env::var_os("BELAY_FLEET_SLOT").is_some());
    // Its role's write scope, and where the fleet's claims are (F5c).
    let rules: Option<belay_core::fleet::scope::RulesFile> = std::env::var("BELAY_FLEET_RULES")
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok());
    if let Some(r) = &rules {
        controller.set_fleet_rules(belay_core::fleet::scope::Rules::new(r));
    }
    let mut store = Store::open(&dir)?;
    if let Some((from, seq, conversation)) = resumed_now {
        let d = controller.resumed(&from, seq, conversation.clone(), true, now_ms());
        store.persist(&d)?;
        eprintln!("belayd: continues session {from}");
    }
    // Without a token file (a daemon started by hand), control requests are open.
    let token: Arc<Option<String>> = Arc::new(crate::paths::read_token(&dir));
    let cfg = crate::config::load();
    let state = Arc::new(Mutex::new(State {
        controller,
        connector,
        store,
        memory: None,
        attention: attention::Attention::new(cfg.notify, cfg.notify_quiet),
        channels: attention::Channels {
            session: dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            repo: String::new(),
            // The launcher passes its pid; its terminal is the user's.
            bell: parent_pid.is_some() && std::env::var_os("BELAY_TEST_PARENT").is_none(),
            // A test run (e2e sets BELAY_TEST_PARENT) shows no desktop popups; the
            // command channel stays, so tests can see what was sent.
            desktop: std::env::var_os("BELAY_TEST_PARENT").is_none(),
            command: cfg.notify_command.clone(),
        },
        cwd: None,
        claims: rules
            .as_ref()
            .and_then(|r| r.claims.clone())
            .map(|p| (PathBuf::from(p), None)),
        inbox: {
            let mut v: Vec<(PathBuf, Seen, bool)> = Vec::new();
            if let Some(p) = rules.as_ref().and_then(|r| r.inbox.clone()) {
                let p = PathBuf::from(p);
                let seen = inbox_seen(&p);
                v.push((p, seen, false));
            }
            // The user's own messages reach the agent whatever `commons` says.
            let p = crate::commons::inbox_path(&dir);
            let seen = inbox_seen(&p);
            v.push((p, seen, true));
            v
        },
        session_id: dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        project: None,
        fixes_seen: None,
        dir: dir.clone(),
        picker: resume == Resume::Picker && resume_on,
        resume: resume_on,
        mapped: None,
        snapshot_floor,
    }));
    let _ = CARD_DIR.set(dir.clone());
    eprintln!("belayd: listening on {}", sock.display());

    if let Some(pid) = parent_pid {
        let sock = sock.clone();
        let state = Arc::clone(&state);
        thread::spawn(move || watch_parent(pid, &sock, &state));
    }
    LAST_REQUEST_MS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    if crate::config::load().awareness {
        let state = Arc::clone(&state);
        thread::spawn(move || watch_fleet(dir, &state));
    }
    if lock(&state).attention.on() {
        let state = Arc::clone(&state);
        thread::spawn(move || watch_done(&state));
    }
    if let Some(minutes) = idle_exit_minutes {
        let sock = sock.clone();
        let state = Arc::clone(&state);
        thread::spawn(move || watch_idle(minutes, &sock, &state));
    }

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let state = Arc::clone(&state);
                let sock = sock.clone();
                let token = Arc::clone(&token);
                thread::spawn(move || {
                    if let Err(e) = serve(stream, &state, &sock, &token) {
                        eprintln!("belayd: {e:#}");
                    }
                    write_card(&state);
                });
            }
            Err(e) => eprintln!("belayd: accept: {e}"),
        }
    }
    Ok(())
}

/// Per-project memory (E9): at session start, take up the shape levels earlier sessions
/// in the project learned; after an adaptation, save them. Kept in
/// `<home>/projects/<hash of the repo root>.json`.
fn remember(st: &mut State, decisions: &[belay_core::Decision]) -> Result<()> {
    for e in decisions.iter().flat_map(|d| &d.entries) {
        if let belay_core::Body::Event {
            event: belay_core::Event::SessionStarted { cwd, .. },
        } = &e.body
        {
            st.cwd = Some(cwd.clone());
            let cwd = Path::new(cwd);
            let root = crate::audit::repo_root(cwd).unwrap_or_else(|_| cwd.to_path_buf());
            let key = belay_core::text::sha256_hex(&root.to_string_lossy());
            let file = crate::paths::home()
                .join("projects")
                .join(format!("{}.json", &key[..16]));
            let levels: std::collections::BTreeMap<String, u8> = fs::read_to_string(&file)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| serde_json::from_value(v.get("shape_levels")?.clone()).ok())
                .unwrap_or_default();
            let d = st.controller.restore_levels(&levels, now_ms());
            st.store.persist(&d)?;
            st.memory = Some(file);
        }
    }
    let adapted = decisions
        .iter()
        .flat_map(|d| &d.entries)
        .any(|e| matches!(e.body, belay_core::Body::Adaptation { .. }));
    if let (true, Some(file)) = (adapted, &st.memory) {
        if let Some(dir) = file.parent() {
            let _ = crate::paths::ensure_private_dir(dir);
        }
        let body = serde_json::json!({ "shape_levels": st.controller.learned_levels() });
        let _ = fs::write(file, body.to_string());
    }
    Ok(())
}

/// Send the alerts the user may hear now (rate limit, quiet window), off this thread.
fn alert(st: &mut State, alerts: Vec<attention::Alert>) {
    let now = now_ms();
    let minute = attention::local_minute(now);
    let alerts: Vec<_> = alerts
        .into_iter()
        .filter(|a| st.attention.admit(a.kind, now, minute))
        .collect();
    if alerts.is_empty() {
        return;
    }
    if let Some(p) = &st.project {
        st.channels.repo.clone_from(p);
    }
    attention::deliver(st.channels.clone(), alerts, now);
}

/// A finished turn that stays idle is "done" (V3): checked each second.
fn watch_done(state: &Mutex<State>) {
    loop {
        thread::sleep(Duration::from_secs(1));
        let mut st = lock(state);
        if let Some(a) = st.attention.idle_due(now_ms()) {
            alert(&mut st, vec![a]);
        }
    }
}

/// Read the fleet's claims again if the file changed (F5c). A missing or bad file
/// leaves the last ones: fail open.
fn refresh_claims(st: &mut State) {
    let Some((path, seen)) = &mut st.claims else {
        return;
    };
    let Ok(modified) = fs::metadata(&*path).and_then(|m| m.modified()) else {
        return;
    };
    if *seen == Some(modified) {
        return;
    }
    *seen = Some(modified);
    if let Ok(claims) = fs::read_to_string(&*path).map_err(|_| ()).and_then(|t| {
        serde_json::from_str::<Vec<belay_core::fleet::scope::ClaimFile>>(&t).map_err(|_| ())
    }) {
        st.controller.set_fleet_claims(&claims);
    }
}

/// New messages in this session's fleet inbox become events, so this hook's reply can
/// carry them (F4). A missing file means none yet.
fn read_inbox(st: &mut State) -> Result<()> {
    let mut lines = Vec::new();
    let mut read_to = Vec::new();
    for (i, (path, offset, commons)) in st.inbox.iter().enumerate() {
        let (new, next) = new_inbox_lines(path, offset);
        lines.extend(new.into_iter().map(|v| (v, *commons)));
        read_to.push((i, next));
    }
    for (v, commons) in lines {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let opt = |k: &str| Some(s(k)).filter(|x| !x.is_empty());
        let ev = belay_core::Event::Message {
            from: s("from"),
            kind: s("kind"),
            text: s("text"),
            channel: commons.then(|| "commons".to_string()),
            from_session: if commons { opt("from_session") } else { None },
            repo: if commons { opt("repo") } else { None },
        };
        let d = st.controller.handle(ev, now_ms());
        st.store.persist(&d)?;
    }
    // Only once the messages are in the ledger: a failed write delivers them again.
    for (i, next) in read_to {
        let (path, seen, _) = &mut st.inbox[i];
        // Only a new offset or file is written down; a new stamp alone stays in memory.
        if (seen.offset, &seen.head) != (next.offset, &next.head) {
            mark_inbox_seen(path, &next);
        }
        *seen = next;
    }
    Ok(())
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

const COMMONS_OFF: &str = "belay: refused. `commons = off` in belay's config: this session keeps no card, and sends and shares nothing.";

/// A commons request (`belay msg`, `share`, `ask`, `card`, `handoff`): the lock only
/// around the controller; cards, inboxes, findings, and hubs outside it.
fn commons_request(state: &Mutex<State>, req: Request) -> Result<String> {
    if CARD_DIR.get().is_none() {
        return Ok(COMMONS_OFF.into());
    }
    // Requests carry who sends them (identity, not authentication): a shell of another
    // session that points BELAY_SOCKET here would act as this session.
    let from = match &req {
        Request::Send { from, .. }
        | Request::Share { from, .. }
        | Request::Ask { from, .. }
        | Request::Card { from, .. } => from.clone(),
        _ => None,
    };
    if let Some(from) = from {
        let me = lock(state).session_id.clone();
        if from != me {
            return Ok(format!(
                "belay: refused. This socket is session {me}'s, and the request came from session {from}. Run belay from that session's own shell."
            ));
        }
    }
    match req {
        Request::Send { to, kind, text, .. } => send_commons(state, &to, kind, &text),
        Request::Share {
            kind,
            text,
            tags,
            files,
            code,
            ..
        } => {
            if text.trim().is_empty() {
                return Ok("belay: refused. A finding needs text.".into());
            }
            let now = now_ms();
            let (me, project) = my_card_and_project(state, now);
            let f = crate::findings::add_in(&crate::findings::dir(), |id| {
                belay_core::commons::clean_finding(
                    id,
                    kind.as_deref(),
                    &text,
                    &tags,
                    &files,
                    code.as_deref(),
                    &project,
                    &me.name,
                    &me.id,
                    now,
                )
            })?;
            {
                let mut st = lock(state);
                let d = st.controller.shared(&f, now);
                st.store.persist(&d)?;
            }
            Ok(format!("Shared {}. Others find it with `belay ask`.", f.id))
        }
        Request::Ask {
            query, here, json, ..
        } => {
            let now = now_ms();
            let (me, project) = my_card_and_project(state, now);
            let hits =
                belay_core::commons::search(&query, &crate::findings::searchable(), &project, here);
            {
                let mut st = lock(state);
                let ids = hits.iter().map(|h| h.finding.id.clone()).collect();
                let d = st.controller.asked(&query, ids, now);
                st.store.persist(&d)?;
            }
            if json {
                return Ok(serde_json::to_string_pretty(&hits)? + "\n");
            }
            let cards = crate::commons::read_cards(&crate::paths::sessions_dir());
            let know = crate::findings::might_know(&cards, &me.id, &query);
            Ok(crate::findings::render_hits(&query, &hits, &know, now))
        }
        Request::Card {
            name,
            task,
            note,
            offers,
            join,
            leave,
            handoff,
            ..
        } => {
            if let Some(n) = name
                .as_deref()
                .filter(|n| !belay_core::commons::valid_name(n))
            {
                return Ok(format!("belay: refused. `{n}` is not a card name: use lowercase letters, digits, `-`, `_`, `.` (up to 40), and not `user` or `repo`."));
            }
            if let Some(t) = join
                .iter()
                .flatten()
                .find(|t| !belay_core::commons::valid_name(t))
            {
                return Ok(format!(
                    "belay: refused. `{t}` is not a topic name: use lowercase letters, digits, `-`, `_`, `.`."
                ));
            }
            if handoff.as_deref().is_some_and(|h| h.trim().is_empty()) {
                return Ok("belay: refused. A handoff needs text.".into());
            }
            let me = lock(state).session_id.clone();
            if let Some(n) = &name {
                if let Some(id) = crate::commons::name_taken(&crate::paths::sessions_dir(), &me, n)
                {
                    return Ok(format!(
                        "belay: refused. The name `{n}` is taken by live session {id}."
                    ));
                }
            }
            let mut st = lock(state);
            let topics = (join.is_some() || leave.is_some()).then(|| {
                let mut t = st.controller.card(now_ms()).said.topics;
                t.extend(join.unwrap_or_default());
                let leave = leave.unwrap_or_default();
                t.retain(|x| !leave.contains(x));
                t
            });
            let ev = belay_core::Event::CardSet {
                name,
                task,
                note,
                offers,
                topics,
                handoff,
            };
            let d = st.controller.handle(ev, now_ms());
            st.store.persist(&d)?;
            let gap = st.controller.config().idle_gap_minutes * 60_000;
            Ok(crate::commons::render_card(
                &st.controller.card(now_ms()),
                now_ms(),
                gap,
            ))
        }
        _ => unreachable!("only commons requests"),
    }
}

/// This session's card and project, under the lock for a moment.
fn my_card_and_project(state: &Mutex<State>, now: u64) -> (belay_core::commons::Card, String) {
    let st = lock(state);
    let me = st.controller.card(now);
    let project = st
        .project
        .clone()
        .unwrap_or_else(|| crate::learnstore::project_of(Path::new(&me.top)));
    (me, project)
}

/// A commons message from this session (C2): resolve `to` against live cards, let the
/// controller decide and record it, then append it to each recipient's inbox, and
/// with `commons = on`, wake an idle fleet stream session (rate-limited by its hub).
fn send_commons(
    state: &Mutex<State>,
    to: &str,
    kind: Option<String>,
    text: &str,
) -> Result<String> {
    let now = now_ms();
    let (me, on, gap) = {
        let st = lock(state);
        let cfg = st.controller.config();
        (
            st.controller.card(now),
            cfg.commons,
            cfg.idle_gap_minutes * 60_000,
        )
    };
    let root = crate::paths::sessions_dir();
    let cards = crate::commons::read_cards(&root);
    let to_user = to == "user";
    let targets: Vec<&belay_core::commons::Card> = if to_user {
        Vec::new()
    } else {
        crate::commons::resolve(&cards, &me.id, &me.repo, to)
    };
    let names: Vec<String> = if to_user {
        vec!["user".into()]
    } else {
        targets.iter().map(|c| c.name.clone()).collect()
    };
    let verdict = {
        let mut st = lock(state);
        let (d, verdict) = st.controller.send(to, names, kind, text, now);
        st.store.persist(&d)?;
        verdict
    };
    let (kind, text) = match verdict {
        Ok(v) => v,
        Err(why) => return Ok(format!("belay: refused. {}", capitalize(&why))),
    };
    let line = crate::commons::message_line(&me.name, &me.id, &me.repo, &kind, &text, now);
    if to_user {
        crate::commons::append_message(&crate::commons::user_inbox(), &line)?;
        let a = attention::Alert {
            kind: attention::Kind::Message,
            text: format!("{kind} from {}: {text}", me.name),
            action: None,
        };
        alert(&mut lock(state), vec![a]);
        return Ok("Sent to the user (`belay inbox`).".into());
    }
    // Best effort: one inbox that cannot be written does not undo the others.
    let mut failed = Vec::new();
    for c in &targets {
        if crate::commons::append_message(&crate::commons::inbox_path(&root.join(&c.id)), &line)
            .is_err()
        {
            failed.push(c.name.clone());
            continue;
        }
        // An idle fleet stream session reads it only when a turn starts: ask its hub.
        // Not under `look`: the message would only be recorded, the turn wasted.
        if let (true, Some(fleet), Some(slot)) = (on, &c.fleet, &c.slot) {
            if c.status_at(now, gap) == belay_core::commons::CardStatus::Waiting {
                let _ = crate::hub::wake(fleet, slot, &me.name);
            }
        }
    }
    let mut said = crate::commons::sent_to(to, &targets);
    if !failed.is_empty() {
        said.push_str(&format!(
            " Not delivered to {}: its inbox could not be written.",
            failed.join(", ")
        ));
    }
    Ok(said)
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// Fixes other sessions learned since the last hook (F2).
fn refresh_fixes(st: &mut State) {
    if let Some(seen) = &mut st.fixes_seen {
        let new = crate::learnstore::new_fixes(seen, &st.session_id);
        if !new.is_empty() {
            st.controller.add_known_fixes(new);
        }
    }
}

/// How far an inbox was read, and a fingerprint of the file it was (a hash of its
/// first bytes): an inbox replaced by another file is read from its start, whatever
/// its length. `stamp` (size and mtime, not kept on disk) keeps the hook path at one
/// `stat` while nothing changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Seen {
    offset: u64,
    head: Option<String>,
    stamp: Option<(u64, i64, i64)>,
}

/// Bytes the fingerprint covers: the start of the first message.
const HEAD_BYTES: u64 = 64;

/// Where an earlier daemon of this slot stopped reading its inbox (a restart, a
/// rejoin): old messages are not delivered twice. `.seen` holds "<offset> <head>",
/// or the offset alone (older daemons).
fn inbox_seen(path: &Path) -> Seen {
    let text = fs::read_to_string(format!("{}.seen", path.display())).unwrap_or_default();
    let mut w = text.split_whitespace();
    Seen {
        offset: w.next().and_then(|x| x.parse().ok()).unwrap_or(0),
        head: w.next().map(String::from),
        stamp: None,
    }
}

/// The fingerprint of the file's first `n` bytes: what was read, up to `HEAD_BYTES`.
fn head_of(f: &mut fs::File, n: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut buf = Vec::new();
    f.seek(SeekFrom::Start(0)).ok()?;
    std::io::Read::by_ref(f)
        .take(n.min(HEAD_BYTES))
        .read_to_end(&mut buf)
        .ok()?;
    Some(belay_core::text::sha256_hex(&String::from_utf8_lossy(&buf))[..16].to_string())
}

/// Whole lines of the inbox past `seen`, and how far they go. The caller keeps it,
/// with `mark_inbox_seen`, once the lines are recorded.
fn new_inbox_lines(path: &Path, seen: &Seen) -> (Vec<serde_json::Value>, Seen) {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::MetadataExt;
    // On the hook path: a `stat` when nothing changed, and only the new bytes (plus the
    // first few) when something did.
    let Ok(mut f) = fs::File::open(path) else {
        return (Vec::new(), seen.clone());
    };
    let Ok(meta) = f.metadata() else {
        return (Vec::new(), seen.clone());
    };
    let stamp = Some((meta.len(), meta.mtime(), meta.mtime_nsec()));
    if seen.stamp.is_some() && seen.stamp == stamp {
        return (Vec::new(), seen.clone());
    }
    let len = meta.len();
    // Another file now, or shorter than what was read: it was rewritten. Read it again,
    // or nothing more would ever arrive.
    let replaced =
        seen.head.is_some() && seen.offset <= len && seen.head != head_of(&mut f, seen.offset);
    let offset = if replaced || seen.offset > len {
        0
    } else {
        seen.offset
    };
    let mut buf = Vec::new();
    if offset < len
        && (f.seek(SeekFrom::Start(offset)).is_err() || f.read_to_end(&mut buf).is_err())
    {
        return (Vec::new(), seen.clone());
    }
    let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    // Bytes, then text: one line that is not UTF-8 is skipped, not the rest.
    let lines = buf[..end]
        .split(|&b| b == b'\n')
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect();
    let offset = offset + end as u64;
    let head = (offset > 0).then(|| head_of(&mut f, offset)).flatten();
    (
        lines,
        Seen {
            offset,
            head,
            stamp,
        },
    )
}

/// Keep where this session stopped reading `path`, for the next daemon of the slot.
fn mark_inbox_seen(path: &Path, seen: &Seen) {
    let text = match &seen.head {
        Some(h) => format!("{} {h}", seen.offset),
        None => seen.offset.to_string(),
    };
    let _ = fs::write(format!("{}.seen", path.display()), text);
}

/// How often the fleet watcher looks at the other sessions (F1).
const FLEET_POLL: Duration = Duration::from_secs(1);

/// Fleet awareness (F1): once the session has started, record its workspace, then feed
/// the other live sessions' activity to the controller. Off the hook path: the lock is
/// held only to handle what one round found. Errors are logged and skipped.
fn watch_fleet(dir: PathBuf, state: &Mutex<State>) {
    let cwd = loop {
        thread::sleep(FLEET_POLL);
        if let Some(c) = state.lock().unwrap_or_else(|e| e.into_inner()).cwd.clone() {
            break c;
        }
    };
    let ws = crate::fleet::workspace(Path::new(&cwd));
    let mut watcher = crate::fleet::Watcher::new(dir, ws.clone());
    let known = state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .controller
        .has_workspace();
    let mut events = if known {
        Vec::new()
    } else {
        vec![belay_core::Event::Workspace {
            top: ws.top,
            common: ws.common,
        }]
    };
    let root = crate::paths::sessions_dir();
    loop {
        if !events.is_empty() {
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            for ev in events.drain(..) {
                let d = st.controller.handle(ev, now_ms());
                if let Err(e) = st.store.persist(&d) {
                    eprintln!("belayd: fleet: {e:#}");
                }
            }
        }
        thread::sleep(FLEET_POLL);
        events = watcher.poll(&root, now_ms());
    }
}

/// Plugin mode has no launcher to watch: exit after a long quiet spell. A later hook
/// starts a new daemon for the same session if the agent comes back.
fn watch_idle(minutes: u64, sock: &Path, state: &Mutex<State>) {
    loop {
        thread::sleep(Duration::from_secs(30));
        let last = LAST_REQUEST_MS.load(std::sync::atomic::Ordering::Relaxed);
        if now_ms().saturating_sub(last) > minutes * 60_000 {
            eprintln!("belayd: idle for {minutes} min; exiting");
            // No request may be writing the ledger when the process ends.
            let done = lock(state);
            keep_handoff(&done);
            card_ended();
            let _ = fs::remove_file(sock);
            std::process::exit(0);
        }
    }
}

/// Exit when the launcher is gone, so no daemon outlives its session.
fn watch_parent(pid: i32, sock: &Path, state: &Mutex<State>) {
    loop {
        thread::sleep(Duration::from_secs(2));
        // SAFETY: kill with signal 0 only checks that the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
        if !alive {
            eprintln!("belayd: launcher {pid} is gone; exiting");
            let done = lock(state);
            keep_handoff(&done);
            card_ended();
            let _ = fs::remove_file(sock);
            std::process::exit(0);
        }
    }
}

/// What one hook call decides, and the response to send. Time comes from `clock`, so the
/// late paths can be tested (I5, T3). D9: a reply after the shim's deadline reaches no
/// one. Late before handling: nothing is applied. Late after: the decisions are recorded
/// as not delivered, and nothing is sent.
/// This session, as a hook's lookups need it.
struct Me<'a> {
    id: &'a str,
    /// Its directory, when a `/resume` continues an earlier session's conversation (U8).
    resume: Option<&'a Path>,
}

fn decide_hook(
    controller: &mut Controller,
    connector: &mut dyn Connector,
    event: &str,
    payload: &serde_json::Value,
    deadline_ms: Option<u64>,
    clock: &mut dyn FnMut() -> u64,
    me: Me,
) -> (Vec<belay_core::Decision>, Response) {
    let Me {
        id: session_id,
        resume,
    } = me;
    let late = |t: u64| deadline_ms.is_some_and(|d| t + REPLY_MARGIN_MS > d);
    let now = clock();
    let unanswered = late(now);
    let mut decisions = Vec::new();
    let mut workspace_now = None;
    for ev in connector.map(event, payload) {
        // The repo's policy, from the session's working directory (E7).
        if let belay_core::Event::SessionStarted { cwd, .. } = &ev {
            let (policy, warnings) = crate::config::load_policy(Path::new(cwd));
            for w in warnings {
                eprintln!("belayd: {w}");
            }
            controller.set_policy(policy.clone());
            connector.set_policy(&policy);
            // One git call for the workspace; the project (the main checkout) and the
            // root (this worktree) come from it, not from three more (rule 7).
            // The card needs it whatever the features say.
            let ws = Some(crate::fleet::workspace(Path::new(cwd)));
            let project = ws
                .as_ref()
                .map(
                    // Real paths, as `learnstore::project_of` gives them: the user's
                    // `belay` commands look the project up from a shell's getcwd.
                    |w| match w.common.trim_end_matches('/').strip_suffix("/.git") {
                        Some(main) => main.to_string(),
                        None => fs::canonicalize(&w.top)
                            .map_or_else(|_| w.top.clone(), |p| p.display().to_string()),
                    },
                )
                .unwrap_or_default();
            // The commons (C1): who this session is, and who else is here.
            if let Some(ws) = ws.as_ref() {
                let cards = crate::commons::read_cards(&crate::paths::sessions_dir());
                let repo = project.clone();
                // The card's repo and files need the workspace now, also with
                // `fleet = off` (no watcher), and before the first edit.
                workspace_now = Some(belay_core::Event::Workspace {
                    top: ws.top.clone(),
                    common: ws.common.clone(),
                });
                let slot = std::env::var("BELAY_FLEET_SLOT").ok();
                let name = crate::commons::default_name(&cards, session_id, &repo, slot.as_deref());
                let others: Vec<_> = crate::commons::live_others(&cards, session_id).collect();
                let here = others.iter().filter(|c| c.repo == repo).count();
                controller.set_peers(others.len(), here);
                let found = crate::findings::load_in(&crate::findings::dir())
                    .iter()
                    .filter(|f| f.project == project && f.session != session_id)
                    .count();
                controller.set_findings_here(found);
                controller.set_handoff(crate::commons::latest_handoff(
                    &cards,
                    session_id,
                    &project,
                    crate::paths::now_ms(),
                ));
                controller.set_identity(belay_core::commons::Identity {
                    id: session_id.to_string(),
                    default_name: name,
                    fleet: std::env::var("BELAY_FLEET").ok(),
                    slot,
                });
                // Memory (U4b): the last handoff here and what to recall, as data.
                if controller.config().memory {
                    controller
                        .set_memories(crate::memorystore::recall_candidates(&project, &cards));
                    // What the agent loads on its own (M1): not recalled twice.
                    controller.set_host_context(
                        connector
                            .native_context(Path::new(cwd))
                            .into_iter()
                            .map(connector::NativeSource::host_text)
                            .collect(),
                    );
                }
            }
            // Shared learning (F2): other sessions' fixes and this project's notes.
            if let Some(ws) = ws.as_ref().filter(|_| controller.config().memory) {
                controller.set_learning(
                    &project,
                    &ws.top,
                    crate::learnstore::load_fixes(session_id),
                    crate::learnstore::approved_notes(&project),
                );
            }
        }
        let resumed = matches!(&ev, belay_core::Event::ConversationChanged { reason, .. } if reason == "resume");
        decisions.push(controller.handle_with(ev, now, !unanswered));
        // `/resume` in the agent: a conversation an earlier session ran takes up its
        // state (U8). [Verify] conformance `resume_in_process`: SessionStart has
        // `source: "resume"` and the resumed conversation's session_id.
        if let (true, Some(own), Some(conv)) = (resumed, resume, connector.conversation()) {
            if let Some((from, runs)) = earlier_runs(&conv, own) {
                let all = crate::store::chain_entries(&runs);
                let blobs = |h: &str| crate::store::read_chain_blob(&runs, h);
                decisions.push(controller.restore_conversation(
                    &all,
                    &blobs,
                    &from,
                    Some(conv),
                    now,
                ));
            }
        }
        if let Some(ws) = workspace_now.take() {
            decisions.push(controller.handle(ws, now));
        }
    }
    let mut response = Response::default();
    for d in &decisions {
        response.merge(d.response.clone());
    }
    if !response.is_empty() {
        let t = clock();
        if late(t) {
            let d = controller.undelivered(&decisions, t);
            decisions.push(d);
            response = Response::default();
        }
    }
    (decisions, response)
}

/// A snapshot to take once the reply is sent: the session started, or a turn ended.
struct SnapshotDue {
    at: crate::undo::At,
    cwd: PathBuf,
    dir: PathBuf,
    cause: u64,
    floor: u64,
}

/// Whether these decisions start the session or end a turn in a known directory. Counted
/// as pending at once, before the reply, so a shutdown that follows waits for it.
fn snapshot_due(st: &State, decisions: &[belay_core::Decision]) -> Option<SnapshotDue> {
    // A hook without a cwd says nothing about where the files are.
    let cwd = st.cwd.as_ref().filter(|c| !c.is_empty())?;
    let (at, cause) = decisions
        .iter()
        .flat_map(|d| &d.entries)
        .find_map(|e| match &e.body {
            belay_core::Body::Event {
                event: belay_core::Event::TurnEnded,
            } => Some((crate::undo::At::TurnEnd, e.seq)),
            belay_core::Body::Event {
                event: belay_core::Event::SessionStarted { .. },
            } => Some((crate::undo::At::Start, e.seq)),
            _ => None,
        })?;
    SNAPSHOTS_PENDING.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    Some(SnapshotDue {
        at,
        cwd: PathBuf::from(cwd),
        dir: st.dir.clone(),
        cause,
        floor: st.snapshot_floor,
    })
}

/// Take and record a snapshot (U6.2). Git runs outside the state lock, after the hook
/// has its reply: the shim never waits for it (rule 7). Outside git, nothing happens.
fn take_snapshot(state: &Mutex<State>, due: SnapshotDue) {
    {
        let _one = SNAPSHOT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((id, commit, files)) = crate::undo::take(&due.dir, &due.cwd, due.at, due.floor)
        {
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            let d = st
                .controller
                .snapshot(&id, &commit, files, Some(due.cause), now_ms());
            if let Err(e) = st.store.persist(&d) {
                eprintln!("belayd: record snapshot {id}: {e:#}");
            }
        }
    }
    SNAPSHOTS_PENDING.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
}

/// The earlier runs of the conversation the agent's picker chose, at the first hook of a
/// `Resume::Picker` session, if it is a SessionStart that resumes one.
/// [Verify] conformance `resume_picker`: after the picker, SessionStart has
/// `source: "resume"` and the chosen conversation's session_id.
fn picked_runs(
    state: &Mutex<State>,
    event: &str,
    payload: &serde_json::Value,
) -> Option<(String, Vec<crate::store::Run>, String)> {
    let own = {
        let mut st = lock(state);
        if !std::mem::take(&mut st.picker) {
            return None;
        }
        st.dir.clone()
    };
    let field = |k: &str| payload.get(k).and_then(serde_json::Value::as_str);
    if event != "SessionStart" || field("source") != Some("resume") {
        return None;
    }
    let conv = field("session_id")?.to_string();
    let (from, runs) = earlier_runs(&conv, &own)?;
    Some((from, runs, conv))
}

fn reply_to(mut stream: UnixStream, reply: &Reply) -> Result<()> {
    let mut line = serde_json::to_string(reply)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    Ok(())
}

fn serve(
    stream: UnixStream,
    state: &Mutex<State>,
    sock: &Path,
    token: &Option<String>,
) -> Result<()> {
    // A client that connects and sends nothing must not pin a thread forever (F9).
    stream.set_read_timeout(Some(READ_TIMEOUT))?;
    // Nor may one that never reads its reply.
    stream.set_write_timeout(Some(READ_TIMEOUT))?;
    let mut line = String::new();
    BufReader::new(stream.try_clone()?)
        .take(MAX_REQUEST_BYTES)
        .read_line(&mut line)?;
    // A liveness probe (`is_live`) connects and closes: not an error, and not activity,
    // or peers' fleet watchers would keep a plugin daemon from its idle exit.
    if line.is_empty() {
        return Ok(());
    }
    LAST_REQUEST_MS.store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    let req: Request = serde_json::from_str(&line).context("bad request")?;
    let mut shutdown = false;
    let mut snapshot = None;

    // Control requests must show the session token (R0.8).
    let given = match &req {
        Request::Pause { token, .. }
        | Request::Allow { token, .. }
        | Request::Undone { token, .. }
        | Request::Shutdown { token } => Some(token),
        _ => None,
    };
    if let Some(given) = given {
        if token.is_some() && given != token {
            eprintln!("belayd: refused a control request without the session token");
            return reply_to(stream, &Reply {
                output: None,
                message: Some(
                    "belay: refused: this needs the session's control token, which this command could not show.".into(),
                ),
            });
        }
    }

    // Requests that must not wait behind other work (R0.3).
    let req = match req {
        // Liveness: answer without the lock, so a busy daemon is never taken for dead.
        Request::Ping => {
            return reply_to(
                stream,
                &Reply {
                    output: None,
                    message: Some("pong".into()),
                },
            );
        }
        // Commons requests read other sessions' cards and inboxes, and may call a hub:
        // that I/O runs outside the lock, which is taken only for the controller (C1-C3).
        r @ (Request::Send { .. }
        | Request::Share { .. }
        | Request::Ask { .. }
        | Request::Card { .. }) => {
            let message = commons_request(state, r)?;
            return reply_to(
                stream,
                &Reply {
                    output: None,
                    message: Some(message),
                },
            );
        }
        // The heavy part of a filter (hash, failures, summary) runs outside the lock,
        // so hooks from parallel tool calls are not delayed by a large output.
        Request::Filter {
            tool_use_id,
            output,
            exit_code,
        } => {
            let plan = {
                let st = state.lock().unwrap_or_else(|e| e.into_inner());
                st.controller
                    .filter_level(&tool_use_id)
                    .map(|level| (level, st.controller.config().clone()))
            };
            let message = match plan {
                None => None,
                Some((level, cfg)) => {
                    let prepared = belay_core::prepare(&output, level, &cfg);
                    let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                    let st = &mut *st;
                    refresh_fixes(st);
                    let (d, text) = st.controller.filter_prepared(
                        &tool_use_id,
                        output,
                        prepared,
                        exit_code,
                        now_ms(),
                    );
                    st.store.persist(&d)?;
                    // A wrapped run is where most passes are seen: keep what it taught.
                    if let Some(project) = &st.project {
                        crate::learnstore::save_fixes(
                            std::slice::from_ref(&d),
                            &st.session_id,
                            project,
                        );
                    }
                    text
                }
            };
            return reply_to(
                stream,
                &Reply {
                    output: None,
                    message,
                },
            );
        }
        other => other,
    };

    // A resume through the picker: the first SessionStart names the conversation. Its
    // earlier runs are read outside the lock; hooks that wait meanwhile fail open.
    let picked = match &req {
        Request::Hook { event, payload, .. } => picked_runs(state, event, payload),
        _ => None,
    };
    let reply = {
        let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
        let st = &mut *st;
        match req {
            Request::Hook {
                event,
                payload,
                deadline_ms,
            } => {
                st.store.capture(now_ms(), &event, &payload)?;
                if let Some((from, runs, conv)) = picked {
                    let seq = restore_runs(&mut st.controller, &runs);
                    st.snapshot_floor = st.snapshot_floor.max(next_turn(&runs));
                    let d = st
                        .controller
                        .resumed(&from, seq, Some(conv), true, now_ms());
                    st.store.persist(&d)?;
                }
                if event == "PreToolUse" {
                    refresh_claims(st);
                }
                read_inbox(st)?;
                refresh_fixes(st);
                let (decisions, response) = decide_hook(
                    &mut st.controller,
                    st.connector.as_mut(),
                    &event,
                    &payload,
                    deadline_ms,
                    &mut now_ms,
                    Me {
                        id: &st.session_id,
                        resume: st.resume.then_some(st.dir.as_path()),
                    },
                );
                for d in &decisions {
                    st.store.persist(d)?;
                }
                // `by-session/<conversation>` names this session: a later resume of the
                // conversation continues from here (U8), and the plugin finds it.
                if let Some(conv) = st.connector.conversation() {
                    if st.mapped.as_ref() != Some(&conv)
                        && crate::paths::map_conversation(&conv, &st.session_id).is_ok()
                    {
                        st.mapped = Some(conv);
                    }
                }
                if st.project.is_none() {
                    let started = decisions
                        .iter()
                        .flat_map(|d| &d.entries)
                        .find_map(|e| match &e.body {
                            belay_core::Body::Event {
                                event: belay_core::Event::SessionStarted { cwd, .. },
                            } => Some(cwd.clone()),
                            _ => None,
                        });
                    // The workspace the start found names the project; git only if not.
                    st.project = started.map(|cwd| {
                        if st.controller.has_workspace() {
                            st.controller.card(now_ms()).repo
                        } else {
                            crate::learnstore::project_of(Path::new(&cwd))
                        }
                    });
                }
                if let Some(project) = &st.project {
                    crate::learnstore::save_fixes(&decisions, &st.session_id, project);
                    // Read from the start once: what was added since the session's
                    // start (a few lines) is checked against what it loaded.
                    if st.fixes_seen.is_none() && st.controller.config().memory {
                        // From where the start's full read ended: only later fixes.
                        st.fixes_seen = Some(crate::learnstore::fixes_len());
                    }
                }
                remember(st, &decisions)?;
                let compacting = decisions.iter().flat_map(|d| &d.entries).any(|e| {
                    matches!(
                        e.body,
                        belay_core::Body::Event {
                            event: belay_core::Event::Compacted { .. }
                        }
                    )
                });
                if compacting {
                    keep_handoff(st);
                }
                snapshot = snapshot_due(st, &decisions);
                let alerts =
                    st.attention
                        .observe(&decisions, st.controller.verify_unmet(), now_ms());
                alert(st, alerts);
                Reply {
                    output: st.connector.encode(&event, &payload, &response),
                    message: None,
                }
            }
            Request::Pause {
                feature, resume, ..
            } => {
                let feature = match feature.as_deref() {
                    None => Ok(None),
                    Some(f) => belay_core::Feature::parse(f).map(Some).ok_or_else(|| {
                        let names: Vec<&str> =
                            belay_core::Feature::ALL.iter().map(|f| f.name()).collect();
                        format!("No feature `{f}`. Features: {}.", names.join(", "))
                    }),
                };
                let done = feature.and_then(|f| {
                    let r = if resume {
                        st.controller.resume(f, "user", now_ms())
                    } else {
                        st.controller.pause(f, "user", now_ms())
                    };
                    r.map(|d| (f, d))
                });
                let message = match done {
                    Ok((f, d)) => {
                        st.store.persist(&d)?;
                        let what = f.map_or("belay".to_string(), |f| f.name().to_string());
                        let paused: Vec<&str> =
                            st.controller.paused().iter().map(|f| f.name()).collect();
                        match (resume, paused.is_empty()) {
                            (false, _) => format!("Paused {what}. `belay resume` undoes it."),
                            (true, true) => format!("Resumed {what}. Nothing is paused."),
                            (true, false) => {
                                format!("Resumed {what}. Still paused: {}.", paused.join(", "))
                            }
                        }
                    }
                    Err(m) => m,
                };
                Reply {
                    output: None,
                    message: Some(message),
                }
            }
            Request::Allow { action, .. } => {
                let message = match st.controller.allow(action.as_deref(), now_ms()) {
                    Ok(d) => {
                        st.store.persist(&d)?;
                        // A rule allowed twice drops to warn (U6.3): the user hears it here.
                        let mut m = "Allowed once. The agent can retry the edit now.".to_string();
                        for n in &d.response.notices {
                            m.push_str(&format!("\nbelay: {}", n.text));
                        }
                        m
                    }
                    Err(m) => m,
                };
                Reply {
                    output: None,
                    message: Some(message),
                }
            }
            Request::Undone {
                id, before, files, ..
            } => {
                let d = st.controller.undone(&id, &before, files, now_ms());
                st.store.persist(&d)?;
                Reply {
                    output: None,
                    message: Some("recorded".into()),
                }
            }
            Request::Sandbox { event } => {
                let message = match event {
                    belay_core::Event::SandboxStarted { .. }
                    | belay_core::Event::SandboxStopped { .. } => {
                        let d = st.controller.handle(event, now_ms());
                        st.store.persist(&d)?;
                        "recorded"
                    }
                    _ => "belay: refused: only sandbox events",
                };
                Reply {
                    output: None,
                    message: Some(message.into()),
                }
            }
            Request::Ping
            | Request::Filter { .. }
            | Request::Send { .. }
            | Request::Share { .. }
            | Request::Ask { .. }
            | Request::Card { .. } => unreachable!("answered above"),
            Request::Shutdown { .. } => {
                shutdown = true;
                Reply {
                    output: None,
                    message: Some("bye".into()),
                }
            }
        }
    };

    let sent = reply_to(stream.try_clone()?, &reply);
    if let Some(due) = snapshot {
        take_snapshot(state, due);
    }
    sent?;
    if shutdown {
        // The last turn's snapshot first: the agent has exited, its files stay still.
        let wait = std::time::Instant::now();
        while SNAPSHOTS_PENDING.load(std::sync::atomic::Ordering::SeqCst) > 0
            && wait.elapsed() < Duration::from_secs(30)
        {
            thread::sleep(Duration::from_millis(20));
        }
        // Wait for any request that is writing the ledger, then exit (R0.6).
        let done = state.lock().unwrap_or_else(|e| e.into_inner());
        keep_handoff(&done);
        card_ended();
        let _ = fs::remove_file(sock);
        std::process::exit(0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read and keep the offset, as `read_inbox` does once the lines are recorded.
    fn take(path: &Path, offset: &mut Seen) -> Vec<serde_json::Value> {
        let (lines, next) = new_inbox_lines(path, offset);
        mark_inbox_seen(path, &next);
        *offset = next;
        lines
    }

    #[test]
    fn an_inbox_replaced_by_a_longer_file_is_read_from_its_start() {
        let d = std::env::temp_dir().join(format!("belay-inbox-ino-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let inbox = d.join("inbox.jsonl");
        fs::write(&inbox, "{\"text\":\"a\"}\n").unwrap();
        let mut seen = inbox_seen(&inbox);
        assert_eq!(take(&inbox, &mut seen).len(), 1);
        // A new file, longer than what was read: before, its first line was skipped.
        fs::remove_file(&inbox).unwrap();
        fs::write(&inbox, "{\"text\":\"bbbbbbbbbbbb\"}\n{\"text\":\"c\"}\n").unwrap();
        let mut seen = inbox_seen(&inbox);
        let got = take(&inbox, &mut seen);
        assert_eq!(got.len(), 2, "{got:?}");
        // An older daemon's `.seen` (an offset alone) still works.
        fs::write(format!("{}.seen", inbox.display()), "17").unwrap();
        assert_eq!(
            inbox_seen(&inbox),
            Seen {
                offset: 17,
                ..Seen::default()
            }
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn inbox_lines_read_but_not_recorded_come_again_after_a_restart() {
        let d = std::env::temp_dir().join(format!("belay-inbox-unrec-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let inbox = d.join("inbox.jsonl");
        fs::write(&inbox, "{\"text\":\"one\"}\n").unwrap();
        let (got, next) = new_inbox_lines(&inbox, &inbox_seen(&inbox));
        assert_eq!((got.len(), next.offset), (1, 15));
        // The ledger write failed: nothing marked, so a new daemon reads it again.
        assert_eq!(new_inbox_lines(&inbox, &inbox_seen(&inbox)).0.len(), 1);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_new_daemon_does_not_deliver_old_inbox_messages_again() {
        let d = std::env::temp_dir().join(format!("belay-inbox-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let inbox = d.join("inbox.jsonl");
        fs::write(&inbox, "{\"text\":\"one\"}\n{\"text\":\"half").unwrap();
        let mut offset = inbox_seen(&inbox);
        assert_eq!(take(&inbox, &mut offset).len(), 1);
        fs::write(&inbox, "{\"text\":\"one\"}\n{\"text\":\"two\"}\n").unwrap();
        // A restart: the offset comes from the file, not from 0.
        let mut offset = inbox_seen(&inbox);
        let got = take(&inbox, &mut offset);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["text"], "two");
        let _ = fs::remove_dir_all(&d);
    }
    use crate::connector::claude_code::ClaudeCode;
    use belay_core::{Body, Capabilities, Config};

    fn weakening_edit() -> serde_json::Value {
        let p = format!(
            "{}/../../fixtures/claude_code/pre_tool_use_edit_test.json",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
    }

    /// Run one guard-deny hook with a clock that returns `times` in turn.
    fn decide(deadline: Option<u64>, times: &[u64]) -> (Vec<belay_core::Decision>, Response) {
        // Not the machine's live sessions, fixes or notes: only the deadline is tested.
        let cfg = Config {
            commons: false,
            routine: false,
            memory: false,
            ..Config::default()
        };
        let mut c = Controller::new(cfg, Capabilities::CLAUDE_CODE);
        let mut conn = ClaudeCode::default();
        let mut t = times.iter().copied();
        let mut clock = move || t.next().unwrap_or(u64::MAX / 2);
        decide_hook(
            &mut c,
            &mut conn,
            "PreToolUse",
            &weakening_edit(),
            deadline,
            &mut clock,
            Me {
                id: "test",
                resume: None,
            },
        )
    }

    fn applied(ds: &[belay_core::Decision]) -> Vec<bool> {
        ds.iter()
            .flat_map(|d| &d.entries)
            .filter_map(|e| match &e.body {
                Body::Action { applied, .. } => Some(*applied),
                _ => None,
            })
            .collect()
    }

    fn undelivered(ds: &[belay_core::Decision]) -> bool {
        ds.iter()
            .flat_map(|d| &d.entries)
            .any(|e| matches!(e.body, Body::Undelivered { .. }))
    }

    #[test]
    fn blocked_edits_and_loops_are_worth_a_notification() {
        let notifications = |ds: &[belay_core::Decision]| {
            attention::Attention::new(crate::config::Notify::Attention, None).observe(ds, false, 0)
        };
        let (ds, _) = decide(Some(1_000), &[100, 200]);
        let n = notifications(&ds);
        assert_eq!(n.len(), 1);
        assert!(n[0].text.contains("belay allow"), "{n:?}");
        // Late: nothing was blocked, nothing to say.
        let (ds, _) = decide(Some(1_000), &[2_000]);
        assert!(notifications(&ds).is_empty());
    }

    #[test]
    fn on_time_reply_is_sent_and_applied() {
        let (ds, r) = decide(Some(1_000), &[100, 200]);
        assert!(r.permission.is_some());
        assert_eq!(applied(&ds), [true]);
        assert!(!undelivered(&ds));
    }

    #[test]
    fn late_before_handling_applies_nothing() {
        let (ds, r) = decide(Some(1_000), &[2_000]);
        assert!(r.is_empty());
        assert!(applied(&ds).iter().all(|a| !a));
    }

    /// Within the reply margin counts as late: the reply could not get out in time.
    #[test]
    fn inside_the_margin_is_late() {
        let (_, r) = decide(Some(1_000), &[1_000 - REPLY_MARGIN_MS + 1]);
        assert!(r.is_empty());
        let (_, r) = decide(Some(1_000), &[1_000 - REPLY_MARGIN_MS - 1, 0]);
        assert!(r.permission.is_some());
    }

    #[test]
    fn late_after_handling_is_recorded_as_undelivered() {
        let (ds, r) = decide(Some(1_000), &[100, 5_000]);
        assert!(r.is_empty(), "nothing is sent");
        assert!(undelivered(&ds));
    }

    #[test]
    fn no_deadline_is_never_late() {
        let (ds, r) = decide(None, &[u64::MAX / 4, u64::MAX / 4]);
        assert!(r.permission.is_some());
        assert!(!undelivered(&ds));
    }

    #[test]
    fn an_inbox_that_got_shorter_is_read_again() {
        let d = std::env::temp_dir().join(format!("belay-inbox-short-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let inbox = d.join("inbox.jsonl");
        fs::write(&inbox, "{\"text\":\"a long first message\"}\n").unwrap();
        let mut offset = Seen::default();
        assert_eq!(take(&inbox, &mut offset).len(), 1);
        fs::write(&inbox, "{\"text\":\"b\"}\n").unwrap();
        assert_eq!(take(&inbox, &mut offset).len(), 1, "not stalled");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_line_that_is_not_utf8_does_not_stall_the_inbox() {
        let d = std::env::temp_dir().join(format!("belay-inbox-utf8-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let inbox = d.join("inbox.jsonl");
        let mut b = b"{\"text\":\"a\"}\n\xff\xfe\n".to_vec();
        b.extend_from_slice(b"{\"text\":\"b\"}\n");
        fs::write(&inbox, b).unwrap();
        let mut offset = Seen::default();
        assert_eq!(take(&inbox, &mut offset).len(), 2);
        assert!(take(&inbox, &mut offset).is_empty(), "nothing new");
        let _ = fs::remove_dir_all(&d);
    }
}
