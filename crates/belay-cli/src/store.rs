//! Session store: append-only JSONL ledger plus content-addressed blobs.
//!
//! Prototype deviation from the reference (section 9.1): JSONL instead of SQLite.
//! Keep this module's API stable so the backend can change later.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use belay_core::text::redact;
use belay_core::{Decision, Entry, LEDGER_FORMAT};
use serde_json::{json, Value};

pub struct Store {
    dir: PathBuf,
    ledger: File,
    hooks: Option<File>,
}

/// True when the file's last byte is not a newline. Reads one byte, not the file.
pub fn ends_torn(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = File::open(path) else {
        return false;
    };
    let mut c = [0u8; 1];
    f.seek(SeekFrom::End(-1)).is_ok() && f.read_exact(&mut c).is_ok() && c[0] != b'\n'
}

fn append_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir.join("blobs"))?;
        // Raw payloads hold code and file contents. Keep them only when asked.
        let capture = std::env::var("BELAY_CAPTURE").is_ok_and(|v| v == "1");
        let ledger_path = dir.join("ledger.jsonl");
        let mut ledger = append_file(&ledger_path)?;
        // A crash can leave a last line without its newline. End it, so the next entry
        // is not glued to it (R0.6). Readers skip the torn line.
        if ends_torn(&ledger_path) {
            ledger.write_all(b"\n")?;
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            ledger,
            hooks: if capture {
                Some(append_file(&dir.join("hooks.jsonl"))?)
            } else {
                None
            },
        })
    }

    /// Keep the raw hook payload. Phase 0 spikes and new fixtures come from this file.
    pub fn capture(&mut self, ts_ms: u64, event: &str, payload: &Value) -> Result<()> {
        if let Some(f) = self.hooks.as_mut() {
            let line = json!({ "ts_ms": ts_ms, "event": event, "payload": payload }).to_string();
            writeln!(f, "{}", redact(&line))?;
        }
        Ok(())
    }

    /// Write blobs first, then entries, so every blob an entry names exists.
    pub fn persist(&mut self, d: &Decision) -> Result<()> {
        for b in &d.blobs {
            let p = self.dir.join("blobs").join(&b.hash);
            if !p.exists() {
                // Temp file + rename: a crash never leaves a truncated blob under its hash.
                let tmp =
                    self.dir
                        .join("blobs")
                        .join(format!(".{}.{}.tmp", b.hash, std::process::id()));
                // Secrets never reach the disk; the hash still names the original.
                fs::write(&tmp, redact(&b.content).as_bytes())?;
                fs::rename(&tmp, &p)?;
            }
        }
        // One write for all lines of a decision: entries never interleave or split.
        let mut lines = String::new();
        for e in &d.entries {
            lines.push_str(&redact(&serde_json::to_string(e)?));
            lines.push('\n');
        }
        self.ledger.write_all(lines.as_bytes())?;
        Ok(())
    }
}

pub fn read_entries(dir: &Path) -> Result<Vec<Entry>> {
    let path = dir.join("ledger.jsonl");
    let f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
    let mut out = Vec::new();
    for (n, line) in BufReader::new(f).split(b'\n').enumerate() {
        // A torn last line can end inside a multi-byte character: skip it, not the ledger.
        let line = String::from_utf8_lossy(&line?).into_owned();
        if line.trim().is_empty() {
            continue;
        }
        // Check the version before parsing entries: a newer format may not parse at all.
        if out.is_empty() {
            check_format(&line, &path)?;
        }
        match serde_json::from_str(&line) {
            Ok(e) => out.push(e),
            Err(err) => eprintln!("belay: skip ledger line {}: {err}", n + 1),
        }
    }
    belay_core::record::settle_undelivered(&mut out);
    Ok(out)
}

/// One run of a resumed conversation: its session directory and ledger.
pub struct Run {
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
}

/// The session this ledger continues (U8): a `resumed` entry before its first event.
/// A `resumed` entry later on records a `/resume` inside a run: not a link.
pub fn resumed_from(entries: &[Entry]) -> Option<(String, Option<String>)> {
    entries
        .iter()
        .take_while(|e| !matches!(e.body, belay_core::Body::Event { .. }))
        .find_map(|e| match &e.body {
            belay_core::Body::Resumed {
                from_session,
                conversation,
                ..
            } => Some((from_session.clone(), conversation.clone())),
            _ => None,
        })
}

/// Longest chain of resumed runs followed (A → B → C …).
const MAX_CHAIN: usize = 64;

/// A session's ledger and the earlier runs it resumed, oldest first. A run whose
/// directory is gone (pruned) ends the chain there.
pub fn read_chain(dir: &Path) -> Result<Vec<Run>> {
    let entries = read_entries(dir)?;
    let mut runs = vec![Run {
        dir: dir.to_path_buf(),
        entries,
    }];
    while runs.len() < MAX_CHAIN {
        let last = &runs[runs.len() - 1];
        let Some((from, _)) = resumed_from(&last.entries) else {
            break;
        };
        // A session name, never a path; earlier runs live next to this one.
        if from.is_empty() || from.contains(['/', '\\']) || from.starts_with('.') {
            break;
        }
        let Some(prev) = last.dir.parent().map(|p| p.join(&from)) else {
            break;
        };
        if runs.iter().any(|r| r.dir == prev) {
            break;
        }
        match read_entries(&prev) {
            Ok(entries) => runs.push(Run { dir: prev, entries }),
            Err(_) => break,
        }
    }
    runs.reverse();
    Ok(runs)
}

/// Every entry of the chain, oldest run first. Ids and entry numbers continue from run
/// to run, so they name one thing across it.
pub fn chain_entries(runs: &[Run]) -> Vec<Entry> {
    runs.iter()
        .flat_map(|r| r.entries.iter().cloned())
        .collect()
}

/// A blob of any run in the chain, newest run first.
pub fn read_chain_blob(runs: &[Run], hash: &str) -> Option<String> {
    runs.iter().rev().find_map(|r| read_blob(&r.dir, hash).ok())
}

/// Reject a ledger written by a newer belay. No `format` entry means version 1.
fn check_format(first_line: &str, path: &Path) -> Result<()> {
    let Ok(v) = serde_json::from_str::<Value>(first_line) else {
        return Ok(());
    };
    if v["body"]["kind"] != "format" {
        return Ok(());
    }
    let version = v["body"]["version"].as_u64().unwrap_or(0);
    if version > u64::from(LEDGER_FORMAT) {
        bail!(
            "{} uses ledger format {version}, but this version of belay reads format {LEDGER_FORMAT} or older. Upgrade belay to read it.",
            path.display()
        );
    }
    Ok(())
}

/// A user's note on a session, an action, an output, or an entry (`belay flag`). Flags
/// are the review queue for Phase 1: what belay got wrong. They live next to the ledger,
/// not in it, so the controller stays the only writer of the ledger.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Flag {
    pub ts_ms: u64,
    /// `a-N`, `o-N`, an entry number, or `session`.
    pub target: String,
    pub note: String,
}

pub fn append_flag(dir: &Path, flag: &Flag) -> Result<()> {
    let mut line = serde_json::to_string(flag)?;
    line.push('\n');
    append_file(&dir.join("flags.jsonl"))?.write_all(line.as_bytes())?;
    Ok(())
}

/// Flags of a session, oldest first. A missing file means no flags.
pub fn read_flags(dir: &Path) -> Vec<Flag> {
    fs::read_to_string(dir.join("flags.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

pub fn read_blob(dir: &Path, hash: &str) -> Result<String> {
    // A blob name is a sha256 in hex: never a path (`../x`).
    if hash.is_empty() || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("not a blob hash: {hash}");
    }
    let p = dir.join("blobs").join(hash);
    fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(name: &str, first: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("belay-store-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let entry = r#"{"seq":1,"ts_ms":0,"body":{"kind":"event","event":{"type":"turn_ended"}}}"#;
        fs::write(d.join("ledger.jsonl"), format!("{first}\n{entry}\n")).unwrap();
        d
    }

    /// R0.6: a line torn inside a multi-byte character does not hide the rest.
    #[test]
    fn torn_utf8_line_is_skipped() {
        let d = ledger("utf8", "{\"seq\":0}");
        let mut b = fs::read(d.join("ledger.jsonl")).unwrap();
        b.extend_from_slice(b"{\"x\":\"\xe2\x82");
        fs::write(d.join("ledger.jsonl"), b).unwrap();
        assert_eq!(read_entries(&d).unwrap().len(), 1);
    }

    /// E8: a secret in an output never reaches the disk; the hash still names it.
    #[test]
    fn blobs_and_ledger_are_redacted() {
        let d = std::env::temp_dir().join(format!("belay-store-redact-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let mut store = Store::open(&d).unwrap();
        let content = "GITHUB_TOKEN=ghp_0123456789abcdefghijklmnopqrstuvwxyzAB\n".to_string();
        let hash = belay_core::text::sha256_hex(&content);
        let decision = Decision {
            blobs: vec![belay_core::Blob {
                hash: hash.clone(),
                content,
            }],
            ..Default::default()
        };
        store.persist(&decision).unwrap();
        let written = fs::read_to_string(d.join("blobs").join(&hash)).unwrap();
        let _ = fs::remove_dir_all(&d);
        assert_eq!(written, "GITHUB_TOKEN=[redacted]\n");
    }

    #[test]
    fn reads_current_and_unversioned_ledgers() {
        let current = format!(
            r#"{{"seq":0,"ts_ms":0,"body":{{"kind":"format","version":{LEDGER_FORMAT}}}}}"#
        );
        let d = ledger("current", &current);
        assert_eq!(read_entries(&d).unwrap().len(), 2);
        let old = r#"{"seq":0,"ts_ms":0,"body":{"kind":"event","event":{"type":"turn_ended"}}}"#;
        let d2 = ledger("old", old);
        assert_eq!(read_entries(&d2).unwrap().len(), 2);
        let _ = fs::remove_dir_all(d);
        let _ = fs::remove_dir_all(d2);
    }

    /// R0.6: a torn last line (crash) does not swallow the next entry.
    #[test]
    fn torn_last_line_does_not_swallow_the_next_entry() {
        let d = ledger(
            "torn",
            r#"{"seq":0,"ts_ms":0,"body":{"kind":"format","version":1}}"#,
        );
        let mut f = OpenOptions::new()
            .append(true)
            .open(d.join("ledger.jsonl"))
            .unwrap();
        f.write_all(br#"{"seq":2,"ts_ms":0,"bo"#).unwrap();
        drop(f);
        let mut store = Store::open(&d).unwrap();
        let entry: Entry = serde_json::from_str(
            r#"{"seq":3,"ts_ms":0,"body":{"kind":"event","event":{"type":"turn_ended"}}}"#,
        )
        .unwrap();
        store
            .persist(&Decision {
                entries: vec![entry],
                ..Default::default()
            })
            .unwrap();
        let seqs: Vec<u64> = read_entries(&d).unwrap().iter().map(|e| e.seq).collect();
        assert_eq!(seqs, vec![0, 1, 3], "torn line skipped, the new one kept");
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn rejects_newer_format_with_a_clear_message() {
        let d = ledger(
            "newer",
            r#"{"seq":0,"ts_ms":0,"body":{"kind":"format","version":99}}"#,
        );
        let err = read_entries(&d).unwrap_err().to_string();
        assert!(err.contains("ledger format 99"), "{err}");
        assert!(err.contains("Upgrade belay"), "{err}");
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn a_blob_name_is_never_a_path() {
        let d = ledger("blobpath", "{\"seq\":0}");
        assert!(read_blob(&d, "../ledger.jsonl").is_err());
        assert!(read_blob(&d, "").is_err());
    }
}
