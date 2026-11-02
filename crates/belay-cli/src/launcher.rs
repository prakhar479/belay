//! `belay claude` (`belay launch claude-code`): start the daemon, then run Claude Code with belay hooks.
//! If anything of belay's own fails (setup, daemon), run Claude Code without belay
//! (fail open, rule 1).

use std::env;
use std::ffi::OsString;
use std::fs;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::connector::claude_code;
use crate::ipc::{self, Request};
use crate::paths;

/// What `belay claude` set up for a session.
struct Session {
    id: String,
    dir: PathBuf,
    sock: PathBuf,
    token: String,
    settings: PathBuf,
    daemon: Child,
}

/// `belay launch <agent>`: the agent's connector says whether belay can run it.
/// belay's own flags (`--belay-fleet`, …) come in `args`.
pub fn launch(agent: &str, args: Vec<String>) -> Result<i32> {
    let connector = crate::connector::for_agent(agent)?;
    match connector.agent() {
        "claude-code" => run_claude(args),
        other => bail!("belay cannot launch `{other}` yet"),
    }
}

pub fn run_claude(args: Vec<String>) -> Result<i32> {
    let args = drop_mode(args);
    let (join, args) = take_fleet(args);
    let (fresh, args) = take_fresh(args);
    let bin = env::var("BELAY_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    // A missing agent is the user's to fix; set nothing up for it.
    if find_program(&bin).is_none() {
        bail!("`{bin}` not found on PATH (set BELAY_CLAUDE_BIN to use another binary)");
    }
    // Join a fleet from this terminal (F4): the daemon and the agent run as its slot.
    let joined = match join {
        Some((fleet, role, name)) => {
            let (id, slot, rules) =
                crate::hub::join(fleet.as_deref(), role, name, std::process::id() as i32)?;
            // SAFETY: no other thread runs yet; the daemon and the agent inherit these.
            unsafe {
                env::set_var("BELAY_FLEET", &id);
                env::set_var("BELAY_FLEET_SLOT", &slot);
                env::set_var("BELAY_FLEET_RULES", &rules);
            }
            eprintln!("belay: joined fleet {id} as {slot}. `belay msg`, `belay inbox`, and `belay task` work in this session.");
            Some((id, slot))
        }
        None => None,
    };
    let (user_settings, args) = take_settings(args);
    let exe = env::current_exe().context("find belay executable")?;
    // In parallel with the daemon's start: is this Claude Code version checked?
    let version_check = {
        let bin = bin.clone();
        thread::spawn(move || unchecked_version(&bin))
    };

    let resume = if fresh || !crate::config::load().resume_continues() {
        crate::daemon::Resume::No
    } else {
        resume_of(&args)
    };
    let session = match setup(&exe, user_settings.as_deref(), &resume) {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("belay: {e:#}; running {bin} without belay");
            None
        }
    };

    if let Ok(Some(v)) = version_check.join() {
        eprintln!(
            "belay: Claude Code {v} has not been checked with this version of belay (scripts/conformance.py). If it changed a hook, belay fails open and records less."
        );
        mark_version_seen(&v);
    }

    forward_signals();
    let mut cmd = Command::new(&bin);
    match &session {
        Some(s) => {
            cmd.arg("--settings")
                .arg(&s.settings)
                .env("BELAY_SOCKET", &s.sock)
                .env("BELAY_SESSION_DIR", &s.dir)
                .env("PATH", agent_path(&exe, &s.dir));
        }
        // belay could not take the user's --settings over: pass it on unchanged.
        None => {
            if let Some(v) = &user_settings {
                cmd.arg("--settings").arg(v);
            }
        }
    }
    let status = run_child(cmd.args(&args));
    // Clean up even when the agent did not start: no daemon or fleet slot left behind.

    if let Some(mut s) = session {
        finish(&mut s);
    }
    if let Some((id, slot)) = joined {
        crate::hub::leave(&id, &slot);
    }
    Ok(exit_code(status.with_context(|| format!("start `{bin}`"))?))
}

/// An agent session whose agent runs in a sandbox (`belay sandbox claude|task`). The
/// daemon runs here, outside, where the agent cannot signal it; the agent's hooks reach
/// it through the socket, bound into the sandbox.
pub struct Sandboxed {
    session: Session,
    /// The agent's command line: its program (a real path, so it can be bound in),
    /// belay's arguments, then the caller's.
    pub argv: Vec<String>,
    /// Set inside, over the sandbox's own environment.
    pub env: Vec<(String, String)>,
    /// Bound read-only at the same paths: the session dir (settings) and the socket.
    pub ro: Vec<PathBuf>,
    pub sock: PathBuf,
}

/// Start the supervision of a sandboxed `agent` run with `args`. Unlike `belay claude`,
/// this fails closed: with no supervisor, the sandbox does not start.
pub fn sandboxed(agent: &str, args: Vec<String>) -> Result<Sandboxed> {
    let connector = crate::connector::for_agent(agent)?;
    if connector.agent() != "claude-code" {
        bail!("belay cannot launch `{}` yet", connector.agent());
    }
    let (fresh, args) = take_fresh(args);
    let (user_settings, args) = take_settings(args);
    let bin = env::var("BELAY_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    let program = find_program(&bin)
        .and_then(|p| fs::canonicalize(p).ok())
        .with_context(|| {
            format!("`{bin}` not found on PATH (set BELAY_CLAUDE_BIN to use another binary)")
        })?;
    let exe = env::current_exe().context("find belay executable")?;
    let resume = if fresh || !crate::config::load().resume_continues() {
        crate::daemon::Resume::No
    } else {
        resume_of(&args)
    };
    let session = setup(&exe, user_settings.as_deref(), &resume)
        .context("belay: refused: the session's supervisor did not start, so nothing runs")?;
    let mut argv = vec![
        program.to_string_lossy().into_owned(),
        "--settings".into(),
        session.settings.to_string_lossy().into_owned(),
    ];
    argv.extend(args);
    Ok(Sandboxed {
        argv,
        env: vec![
            (
                "BELAY_SOCKET".into(),
                session.sock.to_string_lossy().into_owned(),
            ),
            (
                "BELAY_SESSION_DIR".into(),
                session.dir.to_string_lossy().into_owned(),
            ),
        ],
        ro: vec![program, session.dir.clone()],
        sock: session.sock.clone(),
        session,
    })
}

impl Sandboxed {
    /// The belay session's id.
    pub fn id(&self) -> &str {
        &self.session.id
    }
    /// Stop the daemon and print the session's summary.
    pub fn finish(mut self) {
        finish(&mut self.session);
    }
}

/// Session dir, token, settings, daemon. Any error here means: run without belay.
fn setup(
    exe: &Path,
    user_settings: Option<&str>,
    resume: &crate::daemon::Resume,
) -> Result<Session> {
    let root = paths::sessions_dir();
    // A crashed daemon leaves its socket behind. Clean up before starting a new one.
    let _ = crate::sessions::remove_stale_sockets(&root);
    paths::ensure_private_dir(&paths::home())?;
    let keep_days = crate::config::load().keep_days;
    if keep_days > 0 {
        let root = root.clone();
        // Old sessions hold copies of tool output; drop them without delaying the start.
        thread::spawn(move || {
            let now = paths::now_ms() / 1000;
            let _ = crate::sessions::prune(&root, keep_days, now);
        });
    }
    let id = paths::new_session_id();
    let dir = root.join(&id);
    paths::ensure_private_dir(&dir)?;
    let token = paths::new_token(&dir)?;
    let mut settings = claude_code::settings(exe);
    if let Some(user) = user_settings {
        merge_settings(&mut settings, &read_settings(user)?);
    }
    let settings_path = dir.join("claude-settings.json");
    fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
    if let Ok(cwd) = env::current_dir() {
        for w in crate::config::load_policy(&cwd).1 {
            eprintln!("belay: {w}");
        }
    }
    if crate::config::load().experimental_audit {
        if let Ok(cwd) = env::current_dir() {
            record_audit_base(&dir, &cwd);
        }
    }
    let sock = paths::socket_path(&dir);
    let mut daemon = start_daemon(exe, &dir, Some(std::process::id()), None, resume)?;
    if !wait_for(&sock) {
        let _ = daemon.kill();
        let _ = daemon.wait();
        bail!(
            "daemon did not start (see {})",
            dir.join("daemon.log").display()
        );
    }
    Ok(Session {
        id,
        dir,
        sock,
        token,
        settings: settings_path,
        daemon,
    })
}

/// Stop the daemon and print the one-line summary. Say so when the daemon died early:
/// a quiet session and a dead supervisor must not look the same.
fn finish(s: &mut Session) {
    let died = match s.daemon.try_wait() {
        Ok(Some(status)) => Some(status),
        _ => None,
    };
    if died.is_none() {
        let req = Request::Shutdown {
            token: Some(s.token.clone()),
        };
        let _ = ipc::send(&s.sock, &req, Duration::from_millis(500));
        // Reap it, so no zombie is left; give up after a short wait.
        for _ in 0..50 {
            if matches!(s.daemon.try_wait(), Ok(Some(_))) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
    let entries = crate::store::read_entries(&s.dir).unwrap_or_default();
    // `claude --version`, `claude mcp …`: no conversation started, nothing to keep.
    let events = entries
        .iter()
        .filter(|e| matches!(e.body, belay_core::Body::Event { .. }))
        .count();
    if events == 0 && died.is_none() {
        let _ = fs::remove_dir_all(&s.dir);
        return;
    }
    let audit = finish_audit(&s.dir);
    eprintln!("{}", exit_summary(&s.id, &s.dir, &entries));
    if let Some(line) = audit {
        eprintln!("belay: {line}");
    }
    if let Some(status) = died {
        let when = entries
            .last()
            .map(|e| format!(" after entry {}", e.seq))
            .unwrap_or_default();
        eprintln!(
            "belay: the daemon stopped during the session{when} ({status}); later tool calls were not recorded (see {})",
            s.dir.join("daemon.log").display()
        );
    }
}

/// The exit summary: the brief's three lines (Changed, Checked, Look at) in place of
/// the action counts, then the report's other lines (kept out, handoff, warnings).
fn exit_summary(id: &str, dir: &Path, entries: &[belay_core::Entry]) -> String {
    let full = crate::report::exit_summary(id, entries);
    let Ok(brief) = crate::brief::of_session(dir) else {
        return full;
    };
    with_brief(&full, &crate::brief::exit_lines(&brief))
}

fn with_brief(full: &str, brief: &[String]) -> String {
    let mut lines = full.lines();
    let mut out: Vec<String> = lines.next().into_iter().map(str::to_string).collect();
    out.extend(brief.iter().cloned());
    out.extend(
        lines
            .filter(|l| !l.starts_with("  did: ") && *l != "  `belay report` for more")
            .map(str::to_string),
    );
    out.push("  `belay brief` for the rest · `belay report` for more".into());
    out.join("\n")
}

/// The session audit (experimental): the working tree's state before the agent starts,
/// as a git tree, in `<session>/audit-base` ("root\ntree"). Nothing in the repo changes.
pub(crate) fn record_audit_base(dir: &Path, cwd: &Path) {
    let base = crate::audit::repo_root(cwd).ok().and_then(|root| {
        let tree = crate::audit::worktree_tree(&root, dir).ok()?;
        Some(format!("{}\n{tree}\n", root.display()))
    });
    if let Some(base) = base {
        let _ = fs::write(dir.join("audit-base"), base);
    }
}

/// Audit the session against its base: write `audit.json`, return the line to print.
pub(crate) fn finish_audit(dir: &Path) -> Option<String> {
    let base = fs::read_to_string(dir.join("audit-base")).ok()?;
    let mut lines = base.lines();
    let (root, from) = (PathBuf::from(lines.next()?), lines.next()?.to_string());
    let to = crate::audit::worktree_tree(&root, dir).ok()?;
    let policy = crate::config::load_policy(&root).0;
    let report = crate::audit::run(&root, &from, &to, &|p| policy.audit_ignores(p)).ok()?;
    let _ = fs::write(dir.join("audit.json"), serde_json::to_string(&report).ok()?);
    Some(format!(
        "{}: {} · `belay audit --session` for details",
        crate::audit::LABEL,
        report.one_line()
    ))
}

/// Claude Code versions with a conformance record, baked in at build time (build.rs).
const CONFORMANT: &str = env!("BELAY_CONFORMANT");

/// The agent's version when no conformance record covers it and the user was not told
/// yet (once per version: `<home>/versions-seen`, see `mark_version_seen`). None when it is checked, already
/// reported, or unknown (I6, P1).
fn unchecked_version(bin: &str) -> Option<String> {
    let mut child = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // A version check must not hold up the start.
    for _ in 0..150 {
        if child.try_wait().ok()?.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    if child.try_wait().ok()?.is_none() {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    let mut stdout = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut stdout).ok()?;
    let version = stdout.split_whitespace().next()?.to_string();
    if !version.chars().next().is_some_and(|c| c.is_ascii_digit())
        || CONFORMANT.split(',').any(|v| v == version)
    {
        return None;
    }
    let seen = paths::home().join("versions-seen");
    let known = fs::read_to_string(&seen).unwrap_or_default();
    if known.lines().any(|l| l == version) {
        return None;
    }
    Some(version)
}

/// Remember that the user was told about `version` (after setup made the data dir).
fn mark_version_seen(version: &str) {
    let seen = paths::home().join("versions-seen");
    let known = fs::read_to_string(&seen).unwrap_or_default();
    let _ = fs::write(&seen, format!("{known}{version}\n"));
}

/// `--belay-fresh`: a resumed conversation starts with a clean belay state (U8).
fn take_fresh(args: Vec<String>) -> (bool, Vec<String>) {
    let before = args.len();
    let rest: Vec<String> = args.into_iter().filter(|a| a != "--belay-fresh").collect();
    (rest.len() != before, rest)
}

/// What the agent's arguments resume, as the daemon will continue it (U8). A
/// conversation no belay session ran, or one whose session was pruned, starts clean.
fn resume_of(args: &[String]) -> crate::daemon::Resume {
    use crate::connector::ResumeTarget;
    use crate::daemon::Resume;
    let Ok(connector) = crate::connector::for_agent("claude-code") else {
        return Resume::No;
    };
    let cwd = env::current_dir().unwrap_or_default();
    let conversation = match connector.resume_target(args, &cwd) {
        None => return Resume::No,
        Some(ResumeTarget::Picker) => return Resume::Picker,
        Some(ResumeTarget::Latest) => connector.latest_conversation(&cwd),
        Some(ResumeTarget::Conversation(id)) => Some(id),
    };
    // `--resume <id>` keeps the id, and `--fork-session` makes a new one, which still
    // continues this story (conformance, Claude Code 2.1.316).
    match conversation.and_then(|c| Some((paths::conversation_dir(&c)?, c))) {
        Some((dir, c)) => Resume::From {
            dir,
            conversation: Some(c),
        },
        None => Resume::No,
    }
}

/// The 0.1 `--belay-mode X` (or `--belay-mode=X`) anywhere in the arguments: dropped,
/// with a warning, so an old script still starts.
fn drop_mode(args: Vec<String>) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--belay-mode" {
            it.next();
        } else if !a.starts_with("--belay-mode=") {
            out.push(a);
            continue;
        }
        warn_mode_flag();
    }
    out
}

/// What an old `--belay-mode` (or `--mode`) flag gets.
pub(crate) fn warn_mode_flag() {
    eprintln!(
        "belay: --belay-mode is ignored: belay has no modes now. It acts within what the agent supports; `belay pause` stops it."
    );
}

/// `--belay-fleet [ID]`, `--belay-role R`, `--belay-name N`: join a fleet (F4). The id is
/// optional: alone, the newest fleet that is up.
#[allow(clippy::type_complexity)]
fn take_fleet(
    args: Vec<String>,
) -> (
    Option<(Option<String>, Option<String>, Option<String>)>,
    Vec<String>,
) {
    let (mut fleet, mut role, mut name, mut join) = (None, None, None, false);
    let mut out = Vec::with_capacity(args.len());
    let mut it = args.into_iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--belay-fleet" => {
                join = true;
                if it.peek().is_some_and(|n| !n.starts_with('-')) {
                    fleet = it.next();
                }
            }
            "--belay-role" => role = it.next(),
            "--belay-name" => name = it.next(),
            _ => match a.strip_prefix("--belay-fleet=") {
                Some(v) => {
                    join = true;
                    fleet = Some(v.to_string());
                }
                None => out.push(a),
            },
        }
    }
    let join = (join || role.is_some() || name.is_some()).then_some((fleet, role, name));
    (join, out)
}

/// The user's own `--settings` value. Claude Code may keep only one `--settings`, so belay
/// merges the user's into its own file instead of passing both.
fn take_settings(args: Vec<String>) -> (Option<String>, Vec<String>) {
    let mut user = None;
    let mut out = Vec::with_capacity(args.len());
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        if a == "--settings" {
            user = it.next();
        } else if let Some(v) = a.strip_prefix("--settings=") {
            user = Some(v.to_string());
        } else {
            out.push(a);
        }
    }
    (user, out)
}

/// `--settings` takes a file or a JSON string.
fn read_settings(value: &str) -> Result<Value> {
    let text = if value.trim_start().starts_with('{') {
        value.to_string()
    } else {
        fs::read_to_string(value).with_context(|| format!("read --settings {value}"))?
    };
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("parse --settings {value}"))?;
    if !v.is_object() {
        bail!("--settings {value} is not a JSON object");
    }
    Ok(v)
}

/// Put the user's settings under belay's: the user's keys win, except that hooks are
/// combined, the user's first, per event.
fn merge_settings(belay: &mut Value, user: &Value) {
    let belay_hooks = belay.get("hooks").cloned().unwrap_or(Value::Null);
    let mut merged = user.clone();
    let mut hooks = user
        .get("hooks")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if let (Some(h), Some(ours)) = (hooks.as_object_mut(), belay_hooks.as_object()) {
        for (event, entries) in ours {
            let list = h
                .entry(event.clone())
                .or_insert_with(|| serde_json::json!([]));
            if let (Some(list), Some(entries)) = (list.as_array_mut(), entries.as_array()) {
                list.extend(entries.iter().cloned());
            }
        }
    }
    merged["hooks"] = hooks;
    *belay = merged;
}

/// Start a session daemon. `parent`: exit when this process is gone (the launcher).
/// `idle_exit_minutes`: exit after this long without a request (plugin mode, where no
/// launcher waits for the agent).
pub(crate) fn start_daemon(
    exe: &Path,
    dir: &Path,
    parent: Option<u32>,
    idle_exit_minutes: Option<u64>,
    resume: &crate::daemon::Resume,
) -> Result<Child> {
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("daemon.log"))?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon").arg("--session-dir").arg(dir);
    if let Some(pid) = parent {
        cmd.arg("--parent-pid").arg(pid.to_string());
    }
    if let Some(m) = idle_exit_minutes {
        cmd.arg("--idle-exit-minutes").arg(m.to_string());
    }
    match resume {
        crate::daemon::Resume::From { dir, conversation } => {
            cmd.arg("--resume-from").arg(dir);
            if let Some(c) = conversation {
                cmd.arg("--resume-conversation").arg(c);
            }
        }
        crate::daemon::Resume::Picker => {
            cmd.arg("--resume-picker");
        }
        crate::daemon::Resume::No => {}
    }
    cmd.stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // Own process group: Ctrl-C in the terminal must not reach the daemon.
        .process_group(0)
        .spawn()
        .context("spawn daemon")
}

pub(crate) fn wait_for(sock: &Path) -> bool {
    for _ in 0..1000 {
        if ipc::send(sock, &Request::Ping, Duration::from_millis(200)).is_ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    false
}

/// The agent's pid while it runs (0 before), for the signal handler.
static CHILD: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(sig: libc::c_int) {
    let pid = CHILD.load(Ordering::SeqCst);
    // Claude Code handles Ctrl-C itself: the terminal sends it to the whole group.
    if pid > 0 && sig != libc::SIGINT {
        // SAFETY: kill is async-signal-safe.
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

/// The launcher must outlive the agent to shut the daemon down. SIGINT is the agent's
/// (it gets it from the terminal); SIGTERM, SIGHUP, and SIGQUIT go to the agent, so
/// `kill <launcher>` stops claude too instead of leaving it unsupervised. A handler (not
/// SIG_IGN) resets to the default in the child on exec.
fn forward_signals() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT] {
        // SAFETY: installs an async-signal-safe handler.
        unsafe {
            libc::signal(
                sig,
                on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
    }
}

fn run_child(cmd: &mut Command) -> std::io::Result<ExitStatus> {
    let mut child = cmd.spawn()?;
    CHILD.store(child.id() as i32, Ordering::SeqCst);
    let status = child.wait();
    CHILD.store(0, Ordering::SeqCst);
    status
}

/// The agent's exit code; 128 + signal when a signal ended it, as a shell reports it.
fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(1)
}

/// The first `name` on PATH (or `name` itself when it has a slash).
fn find_program(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return Path::new(name).is_file().then(|| PathBuf::from(name));
    }
    env::split_paths(&env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

/// The agent's PATH: a private `<session>/bin` with a `belay` link first, so the agent can
/// run `belay show` without the rest of belay's install directory (say `~/.cargo/bin` or
/// `target/release`) shadowing its tools. Falls back to belay's own directory.
fn agent_path(exe: &Path, dir: &Path) -> OsString {
    let bin = dir.join("bin");
    let linked = fs::create_dir_all(&bin).is_ok()
        && std::os::unix::fs::symlink(exe, bin.join("belay")).is_ok();
    let first = if linked {
        Some(bin)
    } else {
        exe.parent().map(Path::to_path_buf)
    };
    let mut parts: Vec<PathBuf> = first.into_iter().collect();
    if let Some(p) = env::var_os("PATH") {
        parts.extend(env::split_paths(&p));
    }
    // A directory with `:` in its name cannot join a PATH; keep the user's PATH then,
    // not an empty one.
    env::join_paths(parts).unwrap_or_else(|_| env::var_os("PATH").unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_exit_summary_puts_the_brief_in_place_of_the_counts() {
        let full = "belay: session s1 ended.\n  did: 1 blocked edit\n  kept out: 9k tokens of tool output\n  `belay report` for more";
        let brief = strings(&[
            "  Changed: 1 file",
            "  Checked: ✓ pytest",
            "  Look at: 1 item",
        ]);
        assert_eq!(
            with_brief(full, &brief),
            "belay: session s1 ended.\n  Changed: 1 file\n  Checked: ✓ pytest\n  Look at: 1 item\n  kept out: 9k tokens of tool output\n  `belay brief` for the rest · `belay report` for more"
        );
    }

    #[test]
    fn agent_path_survives_a_colon_in_the_session_path() {
        let d = env::temp_dir().join(format!("belay-a:b-{}", std::process::id()));
        let p = agent_path(Path::new("/usr/bin/belay"), &d);
        assert_eq!(Some(p), env::var_os("PATH"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn an_old_belay_mode_flag_is_dropped_anywhere() {
        let rest = drop_mode(strings(&["-p", "hi", "--belay-mode", "optimize"]));
        assert_eq!(rest, strings(&["-p", "hi"]));
        let rest = drop_mode(strings(&["--belay-mode=look", "--resume"]));
        assert_eq!(rest, strings(&["--resume"]));
    }

    #[test]
    fn user_settings_are_merged_under_belay_hooks() {
        let (user, rest) = take_settings(strings(&["--settings", r#"{"model":"x"}"#, "-p", "hi"]));
        assert_eq!(rest, strings(&["-p", "hi"]));
        let user = read_settings(&user.unwrap()).unwrap();
        let mut belay = json!({"hooks": {"Stop": [{"hooks": [{"command": "belay hook Stop"}]}]}});
        let user = json!({
            "model": user["model"],
            "hooks": {"Stop": [{"hooks": [{"command": "mine"}]}]}
        });
        merge_settings(&mut belay, &user);
        assert_eq!(belay["model"], "x");
        let stop = belay["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert_eq!(stop[0]["hooks"][0]["command"], "mine");
        assert_eq!(stop[1]["hooks"][0]["command"], "belay hook Stop");
        assert!(read_settings("[1]").is_err());
    }
}
