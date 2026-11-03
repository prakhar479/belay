//! The client side: talking to a fleet's hub, and the read-only views of fleets, tasks and the inbox.

use super::*;

// --- The client side ---------------------------------------------------------------

/// How long a client waits for the hub. It serves one request at a time and moves
/// work with git; on a loaded machine (four workers running `make check`) 10 s was
/// not enough, and a test read "busy" as a failure (dogfood fleet, 2026-10-22).
pub(super) const REPLY_WAIT: Duration = Duration::from_secs(30);

/// Send a request to a fleet's hub; its reply message.
pub fn send(dir: &Path, req: &HubRequest) -> Result<String> {
    let mut stream = crate::ipc::connect_now(&socket_path(dir))?;
    stream.set_read_timeout(Some(REPLY_WAIT))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut out = String::new();
    if let Err(e) = BufReader::new(stream).read_line(&mut out) {
        if matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            // Not "down": the hub got it and is busy. Repeating it could do it twice.
            return Ok("belay: the hub is busy and did not answer in 30 s; your request may still apply. Check with `belay task list` or `belay fleet status` before you repeat it.".into());
        }
        return Err(e.into());
    }
    let r: Reply = serde_json::from_str(&out)?;
    Ok(r.message.unwrap_or_default())
}

/// `send` for the CLI: "not running" only when no hub listens. Any other error (a hub
/// that died mid-request, a reply it could not write) says what happened.
pub(super) fn send_cli(dir: &Path, req: &HubRequest) -> Result<String> {
    send(dir, req).map_err(
        |e| match e.downcast_ref::<std::io::Error>().map(|io| io.kind()) {
            Some(std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => {
                anyhow!("the fleet's hub is not running")
            }
            Some(std::io::ErrorKind::WouldBlock) => {
                anyhow!("the fleet's hub is too busy to take a request; try again in a moment")
            }
            _ => anyhow!("the fleet's hub did not answer: {e}"),
        },
    )
}

pub(super) fn is_up(dir: &Path) -> bool {
    send(dir, &HubRequest::Ping).is_ok()
}

pub(super) fn fleet_dirs() -> Vec<PathBuf> {
    let mut d: Vec<PathBuf> = fs::read_dir(fleets_dir())
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("meta.json").exists())
        .collect();
    d.sort();
    d
}

/// The fleet dir named by `s`: its id, or a unique prefix of one.
pub(super) fn pick(dirs: &[PathBuf], s: &str) -> Result<PathBuf> {
    let hits: Vec<&PathBuf> = dirs
        .iter()
        .filter(|d| {
            d.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(s))
        })
        .collect();
    // An id that is also the start of a longer one names itself.
    if let Some(exact) = hits.iter().find(|d| d.file_name().is_some_and(|n| n == s)) {
        return Ok((*exact).clone());
    }
    match hits[..] {
        [one] => Ok(one.clone()),
        [] => bail!("no fleet `{s}` (`belay fleet status` lists them)"),
        _ => bail!("`{s}` matches {} fleets; give more of the id", hits.len()),
    }
}

/// A fleet by id or unique prefix; else the newest one that is up; else the newest.
pub fn resolve(spec: Option<&str>) -> Result<PathBuf> {
    let dirs = fleet_dirs();
    if let Some(s) = spec {
        return pick(&dirs, s);
    }
    if let Some(d) = dirs.iter().rev().find(|d| is_up(d)) {
        return Ok(d.clone());
    }
    dirs.last()
        .cloned()
        .ok_or_else(|| anyhow!("no fleets yet; start one with `belay fleet up`"))
}

pub(super) fn read_meta(dir: &Path) -> Result<Meta> {
    Ok(serde_json::from_str(&fs::read_to_string(
        dir.join("meta.json"),
    )?)?)
}

/// The fleet's records, from its ledger.
pub fn read_ledger(dir: &Path) -> Vec<FleetRecord> {
    let Ok(bytes) = fs::read(dir.join("ledger.jsonl")) else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The model a fleet's ledger describes: its events, replayed.
pub(super) fn replay(meta: &Meta, records: &[FleetRecord]) -> Model {
    let mut m = Model::new(meta.spec.clone());
    for r in records {
        if let FleetBody::Event { event } = &r.body {
            m.step(event.clone(), r.ts_ms);
        }
    }
    m
}

/// `belay fleet status`: the hub's view when it is up, else the ledger's.
pub fn status(spec: Option<&str>) -> Result<String> {
    let dir = resolve(spec)?;
    if let Ok(text) = send(&dir, &HubRequest::Status) {
        return Ok(text);
    }
    let meta = read_meta(&dir)?;
    let model = replay(&meta, &read_ledger(&dir));
    Ok(render_status(&meta, &model, &BTreeMap::new(), false))
}

pub(super) fn render_status(
    meta: &Meta,
    model: &Model,
    procs: &BTreeMap<String, Proc>,
    up: bool,
) -> String {
    let mut out = String::new();
    let preset = serde_json::to_value(meta.spec.preset)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "fleet {} · {} · {preset}{} · {}",
        meta.id,
        if up { "up" } else { "hub not running" },
        if meta.mode.as_deref() == Some("look") {
            " · recorded in look mode (0.1)"
        } else {
            ""
        },
        meta.repo.as_deref().unwrap_or(&meta.cwd)
    );
    let width = model.slots().keys().map(|k| k.len()).max().unwrap_or(4);
    for (id, s) in model.slots() {
        let state = format!("{:?}", s.state).to_lowercase();
        let mut facts = Vec::new();
        if let Some(p) = procs.get(id) {
            facts.push(format!("pid {}", p.pid));
            facts.push(format!(
                "{} turn{}",
                p.turns,
                if p.turns == 1 { "" } else { "s" }
            ));
            facts.push(p.worktree.display().to_string());
        }
        if s.tokens > 0 {
            facts.push(format!("{} tokens", crate::report::k(s.tokens)));
        }
        if s.restarts > 0 {
            facts.push(format!("restarts {}", s.restarts));
        }
        if s.state == SlotState::Waiting {
            if let Some(at) = s.retry_at {
                facts.push(format!(
                    "restart in {} s",
                    at.saturating_sub(now_ms()) / 1000
                ));
            }
        }
        let _ = writeln!(out, "  {id:<width$}  {state:<8}  {}", facts.join(" · "));
    }
    if model.slots().is_empty() {
        let _ = writeln!(out, "  no sessions");
    }
    let (tokens, cost) = model.spend();
    let _ = writeln!(
        out,
        "Spent: {} tokens · {}{}",
        crate::report::k(tokens),
        dollars(cost),
        match (model.paused(), model.resume_at()) {
            (_, Some(at)) => format!(
                " · paused until {} (usage limit)",
                belay_core::fleet::model::clock_utc(at)
            ),
            (true, None) if model.limit_paused() =>
                " · paused (usage limit; `belay fleet resume`)".to_string(),
            (true, None) => " · paused (`belay fleet resume`)".to_string(),
            (false, _) => String::new(),
        }
    );
    out
}

/// A cost in dollars; under a cent is not shown as $0.00.
pub(super) fn dollars(cost: f64) -> String {
    if cost > 0.0 && cost < 0.01 {
        "<$0.01".into()
    } else {
        format!("${cost:.2}")
    }
}

/// The task board, in the order tasks were added.
pub(super) fn render_tasks(model: &Model) -> String {
    let mut out = String::new();
    let tasks: Vec<_> = model.tasks().collect();
    if tasks.is_empty() {
        return out;
    }
    let _ = writeln!(out, "Tasks:");
    let width = tasks.iter().map(|t| t.id.len()).max().unwrap_or(3);
    for t in tasks {
        let state = format!("{:?}", t.state).to_lowercase();
        // One line per task: a title can be a whole pasted brief.
        let mut facts = vec![one_line(&t.title, 100)];
        if let Some(s) = &t.slot {
            facts.push(format!("on {s}"));
        } else if let Some(x) = &t.target {
            facts.push(format!("for {x}"));
        }
        if !t.depends_on.is_empty() {
            facts.push(format!("after {}", t.depends_on.join(", ")));
        }
        if t.attempts > 0 {
            facts.push(format!(
                "{} failed attempt{}",
                t.attempts,
                if t.attempts == 1 { "" } else { "s" }
            ));
        }
        if let Some(o) = &t.outcome {
            facts.push(format!("\"{}\"", one_line(o, 80)));
        }
        let _ = writeln!(out, "  {:<width$}  {state:<8}  {}", t.id, facts.join(" · "));
    }
    out
}

/// The first line of `text`, at most `max` characters; `…` when anything was left out.
pub(super) fn one_line(text: &str, max: usize) -> String {
    let first = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let cut = belay_core::text::truncate_chars(first, max);
    if cut.len() < text.trim_end().len() && !cut.ends_with('…') {
        format!("{cut}…")
    } else {
        cut
    }
}

/// `belay task show`: one task in full.
pub(super) fn render_task(model: &Model, id: &str) -> String {
    let Some(t) = model.tasks().find(|t| t.id == id) else {
        return format!("belay: refused. There is no task {id}.");
    };
    let mut out = format!(
        "{} · {}\n{}\n",
        t.id,
        format!("{:?}", t.state).to_lowercase(),
        t.title
    );
    if !t.body.is_empty() {
        let _ = writeln!(out, "\n{}", t.body);
    }
    let mut facts = Vec::new();
    if let Some(s) = &t.slot {
        facts.push(format!("on {s}"));
    }
    if !t.depends_on.is_empty() {
        facts.push(format!("after {}", t.depends_on.join(", ")));
    }
    if !t.scope.is_empty() {
        facts.push(format!("scope {}", t.scope.join(", ")));
    }
    if let Some(c) = &t.check {
        facts.push(format!("check `{c}`"));
    }
    if !facts.is_empty() {
        let _ = writeln!(out, "\n{}", facts.join(" · "));
    }
    if let Some(o) = &t.outcome {
        let _ = writeln!(out, "\nOutcome: {o}");
    }
    out
}

/// The last 20 lines of an inbox file, oldest first.
pub(super) fn render_inbox(path: &Path) -> String {
    let Ok(bytes) = fs::read(path) else {
        return "No messages.".into();
    };
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::new();
    for l in &lines[lines.len().saturating_sub(20)..] {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(l) else {
            continue;
        };
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let head = match (s("kind"), s("from")) {
            (k, f) if !f.is_empty() => format!("{k} from {f}: "),
            _ => String::new(),
        };
        let _ = writeln!(out, "- {head}{}", s("text"));
    }
    if out.is_empty() {
        out.push_str("No messages.");
    }
    out
}

/// `belay msg` and `belay inbox`, like `belay task`.
pub fn msg(fleet: Option<&str>, to: String, kind: Option<String>, text: String) -> Result<String> {
    let env_fleet = std::env::var("BELAY_FLEET").ok();
    let dir = resolve(fleet.or(env_fleet.as_deref()))?;
    let req = HubRequest::Msg {
        to,
        kind,
        text,
        claimed_slot: std::env::var("BELAY_FLEET_SLOT").ok(),
    };
    send_cli(&dir, &req)
}

/// The last 20 messages of an inbox file (commons or fleet), oldest first.
pub fn render_inbox_file(path: &Path) -> String {
    render_inbox(path)
}

/// Ask fleet `fleet`'s hub to wake `slot` for a commons message from `from` (C2).
pub fn wake(fleet: &str, slot: &str, from: &str) -> Result<String> {
    let dir = resolve(Some(fleet))?;
    send(
        &dir,
        &HubRequest::Wake {
            slot: slot.to_string(),
            from: from.to_string(),
        },
    )
}

pub fn show_inbox(fleet: Option<&str>) -> Result<String> {
    let env_fleet = std::env::var("BELAY_FLEET").ok();
    let dir = resolve(fleet.or(env_fleet.as_deref()))?;
    // The user reads the fleet's inbox: mark it read (G6).
    if std::env::var_os("BELAY_FLEET_SLOT").is_none() {
        let _ = fs::write(
            dir.join("inbox.read"),
            line_count(&dir.join("inbox.jsonl")).to_string(),
        );
    }
    let req = HubRequest::Inbox {
        claimed_slot: std::env::var("BELAY_FLEET_SLOT").ok(),
    };
    match send(&dir, &req) {
        Ok(t) => Ok(t),
        // The hub is down: the user can still read the fleet's inbox.
        Err(_) => Ok(render_inbox(&dir.join("inbox.jsonl"))),
    }
}

/// Join a fleet from a terminal (`belay claude --belay-fleet`): the slot and its rules.
pub fn join(
    fleet: Option<&str>,
    role: Option<String>,
    name: Option<String>,
    pid: i32,
) -> Result<(String, String, String)> {
    let dir = resolve(fleet)?;
    let id = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let cwd = std::env::current_dir()?.display().to_string();
    let reply = send_cli(
        &dir,
        &HubRequest::Join {
            role,
            name,
            pid,
            cwd,
        },
    )?;
    let v: serde_json::Value = serde_json::from_str(&reply)
        .map_err(|_| anyhow!("{}", reply.trim_start_matches("belay: ")))?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    Ok((id, s("slot"), s("rules")))
}

pub fn leave(fleet: &str, slot: &str) {
    if let Ok(dir) = resolve(Some(fleet)) {
        let _ = send(&dir, &HubRequest::Leave { slot: slot.into() });
    }
}

/// `belay task …`: the fleet from `BELAY_FLEET` (a fleet session), `--fleet`, or the
/// newest that is up.
pub fn task(fleet: Option<&str>, verb: TaskVerb) -> Result<String> {
    let env_fleet = std::env::var("BELAY_FLEET").ok();
    let dir = resolve(fleet.or(env_fleet.as_deref()))?;
    let req = HubRequest::Task {
        verb,
        claimed_slot: std::env::var("BELAY_FLEET_SLOT").ok(),
    };
    send_cli(&dir, &req)
}

/// `belay fleet` (no subcommand) lists fleets too; one line each.
pub fn list(all: bool) -> String {
    list_dirs(&fleet_dirs(), all)
}

/// belay's disk use, for `belay fleet`: one line, and a warning past 10 GB.
pub fn disk_use() -> String {
    disk_line(&crate::disk::usage(&paths::home()))
}

pub(super) fn disk_line(u: &crate::disk::Usage) -> String {
    if u.over() {
        format!(
            "warn: {} Over {}: `belay fleet prune --yes` removes finished worktrees; `belay doctor` says more.\n",
            u.line(),
            crate::disk::human(crate::disk::WARN_BYTES)
        )
    } else {
        format!("{}\n", u.line())
    }
}

/// A fleet that is down, with no live session and no worktree left (pruned), is not
/// listed unless `all`: it has nothing left to look at.
pub(super) fn list_dirs(dirs: &[PathBuf], all: bool) -> String {
    let mut out = String::new();
    for d in dirs {
        let d = d.as_path();
        let Ok(meta) = read_meta(d) else { continue };
        let records = read_ledger(d);
        let model = replay(&meta, &records);
        let live = model
            .slots()
            .values()
            .filter(|s| matches!(s.state, SlotState::Starting | SlotState::Running))
            .count();
        let up = is_up(d);
        let worktrees = fs::read_dir(d.join("worktrees"))
            .into_iter()
            .flatten()
            .any(|e| e.is_ok());
        if !all && !up && live == 0 && !worktrees {
            continue;
        }
        let unread = unread_for_user(d);
        let _ = writeln!(
            out,
            "fleet {} · {} · {} sessions running{} · {}",
            meta.id,
            if up { "up" } else { "down" },
            live,
            if unread > 0 {
                format!(" · {unread} unread (`belay inbox`)")
            } else {
                String::new()
            },
            meta.repo.as_deref().unwrap_or(&meta.cwd)
        );
    }
    out
}

pub(super) fn line_count(path: &Path) -> usize {
    fs::read(path).map_or(0, |b| String::from_utf8_lossy(&b).lines().count())
}

/// The user's fleet messages since `belay inbox` last showed them (G6).
pub(super) fn unread_for_user(dir: &Path) -> usize {
    let read: usize = fs::read_to_string(dir.join("inbox.read"))
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0);
    let total = line_count(&dir.join("inbox.jsonl"));
    // A count past the end means the inbox got shorter: nothing of it was read.
    let read = if read > total { 0 } else { read };
    total - read
}

/// Messages a fleet session's inbox holds that its agent has not seen yet: the lines
/// beyond the messages its ledger recorded (G6). For `belay status`.
pub fn unread_for_session(inbox: &Path, entries: &[belay_core::Entry]) -> usize {
    let seen = entries
        .iter()
        .filter(|e| {
            matches!(
                &e.body,
                belay_core::Body::Event {
                    event: belay_core::Event::Message { channel: None, .. }
                }
            )
        })
        .count();
    // Commons messages (C2) come from another inbox: they are not counted here.
    line_count(inbox).saturating_sub(seen)
}
