//! Attention (V3): tell the user when the session needs them. The moments are a guard
//! block that waits for `belay allow`, a loop steer, a stop with verify before done
//! unmet, a finished turn left idle, and a prompt the agent waits on. Each one goes to
//! a desktop notification, the launcher's terminal bell, and `notify_command`. These
//! are user-only channels: nothing here reaches the agent (rule 5).
//!
//! Delivery never blocks a hook: it runs on its own thread, and `notify_command` is
//! killed after `COMMAND_TIMEOUT`. belay makes no network call here (rule 11); the
//! user's command may.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use belay_core::{ActionKind, Body, Decision, Event, NoticeLevel};

use crate::config::Notify;

/// One notification per kind per session in this window.
pub const RATE_MS: u64 = 120_000;
/// A finished turn with no new prompt for this long is "done".
pub const IDLE_MS: u64 = 60_000;
/// `notify_command` is killed after this.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// What the user is needed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// belay blocked an edit; it waits for `belay allow`.
    Blocked,
    /// belay steered a loop.
    Loop,
    /// The agent stopped while verify before done was unmet.
    VerifyUnmet,
    /// The turn ended and no prompt came since.
    Done,
    /// The agent waits on a permission prompt or a question.
    Waiting,
    /// A session wrote to the user (`belay msg user`).
    Message,
    /// Any other warning for the user (`notify = all` only).
    Warning,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Blocked => "blocked",
            Kind::Loop => "loop",
            Kind::VerifyUnmet => "verify_unmet",
            Kind::Done => "done",
            Kind::Waiting => "waiting",
            Kind::Message => "message",
            Kind::Warning => "warning",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub kind: Kind,
    pub text: String,
    /// The action it is about (`a-N`), for `belay why` and `belay allow`.
    pub action: Option<String>,
}

impl Alert {
    fn new(kind: Kind, text: impl Into<String>, action: Option<String>) -> Self {
        Alert {
            kind,
            text: text.into(),
            action,
        }
    }
}

/// What attention keeps for one session.
#[derive(Debug)]
pub struct Attention {
    level: Notify,
    /// Minutes of the day, local time: start, end.
    quiet: Option<(u32, u32)>,
    /// When each kind was last sent.
    last: BTreeMap<Kind, u64>,
    /// The turn ended at this time, and no prompt or tool call came since.
    idle_since: Option<u64>,
}

impl Attention {
    pub fn new(level: Notify, quiet: Option<(u32, u32)>) -> Self {
        Attention {
            level,
            quiet,
            last: BTreeMap::new(),
            idle_since: None,
        }
    }

    pub fn on(&self) -> bool {
        self.level != Notify::Off
    }

    /// The moments in a hook's decisions. `verify_unmet`: the controller's state after
    /// them (`Controller::verify_unmet`).
    pub fn observe(&mut self, decisions: &[Decision], verify_unmet: bool, now: u64) -> Vec<Alert> {
        let mut out = Vec::new();
        if !self.on() {
            return out;
        }
        let entries: Vec<_> = decisions.iter().flat_map(|d| &d.entries).collect();
        // belay kept the turn going: the agent is not done yet.
        let continued = entries.iter().any(|e| {
            matches!(
                &e.body,
                Body::Action {
                    action: ActionKind::Verify { .. },
                    applied: true,
                    ..
                }
            )
        });
        let mut told = std::collections::BTreeSet::new();
        for e in &entries {
            match &e.body {
                Body::Event { event } => match event {
                    Event::PromptSubmitted { .. }
                    | Event::ToolRequested { .. }
                    | Event::EditProposed { .. }
                    | Event::ToolCompleted { .. } => self.idle_since = None,
                    Event::TurnEnded if continued => self.idle_since = None,
                    Event::TurnEnded if verify_unmet => {
                        self.idle_since = None;
                        out.push(Alert::new(
                            Kind::VerifyUnmet,
                            "The agent stopped, and its tests have not passed since its last edits.",
                            None,
                        ));
                    }
                    Event::TurnEnded => self.idle_since = Some(now),
                    // Claude Code's own idle reminder is our "done"; a login is not a wait.
                    Event::AgentWaiting { reason, text }
                        if !matches!(reason.as_str(), "idle_prompt" | "auth_success") =>
                    {
                        let text = if text.is_empty() {
                            "The agent waits for you.".to_string()
                        } else {
                            format!("The agent waits for you: {text}")
                        };
                        out.push(Alert::new(Kind::Waiting, text, None));
                    }
                    _ => {}
                },
                Body::Action {
                    id,
                    action,
                    applied: true,
                    ..
                } => match action {
                    ActionKind::Steer { text, .. } => {
                        told.insert(id.clone());
                        out.push(Alert::new(
                            Kind::Loop,
                            belay_core::text::truncate_chars(
                                text.trim_start_matches("belay: "),
                                180,
                            ),
                            Some(id.clone()),
                        ));
                    }
                    ActionKind::Guard {
                        path,
                        decision:
                            belay_core::guard::GuardDecision::Deny
                            | belay_core::guard::GuardDecision::Ask,
                    } => {
                        told.insert(id.clone());
                        out.push(Alert::new(
                            Kind::Blocked,
                            format!("Blocked an edit of {path}. If it is intended, run `! belay allow`."),
                            Some(id.clone()),
                        ));
                    }
                    _ => {}
                },
                Body::Notice { notice, .. }
                    if self.level == Notify::All
                        && notice.level == NoticeLevel::Warn
                        && !notice.action.as_ref().is_some_and(|a| told.contains(a)) =>
                {
                    out.push(Alert::new(
                        Kind::Warning,
                        notice.text.clone(),
                        notice.action.clone(),
                    ));
                }
                _ => {}
            }
        }
        out
    }

    /// "done": the turn ended `IDLE_MS` ago and nothing came since. Once per turn.
    pub fn idle_due(&mut self, now: u64) -> Option<Alert> {
        let since = self.idle_since?;
        if now.saturating_sub(since) < IDLE_MS {
            return None;
        }
        self.idle_since = None;
        Some(Alert::new(
            Kind::Done,
            "The agent finished its turn and waits for you.",
            None,
        ))
    }

    /// Whether to send this kind now: the level is on, outside the quiet window
    /// (`minute`: local minute of the day, if known), and not sent in `RATE_MS`.
    pub fn admit(&mut self, kind: Kind, now: u64, minute: Option<u32>) -> bool {
        if !self.on() || minute.is_some_and(|m| self.quiet.is_some_and(|q| in_window(q, m))) {
            return false;
        }
        if let Some(t) = self.last.get(&kind) {
            if now.saturating_sub(*t) < RATE_MS {
                return false;
            }
        }
        self.last.insert(kind, now);
        true
    }
}

/// `minute` is in `[start, end)`, wrapping past midnight when start > end.
fn in_window((start, end): (u32, u32), minute: u32) -> bool {
    if start <= end {
        (start..end).contains(&minute)
    } else {
        minute >= start || minute < end
    }
}

/// The local minute of the day, from the system's time zone.
pub fn local_minute(ms: u64) -> Option<u32> {
    let secs = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r writes only into `tm`, which lives on this stack frame.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return None;
    }
    Some(tm.tm_hour as u32 * 60 + tm.tm_min as u32)
}

/// Where one session's notifications go.
#[derive(Debug, Clone, Default)]
pub struct Channels {
    pub session: String,
    pub repo: String,
    /// The terminal bell: only under the launcher, whose terminal this is.
    pub bell: bool,
    pub desktop: bool,
    pub command: Option<String>,
}

/// The JSON `notify_command` reads on stdin.
pub fn payload(c: &Channels, a: &Alert, now: u64) -> serde_json::Value {
    serde_json::json!({
        "session": c.session,
        "repo": c.repo,
        "kind": a.kind.name(),
        "text": a.text,
        "action": a.action,
        "ts_ms": now,
    })
}

/// Send alerts on a thread of their own: the hook that found them does not wait.
pub fn deliver(c: Channels, alerts: Vec<Alert>, now: u64) {
    if alerts.is_empty() {
        return;
    }
    thread::spawn(move || {
        for a in &alerts {
            let msg = format!("belay: {}", a.text);
            if c.desktop {
                desktop(&msg);
            }
            if c.bell {
                bell();
            }
            if let Some(cmd) = &c.command {
                run_command(cmd, &payload(&c, a, now), COMMAND_TIMEOUT);
            }
        }
    });
}

/// A desktop notification: `notify-send` (Linux) or `osascript` (macOS). A missing tool
/// is ignored.
pub(crate) fn desktop(msg: &str) {
    // Agent text must not leave belay with a secret in it.
    let msg = belay_core::text::redact(msg);
    let msg = msg.as_ref();
    let mut cmd = if cfg!(target_os = "macos") {
        let quoted = msg.replace('\\', "\\\\").replace('"', "\\\"");
        let mut c = Command::new("osascript");
        c.arg("-e").arg(format!(
            "display notification \"{quoted}\" with title \"belay\""
        ));
        c
    } else {
        let mut c = Command::new("notify-send");
        c.arg("belay").arg(msg);
        c
    };
    if let Ok(mut child) = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        // Reap it, or each notification leaves a zombie behind.
        let _ = child.wait();
    }
}

/// BEL to the controlling terminal, if there is one. Non-blocking: a stopped terminal
/// does not hold the daemon.
fn bell() {
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(mut tty) = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open("/dev/tty")
    {
        let _ = tty.write_all(b"\x07");
    }
}

/// Run `cmd` with `sh -c`, `json` on its stdin, and kill its process group after
/// `timeout`. Blocks the calling thread until it ends or is killed. Returns whether it
/// ended on its own.
pub fn run_command(cmd: &str, json: &serde_json::Value, timeout: Duration) -> bool {
    let Ok(mut child) = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Its own group: the kill reaches what it started too.
        .process_group(0)
        .spawn()
    else {
        return false;
    };
    // A command that never reads its stdin must not hold this thread: the line fits
    // the pipe's buffer, and a closed pipe is ignored.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin
            .write_all(format!("{}\n", belay_core::text::redact(&json.to_string())).as_bytes());
    }
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if start.elapsed() < timeout => thread::sleep(Duration::from_millis(20)),
            _ => break,
        }
    }
    let pgid = child.id() as libc::pid_t;
    // SAFETY: a signal to the group this child leads; it has not been reaped yet.
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
    let _ = child.wait();
    false
}

#[cfg(test)]
mod tests;
