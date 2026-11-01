//! `belay watch`: follow a session's ledger and print what belay tells the user as it
//! happens. It reads only the ledger, so it works for any agent, with or without a
//! user channel of its own (rule 5: none of this reaches the model).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use belay_core::{Body, Entry, NoticeLevel};

use crate::sessions::is_live;

const POLL: Duration = Duration::from_millis(250);
/// How often to ask whether the daemon still runs: a connect per poll would be waste.
const LIVE_CHECK: Duration = Duration::from_secs(2);

/// New complete lines of a growing ledger, from where the last read ended.
pub struct Tail {
    path: PathBuf,
    offset: u64,
    partial: Vec<u8>,
}

impl Tail {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: Vec::new(),
        }
    }

    /// Entries appended since the last call. A line still being written waits for its
    /// newline; a line that does not parse is skipped.
    pub fn poll(&mut self) -> Vec<Entry> {
        let Ok(mut f) = File::open(&self.path) else {
            return Vec::new();
        };
        let mut buf = Vec::new();
        if f.seek(SeekFrom::Start(self.offset)).is_err() || f.read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        self.offset += buf.len() as u64;
        self.partial.extend_from_slice(&buf);
        let Some(end) = self.partial.iter().rposition(|b| *b == b'\n') else {
            return Vec::new();
        };
        let complete: Vec<u8> = self.partial.drain(..=end).collect();
        String::from_utf8_lossy(&complete)
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }
}

/// The line to print for an entry: notices only, Info ones with `verbose`.
pub fn line(e: &Entry, verbose: bool) -> Option<String> {
    if let Body::Resumed { from_session, .. } = &e.body {
        // Ids go on from that session's: `belay why` and `belay show` find them there.
        return Some(format!(
            "{} info continues session {from_session}",
            clock(e.ts_ms)
        ));
    }
    let Body::Notice { notice, .. } = &e.body else {
        return None;
    };
    if notice.level == NoticeLevel::Info && !verbose {
        return None;
    }
    Some(format!(
        "{} {:<4} {}",
        clock(e.ts_ms),
        crate::report::level_name(notice.level),
        notice.text
    ))
}

/// Local time of day, HH:MM:SS.
fn clock(ms: u64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r writes only into `tm`, which lives on this stack frame.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        let s = ms / 1000 % 86_400;
        return format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60);
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

fn name(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The newest session whose daemon runs, waiting for one if there is none.
fn newest_live() -> PathBuf {
    let mut said = false;
    loop {
        if let Some(d) = crate::paths::list_sessions()
            .into_iter()
            .rev()
            .find(|d| is_live(d))
        {
            return d;
        }
        if !said {
            println!("belay: no live session yet; waiting for one. Ctrl-C to stop.");
            said = true;
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// Follow `session` (or the newest live one) until it ends or the user presses Ctrl-C.
pub fn run(session: Option<&str>, verbose: bool) -> Result<()> {
    let dir = match session {
        Some(s) => crate::sessions::resolve(Some(s), false)?,
        None => newest_live(),
    };
    println!("belay: watching session {}. Ctrl-C to stop.", name(&dir));
    let mut tail = Tail::new(dir.join("ledger.jsonl"));
    let mut checked = Instant::now();
    let mut live = is_live(&dir);
    loop {
        for e in tail.poll() {
            if let Some(l) = line(&e, verbose) {
                println!("{l}");
            }
        }
        if !live {
            println!(
                "belay: session {} ended. `belay report` for more.",
                name(&dir)
            );
            return Ok(());
        }
        thread::sleep(POLL);
        if checked.elapsed() >= LIVE_CHECK {
            checked = Instant::now();
            // One more read after the daemon stops: its last entries come first.
            live = is_live(&dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_core::Notice;
    use std::io::Write;

    fn notice(seq: u64, level: NoticeLevel, text: &str) -> Entry {
        Entry {
            seq,
            ts_ms: 0,
            causes: vec![],
            body: Body::Notice {
                notice: Notice {
                    level,
                    text: text.into(),
                    action: None,
                },
                shown: false,
            },
        }
    }

    #[test]
    fn tail_reads_notices_as_the_ledger_grows() {
        let dir = std::env::temp_dir().join(format!("belay-watch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.jsonl");
        let mut f = File::create(&path).unwrap();
        let json = |e: &Entry| serde_json::to_string(e).unwrap();
        let mut tail = Tail::new(path.clone());
        assert!(tail.poll().is_empty());

        writeln!(
            f,
            "{}",
            json(&notice(1, NoticeLevel::Warn, "blocked an edit"))
        )
        .unwrap();
        let first = tail.poll();
        assert_eq!(first.len(), 1);
        assert!(line(&first[0], false)
            .unwrap()
            .ends_with(" warn blocked an edit"));

        // Half a line waits for the rest.
        let next = json(&notice(2, NoticeLevel::Act, "summarized output"));
        let (a, b) = next.split_at(10);
        write!(f, "{a}").unwrap();
        assert!(tail.poll().is_empty());
        writeln!(f, "{b}").unwrap();
        writeln!(
            f,
            "{}",
            json(&notice(3, NoticeLevel::Info, "ran it wrapped"))
        )
        .unwrap();
        let got = tail.poll();
        assert_eq!(got.len(), 2);
        assert!(line(&got[0], false)
            .unwrap()
            .ends_with(" act  summarized output"));
        assert_eq!(line(&got[1], false), None, "info only with --verbose");
        assert!(line(&got[1], true).is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
