//! Memory v2 on disk, and `belay memory`.
//!
//! <home>/memory/user.jsonl              the user's cross-project memories
//! <home>/memory/project-<key>.jsonl     one project's (and its sessions') memories
//! <home>/memory/memory.lock, memory.next   the write lock; the highest `m-N` given
//!
//! Older stores (notes, learned fixes, findings, handoffs on ended cards) are read as
//! memories without being rewritten. `belay memory migrate` copies them in once; a copy
//! keeps the adapted id, so it shadows its source and is never copied twice.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use belay_core::commons::{Card, Finding};
use belay_core::learn::KnownFix;
use belay_core::memory::{self, Kind, Memory, Provenance, Query, Scope};
use belay_core::text::sha256_hex;

use crate::learnstore::Note;
use crate::paths;

/// Hits `belay memory search` shows, at most.
const MAX_HITS: usize = 10;
/// What `superseded_by` says for a memory the user dropped. It stays in the file so a
/// migrated copy keeps shadowing its source.
const DROPPED: &str = "dropped";

pub fn dir() -> PathBuf {
    paths::home().join("memory")
}

/// The file a memory lives in: the user's, or its project's (sessions' too).
fn file_for(dir: &Path, m: &Memory) -> PathBuf {
    match (&m.scope, &m.project) {
        (Scope::User, _) | (_, None) => dir.join("user.jsonl"),
        (_, Some(p)) => dir.join(format!("project-{}.jsonl", &sha256_hex(p)[..16])),
    }
}

/// Hold `<dir>/memory.lock` while `f` reads and writes the store.
fn with_lock<T>(dir: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    use std::os::unix::io::AsRawFd;
    paths::ensure_private_dir(dir)?;
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("memory.lock"))?;
    // SAFETY: flock on an open file; released when `lock` is dropped.
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    f()
}

/// The memories in one file. A line this build cannot read is skipped.
fn load_file(path: &Path) -> Vec<Memory> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    bytes
        .split(|&b| b == b'\n')
        .filter_map(|l| serde_json::from_slice::<Memory>(l).ok())
        .filter(|m| !m.id.is_empty())
        .collect()
}

/// The store's files, in name order.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n == "user.jsonl" || n.starts_with("project-") && n.ends_with(".jsonl")
            })
        })
        .collect();
    v.sort();
    v
}

/// Every native memory, superseded ones too.
pub fn load_native(dir: &Path) -> Vec<Memory> {
    files(dir).iter().flat_map(|p| load_file(p)).collect()
}

fn write_file(path: &Path, ms: &[Memory]) -> Result<()> {
    let text: String = ms
        .iter()
        .filter_map(|m| serde_json::to_string(m).ok())
        .map(|l| l + "\n")
        .collect();
    let tmp = path.with_extension(format!("jsonl.{}", std::process::id()));
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn append(path: &Path, ms: &[Memory]) -> Result<()> {
    let mut text = String::new();
    for m in ms {
        text.push_str(&serde_json::to_string(m)?);
        text.push('\n');
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(text.as_bytes())?;
    Ok(())
}

/// The next `m-N`: past every id ever given, also ones whose lines are gone.
fn next_id(dir: &Path, native: &[Memory]) -> String {
    let high: u64 = fs::read_to_string(dir.join("memory.next"))
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0);
    let n = native
        .iter()
        .filter_map(|m| m.id.strip_prefix("m-")?.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        .max(high)
        + 1;
    let _ = fs::write(dir.join("memory.next"), n.to_string());
    format!("m-{n}")
}

/// Consolidate one file after a write: duplicates and the project cap.
fn consolidate_file(path: &Path) -> Result<()> {
    let mut ms = load_file(path);
    let changes = memory::consolidate(&ms);
    if memory::apply(&mut ms, &changes) > 0 {
        write_file(path, &ms)?;
    }
    Ok(())
}

/// Add the memory `make(id)` builds. Returns it as kept.
pub fn add_in(dir: &Path, make: impl FnOnce(&str) -> Memory) -> Result<Memory> {
    with_lock(dir, || {
        let native = load_native(dir);
        let m = make(&next_id(dir, &native));
        let path = file_for(dir, &m);
        append(&path, std::slice::from_ref(&m))?;
        consolidate_file(&path)?;
        Ok(m)
    })
}

/// Change the native memories `ids` with `f`; returns those changed, as now.
fn update_in(dir: &Path, ids: &[String], mut f: impl FnMut(&mut Memory)) -> Result<Vec<Memory>> {
    with_lock(dir, || {
        let mut changed = Vec::new();
        for path in files(dir) {
            let mut ms = load_file(&path);
            let before = changed.len();
            for m in ms.iter_mut().filter(|m| ids.contains(&m.id)) {
                f(m);
                changed.push(m.clone());
            }
            if changed.len() > before {
                write_file(&path, &ms)?;
            }
        }
        Ok(changed)
    })
}

// --- Adapters: older stores read as memories ----------------------------------------

/// Project notes: pending ones are the agent's and wait for approval.
pub fn from_notes(notes: &[Note]) -> Vec<Memory> {
    notes
        .iter()
        .filter(|n| n.state != "dropped")
        .map(|n| {
            let prov = if n.by == "user" {
                Provenance::User
            } else {
                Provenance::Agent
            };
            let num = n.id.strip_prefix("n-").unwrap_or(&n.id);
            let mut m = Memory::new(
                &format!("note-{num}"),
                Scope::Project,
                Kind::Note,
                prov,
                Some(&n.project),
                &n.text,
                &[],
                &[],
                n.ts_ms,
            );
            m.approved = n.state == "approved";
            m
        })
        .collect()
}

/// Learned fixes: what belay saw, keyed by their failure signature.
pub fn from_fixes(fixes: &[KnownFix]) -> Vec<Memory> {
    fixes
        .iter()
        .map(|k| {
            let what = match (&k.test, k.message.is_empty()) {
                (Some(t), true) => t.clone(),
                _ => k.message.clone(),
            };
            let text = format!(
                "`{what}` was fixed by changes to {}; `{}` then passed (learned).",
                k.files.join(", "),
                k.command
            );
            let key = format!("{}|{}|{}|{}", k.signature, k.project, k.session, k.at_ms);
            let mut m = Memory::new(
                &format!("fix-{}", &sha256_hex(&key)[..10]),
                Scope::Project,
                Kind::Fix,
                Provenance::Observed,
                Some(&k.project),
                &text,
                &[],
                &k.files,
                k.at_ms,
            );
            m.signature = Some(k.signature.clone());
            m
        })
        .collect()
}

/// Findings keep their `f-N` ids.
pub fn from_findings(found: &[Finding]) -> Vec<Memory> {
    found
        .iter()
        .map(|f| {
            let kind = match f.kind.as_str() {
                "how" => Kind::Fact,
                k => Kind::parse(k).unwrap_or(Kind::Note),
            };
            let prov = if f.by == "user" {
                Provenance::User
            } else {
                Provenance::Agent
            };
            Memory::new(
                &f.id,
                Scope::Project,
                kind,
                prov,
                Some(&f.project),
                &f.text,
                &f.tags,
                &f.files,
                f.at_ms,
            )
        })
        .collect()
}

/// The handoffs ended sessions left on their cards.
pub fn from_cards(cards: &[(Card, bool)]) -> Vec<Memory> {
    cards
        .iter()
        .filter(|(c, live)| !*live || c.is_ended())
        .filter_map(|(c, _)| {
            let text = c.said.handoff.as_deref()?;
            let files: Vec<String> = c.files.iter().map(|(p, _)| p.clone()).collect();
            Some(Memory::new(
                &format!("h-{}", c.id),
                Scope::Project,
                Kind::Handoff,
                Provenance::Agent,
                Some(&c.repo),
                text,
                &[],
                &files,
                c.last_activity_ms,
            ))
        })
        .collect()
}

/// Everything the older stores hold, as memories.
fn adapted() -> Vec<Memory> {
    let mut v = from_notes(&crate::learnstore::load_notes());
    v.extend(from_fixes(&crate::learnstore::load_fixes("")));
    v.extend(from_findings(&crate::findings::load_in(
        &crate::findings::dir(),
    )));
    v.extend(from_cards(&crate::commons::read_cards(
        &paths::sessions_dir(),
    )));
    v
}

/// Native memories, then adapted ones no native copy shadows.
fn merged(native: Vec<Memory>, adapted: Vec<Memory>) -> Vec<Memory> {
    let ids: BTreeSet<String> = native.iter().map(|m| m.id.clone()).collect();
    let mut all = native;
    all.extend(adapted.into_iter().filter(|m| !ids.contains(&m.id)));
    all
}

// --- Commands --------------------------------------------------------------------------

fn here() -> Result<String> {
    Ok(crate::learnstore::project_of(&std::env::current_dir()?))
}

fn render(out: &mut String, m: &Memory, project: &str, now: u64) {
    let place = match &m.project {
        None => "yours".to_string(),
        Some(p) if p == project => "this project".to_string(),
        Some(p) => p.clone(),
    };
    let approved = if m.approved && m.provenance != Provenance::User {
        " (approved)"
    } else {
        ""
    };
    let _ = writeln!(
        out,
        "{} · {} · {place} · by {}{approved} · {}",
        m.id,
        m.kind.name(),
        m.provenance.name(),
        crate::findings::ago(now.saturating_sub(m.created_ms))
    );
    let _ = writeln!(out, "  {}", m.text);
    if !m.files.is_empty() {
        let _ = writeln!(out, "  files: {}", m.files.join(", "));
    }
    if !m.tags.is_empty() {
        let _ = writeln!(out, "  tags: {}", m.tags.join(", "));
    }
}

/// `belay memory list [--all]`: this project's and the user's, newest first.
pub fn list(all: bool) -> Result<String> {
    let project = here()?;
    let mut ms: Vec<Memory> = merged(load_native(&dir()), adapted())
        .into_iter()
        .filter(|m| m.is_live())
        .filter(|m| all || m.project.is_none() || m.project.as_deref() == Some(&project))
        .collect();
    ms.sort_by(|a, b| {
        b.created_ms
            .cmp(&a.created_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    if ms.is_empty() {
        return Ok("No memories here. `belay memory add \"…\"` keeps one.\n".into());
    }
    let now = paths::now_ms();
    let mut out = String::new();
    for m in &ms {
        render(&mut out, m, &project, now);
    }
    Ok(out)
}

/// `belay memory search <q>`: the best matches. A native hit counts as a use.
pub fn search(query: &str) -> Result<String> {
    search_top(query, MAX_HITS)
}

/// The best `limit` matches, rendered as `belay memory search` shows them.
pub fn search_top(query: &str, limit: usize) -> Result<String> {
    let project = here()?;
    search_in(&dir(), adapted(), query, &project, paths::now_ms(), limit)
}

fn search_in(
    dir: &Path,
    adapted: Vec<Memory>,
    query: &str,
    project: &str,
    now: u64,
    limit: usize,
) -> Result<String> {
    let all = merged(load_native(dir), adapted);
    let q = Query {
        text: query.to_string(),
        project: Some(project.to_string()),
        now_ms: now,
        ..Query::default()
    };
    let mut ranked = memory::rank(&all, &q);
    ranked.truncate(limit);
    if ranked.is_empty() {
        return Ok(format!("No memories about \"{query}\".\n"));
    }
    let ids: Vec<String> = ranked.iter().map(|(id, _)| id.clone()).collect();
    if ids.iter().any(|id| id.starts_with("m-")) {
        // A convenience: a failed count does not fail the search.
        let _ = update_in(dir, &ids, |m| {
            m.uses = m.uses.saturating_add(1);
            m.last_used_ms = now;
        });
    }
    let mut out = String::new();
    for id in &ids {
        if let Some(m) = all.iter().find(|m| &m.id == id) {
            render(&mut out, m, project, now);
        }
    }
    Ok(out)
}

/// Who is writing: the user at their terminal, else an agent.
fn provenance() -> Provenance {
    if paths::users_terminal() {
        Provenance::User
    } else {
        Provenance::Agent
    }
}

/// `belay memory add`.
pub fn add(
    text: &str,
    kind: Option<&str>,
    tags: &[String],
    files: &[String],
    scope: Option<&str>,
) -> Result<String> {
    if text.trim().is_empty() {
        bail!("a memory needs text");
    }
    let kind = match kind {
        None => Kind::Note,
        Some(k) => Kind::parse(k).ok_or_else(|| {
            let names: Vec<&str> = Kind::ALL.iter().map(|k| k.name()).collect();
            anyhow::anyhow!("unknown kind {k}: use one of {}", names.join(", "))
        })?,
    };
    let scope = match scope {
        None | Some("project") => Scope::Project,
        Some("user") => Scope::User,
        Some(s) => bail!("unknown scope {s}: use project or user"),
    };
    let project = match scope {
        Scope::User => None,
        _ => Some(here()?),
    };
    let prov = provenance();
    let m = add_in(&dir(), |id| {
        Memory::new(
            id,
            scope,
            kind,
            prov,
            project.as_deref(),
            text,
            tags,
            files,
            paths::now_ms(),
        )
    })?;
    Ok(match prov {
        Provenance::User => format!("Kept {}.", m.id),
        _ => format!(
            "Kept {} as the agent's. The user can `belay memory approve {}`.",
            m.id, m.id
        ),
    })
}

/// `belay memory show <id>`: every field.
pub fn show(id: &str) -> Result<String> {
    let all = merged(load_native(&dir()), adapted());
    let Some(m) = all.iter().find(|m| m.id == id) else {
        bail!("no memory {id}");
    };
    Ok(serde_json::to_string_pretty(m)? + "\n")
}

/// Where an adapted id comes from, for a review that must go to its own store.
enum Source {
    Native,
    Note(String),
    Finding,
    ReadOnly(&'static str),
}

fn source(dir: &Path, id: &str) -> Source {
    if load_native(dir).iter().any(|m| m.id == id) {
        return Source::Native;
    }
    if let Some(n) = id.strip_prefix("note-") {
        Source::Note(format!("n-{n}"))
    } else if id.starts_with("f-") {
        Source::Finding
    } else if id.starts_with("fix-") {
        Source::ReadOnly("a learned fix (`belay fixes forget` drops them)")
    } else if id.starts_with("h-") {
        Source::ReadOnly("a handoff on an ended session's card")
    } else {
        Source::ReadOnly("not a memory id")
    }
}

/// `belay memory approve <id>`: the user's.
pub fn approve(id: &str) -> Result<String> {
    paths::users_only("approving memories")?;
    let d = dir();
    match source(&d, id) {
        Source::Native => {
            update_in(&d, &[id.to_string()], |m| m.approved = true)?;
            Ok(format!("{id} is approved."))
        }
        Source::Note(n) => crate::learnstore::notes(Some(("approve", &n)), false),
        Source::Finding => {
            bail!("{id} is a finding: `belay memory migrate` first, then approve it")
        }
        Source::ReadOnly(what) => bail!("{id} is {what}: nothing to approve"),
    }
}

/// `belay memory drop <id>`: the user's.
pub fn drop(id: &str) -> Result<String> {
    paths::users_only("dropping memories")?;
    let d = dir();
    match source(&d, id) {
        Source::Native => {
            update_in(&d, &[id.to_string()], |m| {
                m.superseded_by = Some(DROPPED.into())
            })?;
            Ok(format!("Dropped {id}."))
        }
        Source::Note(n) => crate::learnstore::notes(Some(("drop", &n)), false),
        Source::Finding => crate::findings::drop_ids(&[id.to_string()]),
        Source::ReadOnly(what) => bail!("{id} is {what}: belay memory cannot drop it"),
    }
}

/// `belay memory migrate`: copy adapted memories in, once, after a backup of the old
/// files. The old stores stay as they are.
pub fn migrate() -> Result<String> {
    let home = paths::home();
    let old = [
        home.join("learn/notes.jsonl"),
        home.join("learn/fixes.jsonl"),
        crate::findings::dir().join("findings.jsonl"),
    ];
    migrate_in(&dir(), adapted(), &old, paths::now_ms())
}

fn migrate_in(dir: &Path, adapted: Vec<Memory>, old: &[PathBuf], now: u64) -> Result<String> {
    with_lock(dir, || {
        let have: BTreeSet<String> = load_native(dir).into_iter().map(|m| m.id).collect();
        let new: Vec<Memory> = adapted
            .into_iter()
            .filter(|m| !have.contains(&m.id))
            .collect();
        if new.is_empty() {
            return Ok("Nothing to migrate: every older memory is in the store.".into());
        }
        let backup = dir.join(format!("backup-{now}"));
        paths::ensure_private_dir(&backup)?;
        for f in old.iter().filter(|f| f.exists()) {
            if let Some(name) = f.file_name() {
                fs::copy(f, backup.join(name))?;
            }
        }
        let mut by_file: std::collections::BTreeMap<PathBuf, Vec<Memory>> = Default::default();
        for m in new.iter() {
            by_file.entry(file_for(dir, m)).or_default().push(m.clone());
        }
        for (path, ms) in &by_file {
            append(path, ms)?;
        }
        Ok(format!(
            "Copied {} memor{} into the store; the old files are backed up in {}.",
            new.len(),
            if new.len() == 1 { "y" } else { "ies" },
            backup.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("belay-memory-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn mk(id: &str, project: Option<&str>, text: &str) -> Memory {
        Memory::new(
            id,
            if project.is_some() {
                Scope::Project
            } else {
                Scope::User
            },
            Kind::Gotcha,
            Provenance::Agent,
            project,
            text,
            &[],
            &[],
            1,
        )
    }

    fn note(id: &str, text: &str, by: &str, state: &str) -> Note {
        Note {
            id: id.into(),
            project: "/r".into(),
            text: text.into(),
            by: by.into(),
            state: state.into(),
            ts_ms: 5,
        }
    }

    fn finding(id: &str, text: &str) -> Finding {
        belay_core::commons::clean_finding(
            id,
            Some("how"),
            text,
            &["pytest".into()],
            &[],
            None,
            "/r",
            "user",
            "",
            7,
        )
    }

    #[test]
    fn adds_go_to_their_scope_file_with_growing_ids_and_dedupe() {
        let d = tmp("add");
        let a = add_in(&d, |id| mk(id, Some("/r"), "asyncio needs a loop")).unwrap();
        let b = add_in(&d, |id| mk(id, None, "I like short names")).unwrap();
        assert_eq!((a.id.as_str(), b.id.as_str()), ("m-1", "m-2"));
        assert!(d.join("user.jsonl").exists());
        assert_eq!(files(&d).len(), 2);
        // The same text again: the older one is superseded.
        let mut again = mk("", Some("/r"), "Asyncio needs  a loop");
        again.created_ms = 2;
        let c = add_in(&d, |id| Memory {
            id: id.into(),
            ..again
        })
        .unwrap();
        let all = load_native(&d);
        let old = all.iter().find(|m| m.id == "m-1").unwrap();
        assert_eq!(old.superseded_by.as_deref(), Some(c.id.as_str()));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn adapters_present_notes_findings_fixes_and_handoffs() {
        let notes = from_notes(&[
            note("n-1", "use uv", "user", "approved"),
            note("n-2", "agent idea", "123-4", "pending"),
            note("n-3", "gone", "user", "dropped"),
        ]);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].id, "note-1");
        assert!(notes[0].approved && notes[0].provenance == Provenance::User);
        assert!(!notes[1].approved && notes[1].provenance == Provenance::Agent);

        let found = from_findings(&[finding("f-3", "pytest wants asyncio_mode")]);
        assert_eq!((found[0].id.as_str(), found[0].kind), ("f-3", Kind::Fact));
        assert_eq!(found[0].tags, ["pytest"]);

        let fix = KnownFix {
            signature: "sig".into(),
            message: "AssertionError".into(),
            test: None,
            command: "pytest".into(),
            files: vec!["a.py".into()],
            session: "s".into(),
            project: "/r".into(),
            at_ms: 9,
            change: None,
        };
        let fixes = from_fixes(std::slice::from_ref(&fix));
        assert!(fixes[0].id.starts_with("fix-") && fixes[0].id.len() == 14);
        assert_eq!(fixes[0].id, from_fixes(&[fix])[0].id, "stable");
        assert_eq!(fixes[0].signature.as_deref(), Some("sig"));
        assert_eq!(fixes[0].provenance, Provenance::Observed);
    }

    #[test]
    fn search_finds_adapted_and_native_and_counts_native_uses() {
        let d = tmp("search");
        add_in(&d, |id| mk(id, Some("/r"), "pytest needs DB_URL set")).unwrap();
        let adapted = from_findings(&[finding("f-1", "pytest asyncio mode")]);
        let out = search_in(&d, adapted, "pytest", "/r", 100, MAX_HITS).unwrap();
        assert!(out.contains("m-1 · gotcha · this project"), "{out}");
        assert!(out.contains("f-1 · fact · this project · by user"), "{out}");
        let m = &load_native(&d)[0];
        assert_eq!((m.uses, m.last_used_ms), (1, 100));
        assert!(search_in(&d, vec![], "cargo", "/r", 100, MAX_HITS)
            .unwrap()
            .starts_with("No memories"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn migrate_copies_once_with_a_backup_and_shadows_the_source() {
        let d = tmp("migrate");
        let old = d.join("old/notes.jsonl");
        fs::create_dir_all(d.join("old")).unwrap();
        fs::write(&old, "{}\n").unwrap();
        let adapted = || {
            let mut v = from_notes(&[note("n-1", "use uv", "user", "approved")]);
            v.extend(from_findings(&[finding("f-2", "pytest x")]));
            v
        };
        let said = migrate_in(&d, adapted(), std::slice::from_ref(&old), 1).unwrap();
        assert!(said.starts_with("Copied 2 memories"), "{said}");
        assert!(d.join("backup-1/notes.jsonl").exists());
        let said = migrate_in(&d, adapted(), &[old], 2).unwrap();
        assert!(said.starts_with("Nothing to migrate"), "{said}");
        assert!(!d.join("backup-2").exists());
        let all = merged(load_native(&d), adapted());
        assert_eq!(all.len(), 2, "copies shadow their sources");
        let _ = fs::remove_dir_all(&d);
    }
}
