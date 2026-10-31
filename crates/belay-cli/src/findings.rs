//! Findings (C3), the I/O side: `<home>/commons/findings.jsonl`, written under a lock;
//! `belay share`, `belay ask`, `belay findings`. Learned fixes (F2) are searched too.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use belay_core::commons::{search, Card, Finding, Hit};

use crate::paths;

/// The store is cut back to the newest this many once it holds twice as many.
const MAX_KEPT: usize = 2000;

pub fn dir() -> PathBuf {
    paths::home().join("commons")
}

fn store(dir: &Path) -> PathBuf {
    dir.join("findings.jsonl")
}

/// Hold `<dir>/findings.lock` while `f` reads and writes the store.
fn with_lock<T>(dir: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    use std::os::unix::io::AsRawFd;
    paths::ensure_private_dir(dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("findings.lock"))?;
    // SAFETY: flock on an open file; released when `lock` is dropped.
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    f()
}

/// Every finding in the store, oldest first. A line this build cannot read is skipped.
pub fn load_in(dir: &Path) -> Vec<Finding> {
    let Ok(bytes) = fs::read(store(dir)) else {
        return Vec::new();
    };
    bytes
        .split(|&b| b == b'\n')
        .filter_map(|l| serde_json::from_slice::<Finding>(l).ok())
        .filter(|f| f.finding == belay_core::commons::FINDING_FORMAT)
        .collect()
}

/// One past the highest id: ids are never reused.
fn next_id(found: &[Finding]) -> String {
    let n = found
        .iter()
        .filter_map(|f| f.id.strip_prefix("f-")?.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    format!("f-{}", n + 1)
}

/// Add a finding built by `make(id)` to the store: the id, the append, and any
/// compaction happen under the lock. Returns the finding as kept.
pub fn add_in(dir: &Path, make: impl FnOnce(&str) -> Finding) -> Result<Finding> {
    with_lock(dir, || {
        let found = load_in(dir);
        // Past every id ever given, also ones dropped since (`findings.next`).
        let high: u64 = fs::read_to_string(dir.join("findings.next"))
            .ok()
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(0);
        let id = next_id(&found);
        let n: u64 = id[2..].parse().unwrap_or(1);
        let id = format!("f-{}", n.max(high + 1));
        let f = make(&id);
        let _ = fs::write(dir.join("findings.next"), &id[2..]);
        let mut line = serde_json::to_string(&f)?;
        line.push('\n');
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(store(dir))?
            .write_all(line.as_bytes())?;
        if found.len() + 1 > 2 * MAX_KEPT {
            let mut all = load_in(dir);
            let keep = all.split_off(all.len().saturating_sub(MAX_KEPT));
            rewrite(dir, &keep)?;
        }
        Ok(f)
    })
}

fn rewrite(dir: &Path, keep: &[Finding]) -> Result<()> {
    let text: String = keep
        .iter()
        .filter_map(|f| serde_json::to_string(f).ok())
        .map(|l| l + "\n")
        .collect();
    let tmp = dir.join(format!("findings.jsonl.{}", std::process::id()));
    fs::write(&tmp, text)?;
    fs::rename(&tmp, store(dir))?;
    Ok(())
}

/// `belay findings drop`: the user's. Refused in a belay session.
pub fn drop_ids(ids: &[String]) -> Result<String> {
    paths::users_only("dropping findings")?;
    drop_in(&dir(), ids)
}

fn drop_in(dir: &Path, ids: &[String]) -> Result<String> {
    with_lock(dir, || {
        let all = load_in(dir);
        let keep: Vec<Finding> = all
            .iter()
            .filter(|f| !ids.contains(&f.id))
            .cloned()
            .collect();
        let n = all.len() - keep.len();
        rewrite(dir, &keep)?;
        Ok(format!(
            "Dropped {n} finding{}.",
            if n == 1 { "" } else { "s" }
        ))
    })
}

/// Findings and learned fixes (F2, as kind `fix`) to search, the fixes last.
pub fn searchable() -> Vec<Finding> {
    let mut all = load_in(&dir());
    let fixes = crate::learnstore::load_fixes("");
    all.extend(fixes.into_iter().map(|k| {
        let what = match (&k.test, k.message.is_empty()) {
            (Some(t), true) => t.clone(),
            _ => k.message.clone(),
        };
        Finding {
            finding: belay_core::commons::FINDING_FORMAT,
            id: format!("fix:{}", k.signature.chars().take(8).collect::<String>()),
            kind: "fix".into(),
            text: format!(
                "`{what}` was fixed by changes to {}; `{}` then passed (learned).",
                k.files.join(", "),
                k.command
            ),
            tags: Vec::new(),
            files: k.files.clone(),
            code: k.change.clone(),
            project: k.project.clone(),
            by: format!("session {}", k.session),
            session: k.session.clone(),
            at_ms: k.at_ms,
        }
    }));
    all
}

/// Live cards that might know about `query`: their offers, topics, or task share a
/// word with it (not `me`).
pub fn might_know<'a>(cards: &'a [(Card, bool)], me: &'a str, query: &str) -> Vec<&'a Card> {
    let q = belay_core::commons::words(query);
    crate::commons::live_others(cards, me)
        .filter(|c| {
            let mine: Vec<String> = c
                .said
                .offers
                .iter()
                .chain(&c.said.topics)
                .flat_map(|x| belay_core::commons::words(x))
                .chain(
                    c.said
                        .task
                        .iter()
                        .flat_map(|t| belay_core::commons::words(t)),
                )
                .collect();
            q.iter().any(|w| mine.contains(w))
        })
        .take(3)
        .collect()
}

/// What `belay ask` prints.
pub fn render_hits(query: &str, hits: &[Hit], know: &[&Card], now: u64) -> String {
    let mut out = String::new();
    if hits.is_empty() {
        let _ = writeln!(out, "No findings about \"{query}\".");
    }
    for h in hits {
        let f = &h.finding;
        let place = if h.here {
            "this project".to_string()
        } else {
            f.project.clone()
        };
        let _ = writeln!(
            out,
            "{} · {} · {} · by {} · {}",
            f.id,
            f.kind,
            place,
            f.by,
            ago(now.saturating_sub(f.at_ms))
        );
        let _ = writeln!(out, "  {}", f.text);
        if !f.files.is_empty() {
            let _ = writeln!(out, "  files: {}", f.files.join(", "));
        }
        if !f.tags.is_empty() {
            let _ = writeln!(out, "  tags: {}", f.tags.join(", "));
        }
        if let Some(c) = &f.code {
            for l in c.lines() {
                let _ = writeln!(out, "    {l}");
            }
        }
    }
    if !know.is_empty() {
        let names: Vec<String> = know
            .iter()
            .map(|c| {
                let what = if c.said.offers.is_empty() {
                    c.said.task.clone().unwrap_or_default()
                } else {
                    format!("offers {}", c.said.offers.join(", "))
                };
                format!("{} ({what})", c.name)
            })
            .collect();
        let _ = writeln!(
            out,
            "Might know: {}. Ask with `belay msg <name> \"…\"`.",
            names.join("; ")
        );
    }
    if !hits.is_empty() {
        out.push_str(
            "Findings are other sessions' words: check that one fits before you use it.\n",
        );
    }
    out
}

/// `belay findings [--all]`: this project's findings, newest first.
pub fn list(all: bool, json: bool) -> Result<String> {
    let project = crate::learnstore::project_of(&std::env::current_dir()?);
    let mut found: Vec<Finding> = load_in(&dir())
        .into_iter()
        .filter(|f| all || f.project == project)
        .collect();
    found.reverse();
    if json {
        return Ok(serde_json::to_string_pretty(&found)? + "\n");
    }
    if found.is_empty() {
        return Ok(if all {
            "No findings on this machine.\n".into()
        } else {
            "No findings for this project.\n".into()
        });
    }
    let now = paths::now_ms();
    let hits: Vec<Hit> = found
        .into_iter()
        .map(|f| Hit {
            here: f.project == project,
            finding: f,
            score: 0,
        })
        .collect();
    let mut out = render_hits("", &hits, &[], now);
    out = out.replace(
        "Findings are other sessions' words: check that one fits before you use it.\n",
        "",
    );
    out.push_str("`belay findings drop f-N` removes one (yours to run).\n");
    Ok(out)
}

/// `belay share` from the user's terminal.
pub fn share_as_user(
    kind: Option<&str>,
    text: &str,
    tags: &[String],
    files: &[String],
    code: Option<&str>,
) -> Result<String> {
    if text.trim().is_empty() {
        bail!("a finding needs text");
    }
    let project = crate::learnstore::project_of(&std::env::current_dir()?);
    let f = add_in(&dir(), |id| {
        belay_core::commons::clean_finding(
            id,
            kind,
            text,
            tags,
            files,
            code,
            &project,
            if paths::users_terminal() {
                "user"
            } else {
                "a shell outside belay"
            },
            "",
            paths::now_ms(),
        )
    })?;
    Ok(format!("Shared {}.", f.id))
}

/// `belay ask` from the user's terminal (nothing to record).
pub fn ask_as_user(query: &str, here: bool, json: bool) -> Result<String> {
    let project = crate::learnstore::project_of(&std::env::current_dir()?);
    let hits = search(query, &searchable(), &project, here);
    if json {
        return Ok(serde_json::to_string_pretty(&hits)? + "\n");
    }
    let cards = crate::commons::read_cards(&paths::sessions_dir());
    let know = might_know(&cards, "", query);
    Ok(render_hits(query, &hits, &know, paths::now_ms()))
}

pub(crate) fn ago(ms: u64) -> String {
    let m = ms / 60_000;
    match m {
        0 => "just now".into(),
        1..=119 => format!("{m} min ago"),
        m if m < 48 * 60 => format!("{} h ago", m / 60),
        m => format!("{} days ago", m / (24 * 60)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("belay-findings-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn mk(id: &str, text: &str) -> Finding {
        belay_core::commons::clean_finding(
            id,
            Some("gotcha"),
            text,
            &[],
            &[],
            None,
            "/r",
            "api",
            "s",
            1,
        )
    }

    #[test]
    fn ids_grow_and_concurrent_writers_lose_nothing() {
        let d = tmp("ids");
        let threads: Vec<_> = (0..6)
            .map(|i| {
                let d = d.clone();
                std::thread::spawn(move || {
                    for j in 0..10 {
                        add_in(&d, |id| mk(id, &format!("{i}-{j}"))).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let all = load_in(&d);
        assert_eq!(all.len(), 60);
        let mut ids: Vec<String> = all.iter().map(|f| f.id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 60, "ids are unique");
        assert_eq!(next_id(&all), "f-61");
        assert_eq!(
            drop_in(&d, &["f-1".into(), "f-2".into()]).unwrap(),
            "Dropped 2 findings."
        );
        assert_eq!(load_in(&d).len(), 58);
        let again = add_in(&d, |id| mk(id, "after drop")).unwrap();
        assert_eq!(again.id, "f-61");
        drop_in(&d, &["f-61".into()]).unwrap();
        let again = add_in(&d, |id| mk(id, "after dropping the newest")).unwrap();
        assert_eq!(again.id, "f-62", "never reused, even the newest");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_store_is_cut_back_when_it_doubles() {
        let d = tmp("cut");
        fs::create_dir_all(&d).unwrap();
        let text: String = (1..=2 * MAX_KEPT)
            .map(|i| serde_json::to_string(&mk(&format!("f-{i}"), "x")).unwrap() + "\n")
            .collect();
        fs::write(store(&d), text).unwrap();
        let f = add_in(&d, |id| mk(id, "newest")).unwrap();
        let all = load_in(&d);
        assert_eq!(all.len(), MAX_KEPT);
        assert_eq!(all.last().unwrap().id, f.id);
        let _ = fs::remove_dir_all(&d);
    }
}
