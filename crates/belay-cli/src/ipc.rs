//! Shim/CLI ↔ daemon protocol: one JSON line each way over a Unix socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// A hook call from the agent. `payload` is the hook's stdin JSON.
    Hook {
        event: String,
        payload: Value,
        /// When the shim stops waiting (ms since the epoch). A reply after it reaches no
        /// one, so the daemon must not record its actions as applied (D9).
        #[serde(default)]
        deadline_ms: Option<u64>,
    },
    // Control requests carry the session's control token (see paths::read_token).
    /// The user pauses (or resumes) one feature, or all of them (`feature` None).
    Pause {
        #[serde(default)]
        feature: Option<String>,
        #[serde(default)]
        resume: bool,
        #[serde(default)]
        token: Option<String>,
    },
    /// `belay run` finished a wrapped command. The reply message is the text to print.
    Filter {
        tool_use_id: String,
        output: String,
        /// The wrapped command's exit code (older `belay run` builds leave it out).
        #[serde(default)]
        exit_code: Option<i32>,
    },
    /// Allow one blocked edit (the latest if `action` is None).
    Allow {
        action: Option<String>,
        #[serde(default)]
        token: Option<String>,
    },
    /// The user rolled the files back to snapshot `id` (`belay undo`): record it.
    Undone {
        id: String,
        before: String,
        files: Vec<String>,
        #[serde(default)]
        token: Option<String>,
    },
    /// The agent's words for its commons card (`belay card`); unset fields stay.
    Card {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        task: Option<String>,
        #[serde(default)]
        note: Option<String>,
        #[serde(default)]
        offers: Option<Vec<String>>,
        #[serde(default)]
        join: Option<Vec<String>>,
        #[serde(default)]
        leave: Option<Vec<String>>,
        /// The sender's session (its dir's name), from the CLI's `BELAY_SESSION_DIR`:
        /// requests carry who sends them, so a socket used from another session's
        /// shell is refused. None: an older CLI, or a shell that is in no session.
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        handoff: Option<String>,
    },
    /// A commons message from this session (C2): to a card name, `topic:<t>`, `repo`,
    /// or `user`.
    Send {
        to: String,
        #[serde(default)]
        kind: Option<String>,
        /// The sender's session (its dir's name), from the CLI's `BELAY_SESSION_DIR`:
        /// requests carry who sends them, so a socket used from another session's
        /// shell is refused. None: an older CLI, or a shell that is in no session.
        #[serde(default)]
        from: Option<String>,
        text: String,
    },
    /// Publish a finding (C3).
    Share {
        #[serde(default)]
        kind: Option<String>,
        text: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        files: Vec<String>,
        /// The sender's session (its dir's name), from the CLI's `BELAY_SESSION_DIR`:
        /// requests carry who sends them, so a socket used from another session's
        /// shell is refused. None: an older CLI, or a shell that is in no session.
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        code: Option<String>,
    },
    /// Search findings and learned fixes (C3). The reply is what to print.
    Ask {
        query: String,
        #[serde(default)]
        here: bool,
        /// The sender's session (its dir's name), from the CLI's `BELAY_SESSION_DIR`:
        /// requests carry who sends them, so a socket used from another session's
        /// shell is refused. None: an older CLI, or a shell that is in no session.
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        json: bool,
    },
    /// `belay sandbox` started or stopped a sandbox from this session's shell: record it.
    /// Only the two sandbox events are accepted.
    Sandbox {
        event: belay_core::Event,
    },
    Ping,
    Shutdown {
        #[serde(default)]
        token: Option<String>,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Reply {
    /// JSON for the shim to print on stdout. None = print nothing.
    #[serde(default)]
    pub output: Option<Value>,
    #[serde(default)]
    pub message: Option<String>,
}

pub fn send(socket: &Path, req: &Request, timeout: Duration) -> Result<Reply> {
    let mut stream =
        connect_now(socket).with_context(|| format!("connect {}", socket.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(serde_json::from_str(&reply)?)
}

/// Connect without waiting. A plain connect blocks while the daemon's listen backlog
/// is full (a stuck daemon), and the shim's timeout covers only reads and writes: the
/// hook would hang. Here a full backlog fails at once, and the shim fails open.
pub fn connect_now(path: &Path) -> std::io::Result<UnixStream> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::FromRawFd;
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: a zeroed sockaddr_un is valid; the path is copied in with room for NUL.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    // SAFETY: plain socket calls on a descriptor this function owns; it is closed on
    // every error path, or handed to UnixStream, which closes it.
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fail = |fd| {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            Err(e)
        };
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0
            || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
            || libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0
        {
            return fail(fd);
        }
        let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        if libc::connect(fd, &addr as *const _ as *const libc::sockaddr, len) != 0 {
            return fail(fd);
        }
        if libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
            return fail(fd);
        }
        Ok(UnixStream::from_raw_fd(fd))
    }
}

/// A socket file nobody listens on, as a crashed daemon or hub leaves it. Tests only.
///
/// Dropping a listener does not close it at once: a child that another test thread is
/// forking holds a copy of every descriptor until it execs, and the socket still
/// accepts (then resets) a connection meanwhile. Wait until it refuses.
#[cfg(test)]
pub fn dead_socket(path: &Path) {
    drop(std::os::unix::net::UnixListener::bind(path).expect("bind a test socket"));
    for _ in 0..200 {
        match connect_now(path) {
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => return,
            _ => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }
    panic!("{} still accepts connections after 2 s", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_backlog_fails_at_once_instead_of_blocking() {
        let d = std::env::temp_dir().join(format!("belay-ipc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let sock = d.join("s.sock");
        // A daemon that never accepts: its backlog fills.
        let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let start = std::time::Instant::now();
        let mut held = Vec::new();
        let mut refused = false;
        for _ in 0..10_000 {
            match connect_now(&sock) {
                Ok(s) => held.push(s),
                Err(_) => {
                    refused = true;
                    break;
                }
            }
        }
        assert!(refused, "the backlog never filled");
        let t = std::time::Instant::now();
        assert!(send(&sock, &Request::Ping, Duration::from_millis(50)).is_err());
        assert!(t.elapsed() < Duration::from_millis(500), "send waited");
        assert!(start.elapsed() < Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_path_too_long_for_a_socket_is_an_error() {
        assert!(connect_now(Path::new(&"/x".repeat(100))).is_err());
    }
}
