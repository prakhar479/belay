//! Background watchers: done-when-idle, fleet awareness, idle exit, and parent exit.

use super::*;

/// A finished turn that stays idle is "done" (V3): checked each second.
pub(super) fn watch_done(state: &Mutex<State>) {
    loop {
        thread::sleep(Duration::from_secs(1));
        let mut st = lock(state);
        if let Some(a) = st.attention.idle_due(now_ms()) {
            alert(&mut st, vec![a]);
        }
    }
}

/// How often the fleet watcher looks at the other sessions (F1).
pub(super) const FLEET_POLL: Duration = Duration::from_secs(1);

/// Fleet awareness (F1): once the session has started, record its workspace, then feed
/// the other live sessions' activity to the controller. Off the hook path: the lock is
/// held only to handle what one round found. Errors are logged and skipped.
pub(super) fn watch_fleet(dir: PathBuf, state: &Mutex<State>) {
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
pub(super) fn watch_idle(minutes: u64, sock: &Path, state: &Mutex<State>) {
    loop {
        thread::sleep(Duration::from_secs(30));
        let last = LAST_REQUEST_MS.load(std::sync::atomic::Ordering::Relaxed);
        if now_ms().saturating_sub(last) > minutes * 60_000 {
            eprintln!("belayd: idle for {minutes} min; exiting");
            // No request may be writing the ledger when the process ends.
            let _held = end_session(state);
            let _ = fs::remove_file(sock);
            std::process::exit(0);
        }
    }
}

/// Exit when the launcher is gone, so no daemon outlives its session.
pub(super) fn watch_parent(pid: i32, sock: &Path, state: &Mutex<State>) {
    loop {
        thread::sleep(Duration::from_secs(2));
        // SAFETY: kill with signal 0 only checks that the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
        if !alive {
            eprintln!("belayd: launcher {pid} is gone; exiting");
            let _held = end_session(state);
            let _ = fs::remove_file(sock);
            std::process::exit(0);
        }
    }
}
