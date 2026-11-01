//! Sandbox limits. systemd user scopes do not work everywhere (not on WSL without
//! systemd), so belay's own watchdog thread, outside the sandbox, follows the sandbox's
//! process tree through /proc: total memory, process count, disk use, wall time. On a
//! breach it stops the sandbox's outer process; the pid namespace takes the rest down.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Bytes of resident memory, all processes together.
    pub memory: u64,
    pub procs: u64,
    /// Bytes on disk, worktree and private home together.
    pub disk: u64,
    pub wall: Duration,
}

impl Limits {
    pub fn from_config(c: &crate::config::Config) -> Self {
        Self {
            memory: c.sandbox_memory,
            procs: c.sandbox_procs,
            disk: c.sandbox_disk,
            wall: Duration::from_secs(c.sandbox_wall),
        }
    }
}

const UNITS: [(char, u32); 4] = [('T', 40), ('G', 30), ('M', 20), ('K', 10)];

/// `4G`, `512M`, `100K`, or bytes. Binary units.
pub fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, shift) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some(c) => match UNITS.iter().find(|(u, _)| *u == c) {
            Some((_, shift)) => (&s[..s.len() - 1], *shift),
            None => (s, 0),
        },
        None => bail!("empty size"),
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("`{s}` is not a size like 4G or 512M"))?;
    n.checked_shl(shift)
        .filter(|v| v >> shift == n)
        .ok_or_else(|| anyhow::anyhow!("`{s}` is too large"))
}

pub fn fmt_size(b: u64) -> String {
    for (u, shift) in UNITS {
        if b >= 1 << shift && b.is_multiple_of(1 << shift) {
            return format!("{}{u}", b >> shift);
        }
    }
    b.to_string()
}

/// `4h`, `30m`, `90s`, or seconds.
pub fn parse_duration(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mul) = match s.chars().last() {
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('s') => (&s[..s.len() - 1], 1),
        _ => (s, 1),
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("`{s}` is not a duration like 4h or 30m"))?;
    n.checked_mul(mul)
        .ok_or_else(|| anyhow::anyhow!("`{s}` is too long"))
}

pub fn fmt_duration(secs: u64) -> String {
    match secs {
        s if s >= 3600 && s.is_multiple_of(3600) => format!("{}h", s / 3600),
        s if s >= 60 && s.is_multiple_of(60) => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// A count like tokens: `2M`, `500K`, or a number. Decimal units.
pub fn parse_count(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mul) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&s[..s.len() - 1], 1_000),
        Some('M') => (&s[..s.len() - 1], 1_000_000),
        Some('G') => (&s[..s.len() - 1], 1_000_000_000),
        _ => (s, 1),
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("`{s}` is not a count like 2M or 500K"))?;
    n.checked_mul(mul)
        .ok_or_else(|| anyhow::anyhow!("`{s}` is too large"))
}

pub fn fmt_count(n: u64) -> String {
    match n {
        n if n >= 1_000_000 && n.is_multiple_of(1_000_000) => format!("{}M", n / 1_000_000),
        n if n >= 1_000 && n.is_multiple_of(1_000) => format!("{}K", n / 1_000),
        n => n.to_string(),
    }
}

/// `root` and every process below it, from /proc.
pub fn tree(root: i32) -> Vec<i32> {
    let mut children: BTreeMap<i32, Vec<i32>> = BTreeMap::new();
    if let Ok(rd) = fs::read_dir("/proc") {
        for e in rd.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else {
                continue;
            };
            if let Some(ppid) = ppid(pid) {
                children.entry(ppid).or_default().push(pid);
            }
        }
    }
    let mut out = vec![root];
    let mut i = 0;
    while i < out.len() {
        if let Some(c) = children.get(&out[i]) {
            out.extend(c);
        }
        i += 1;
    }
    out
}

fn ppid(pid: i32) -> Option<i32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The name may hold spaces and parentheses: fields start after the last ')'.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

fn rss_bytes(pid: i32) -> u64 {
    let page = page_size();
    fs::read_to_string(format!("/proc/{pid}/statm"))
        .ok()
        .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map_or(0, |pages| pages * page)
}

fn page_size() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if p > 0 {
        p as u64
    } else {
        4096
    }
}

/// Bytes the files under `paths` take on disk. Symlinks are not followed; a file with
/// several links is counted once.
pub fn disk_use(paths: &[PathBuf]) -> u64 {
    let mut seen = std::collections::BTreeSet::new();
    let mut total = 0;
    let mut stack: Vec<PathBuf> = paths.to_vec();
    while let Some(p) = stack.pop() {
        let Ok(m) = fs::symlink_metadata(&p) else {
            continue;
        };
        if m.nlink() > 1 && !m.is_dir() && !seen.insert((m.dev(), m.ino())) {
            continue;
        }
        total += m.blocks() * 512;
        if m.is_dir() {
            if let Ok(rd) = fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
        }
    }
    total
}

/// A process handle that cannot hit a reused pid: signals go through a pidfd.
pub struct Target {
    pid: i32,
    fd: Option<OwnedFd>,
}

impl Target {
    pub fn new(pid: i32) -> Self {
        // SAFETY: pidfd_open takes a pid and flags; it returns a new fd or -1.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        let fd = (fd >= 0).then(|| {
            // SAFETY: the fd is new and ours alone.
            unsafe { OwnedFd::from_raw_fd(fd as i32) }
        });
        Self { pid, fd }
    }

    /// Send `sig`; false when the process is gone.
    pub fn signal(&self, sig: i32) -> bool {
        match &self.fd {
            // SAFETY: a valid pidfd, a null siginfo, no flags.
            Some(fd) => unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    sig,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                ) == 0
            },
            // Kernels before 5.3: the pid is still ours while the child is not reaped,
            // and the watchdog stops before it is.
            // SAFETY: kill has no memory preconditions.
            None => unsafe { libc::kill(self.pid, sig) == 0 },
        }
    }

    pub fn alive(&self) -> bool {
        self.signal(0)
    }
}

/// The breach, in words, or None while within the limits.
pub fn check(
    limits: &Limits,
    procs: &[i32],
    started: Instant,
    disk: Option<u64>,
) -> Option<String> {
    if procs.len() as u64 > limits.procs {
        return Some(format!(
            "it ran more than {} processes (sandbox_procs)",
            limits.procs
        ));
    }
    let rss: u64 = procs.iter().map(|p| rss_bytes(*p)).sum();
    if rss > limits.memory {
        return Some(format!(
            "its memory went over {} (sandbox_memory)",
            fmt_size(limits.memory)
        ));
    }
    if disk.is_some_and(|d| d > limits.disk) {
        return Some(format!(
            "its disk use went over {} (sandbox_disk)",
            fmt_size(limits.disk)
        ));
    }
    if started.elapsed() > limits.wall {
        return Some(format!(
            "it ran longer than {} (sandbox_wall)",
            fmt_duration(limits.wall.as_secs())
        ));
    }
    None
}

/// Follows a running sandbox until `stop`.
pub struct Watchdog {
    done: Arc<AtomicBool>,
    handle: JoinHandle<Option<String>>,
}

const TICK: Duration = Duration::from_millis(50);
const DISK_EVERY: Duration = Duration::from_secs(2);
const GRACE: Duration = Duration::from_secs(2);

impl Watchdog {
    /// Watch the process `pid` and all below it. On a breach: SIGTERM, then SIGKILL
    /// after a grace period.
    pub fn start(pid: i32, limits: Limits, disk_paths: Vec<PathBuf>) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let target = Target::new(pid);
        let handle = std::thread::spawn(move || {
            let started = Instant::now();
            let mut disk_at: Option<Instant> = None;
            while !flag.load(Ordering::Relaxed) {
                let disk = (disk_at.is_none_or(|t| t.elapsed() >= DISK_EVERY)).then(|| {
                    disk_at = Some(Instant::now());
                    disk_use(&disk_paths)
                });
                let procs = tree(pid);
                if let Some(why) = check(&limits, &procs, started, disk) {
                    stop(&target, &flag);
                    return Some(why);
                }
                std::thread::sleep(TICK);
            }
            None
        });
        Self { done, handle }
    }

    /// Stop watching (the process ended). The breach, if the watchdog stopped it.
    pub fn finish(self) -> Option<String> {
        self.done.store(true, Ordering::Relaxed);
        self.handle.join().ok().flatten()
    }
}

/// SIGTERM, then SIGKILL if it is still there after the grace period, or until the
/// caller says it is done (it reaped the process).
pub fn stop(target: &Target, done: &AtomicBool) {
    if !target.signal(libc::SIGTERM) {
        return;
    }
    let t = Instant::now();
    while t.elapsed() < GRACE && !done.load(Ordering::Relaxed) && target.alive() {
        std::thread::sleep(TICK);
    }
    if !done.load(Ordering::Relaxed) {
        target.signal(libc::SIGKILL);
    }
}

/// In the child, before exec: no core files, no file larger than the disk cap, and
/// Ctrl-C back to its default (the parent ignores it to commit what was done).
pub fn pre_exec(cmd: &mut Command, fsize: u64) {
    // SAFETY: only async-signal-safe calls (setrlimit, signal) run between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            let core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            let file = libc::rlimit {
                rlim_cur: fsize as libc::rlim_t,
                rlim_max: fsize as libc::rlim_t,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &core) != 0
                || libc::setrlimit(libc::RLIMIT_FSIZE, &file) != 0
            {
                // Fail closed: no limits, no run.
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            Ok(())
        });
    }
}

/// Whether `systemd-run --user --scope` works here (it does not on WSL without systemd).
pub fn systemd_scope_works() -> bool {
    Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The systemd-run words that put a command in a scope with these limits.
pub fn systemd_prefix(l: &Limits) -> Vec<String> {
    vec![
        "systemd-run".into(),
        "--user".into(),
        "--scope".into(),
        "--quiet".into(),
        "-p".into(),
        format!("MemoryMax={}", l.memory),
        "-p".into(),
        format!("TasksMax={}", l.procs),
        "--".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_durations_round_trip() {
        assert_eq!(parse_size("4G").unwrap(), 4 << 30);
        assert_eq!(parse_size("512m").unwrap(), 512 << 20);
        assert_eq!(parse_size("1000").unwrap(), 1000);
        assert!(parse_size("lots").is_err());
        assert!(parse_size("99999999999T").is_err());
        assert_eq!(fmt_size(4 << 30), "4G");
        assert_eq!(fmt_size(1536 << 20), "1536M");
        assert_eq!(fmt_size(1000), "1000");
        assert_eq!(parse_duration("4h").unwrap(), 4 * 3600);
        assert_eq!(parse_duration("90s").unwrap(), 90);
        assert_eq!(parse_duration("7").unwrap(), 7);
        assert!(parse_duration("1d").is_err());
        assert_eq!(fmt_duration(4 * 3600), "4h");
        assert_eq!(fmt_duration(90), "90s");
    }

    #[test]
    fn the_tree_holds_a_child_and_its_child() {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 5 & wait"])
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let t = Instant::now();
        while tree(pid).len() < 2 && t.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(tree(pid).len() >= 2);
        Target::new(pid).signal(libc::SIGKILL);
        let _ = child.wait();
        for p in tree(pid).into_iter().skip(1) {
            Target::new(p).signal(libc::SIGKILL);
        }
    }

    #[test]
    fn the_watchdog_stops_a_memory_hog_and_says_why() {
        let limits = Limits {
            memory: 64 << 20,
            procs: 100,
            disk: 1 << 40,
            wall: Duration::from_secs(60),
        };
        // `tail` keeps a line in memory, and /dev/zero has no newline.
        let mut child = Command::new("sh")
            .args(["-c", "exec head -c 2000M /dev/zero | tail"])
            .stdout(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let dog = Watchdog::start(pid, limits, vec![]);
        let status = child.wait().unwrap();
        // The shell's pipeline children outlive it: clean them up.
        // SAFETY: a negative pid signals the group we made above.
        unsafe { libc::kill(-pid, libc::SIGKILL) };
        let why = dog.finish();
        assert!(!status.success());
        assert!(
            why.as_deref().is_some_and(|w| w.contains("memory")),
            "{why:?}"
        );
    }

    #[test]
    fn checks_name_the_limit() {
        let l = Limits {
            memory: 1 << 40,
            procs: 1,
            disk: 10,
            wall: Duration::from_secs(60),
        };
        let me = std::process::id() as i32;
        assert!(check(&l, &[me, me], Instant::now(), None)
            .unwrap()
            .contains("sandbox_procs"));
        assert!(check(&l, &[me], Instant::now(), Some(11))
            .unwrap()
            .contains("sandbox_disk"));
        assert!(check(&l, &[me], Instant::now(), Some(10)).is_none());
    }
}
