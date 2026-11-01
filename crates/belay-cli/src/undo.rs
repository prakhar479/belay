//! Per-turn snapshots and `belay undo` (U6.2). A snapshot is a commit of the whole
//! working tree (tracked and untracked files, not ignored ones), built from a temporary
//! index and kept under `refs/belay/<session>/t-N`. Undo writes files back from one
//! through a temporary index too: the user's index, HEAD, stash and branches never
//! change. It covers what shell commands changed, which an agent's own rewind cannot.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::audit::{git, repo_root, worktree_commit};

/// One kept tree: `t-N` (turn N's end, `t-0` the start) or `u-N` (before undo N).
#[derive(Debug, Clone, PartialEq)]
pub struct Snap {
    pub id: String,
    pub commit: String,
    pub time_s: u64,
}

impl Snap {
    fn key(&self) -> (String, u64) {
        let (kind, n) = self.id.split_once('-').unwrap_or((&self.id, ""));
        (kind.to_string(), n.parse().unwrap_or(0))
    }
}

fn session_id(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn prefix(sid: &str) -> String {
    format!("refs/belay/{sid}/")
}

/// The session's snapshots, `t-*` in turn order, then `u-*`.
pub fn list(root: &Path, sid: &str) -> Result<Vec<Snap>> {
    let out = git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(committerdate:unix)",
            &prefix(sid),
        ],
        None,
    )?;
    let mut snaps: Vec<Snap> = out
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\0');
            let id = f.next()?.strip_prefix(&prefix(sid))?.to_string();
            Some(Snap {
                id,
                commit: f.next()?.to_string(),
                time_s: f.next()?.parse().unwrap_or(0),
            })
        })
        .collect();
    snaps.sort_by_key(|s| {
        let (kind, n) = s.key();
        (kind != "t", kind, n)
    });
    Ok(snaps)
}

/// Paths that differ between two tree-ishes, NUL-safe.
fn changed(root: &Path, from: &str, to: &str) -> Result<Vec<(char, String)>> {
    let out = git(
        root,
        &["diff", "--name-status", "-z", "--no-renames", from, to],
        None,
    )?;
    let mut f = out.split('\0').filter(|s| !s.is_empty());
    let mut v = Vec::new();
    while let (Some(status), Some(path)) = (f.next(), f.next()) {
        v.push((status.chars().next().unwrap_or('M'), path.to_string()));
    }
    Ok(v)
}

/// The ledger keeps this many paths per snapshot; the commit keeps them all.
const MAX_FILES: usize = 100;

/// What the daemon takes: the start's snapshot (only once per session) or a turn's.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum At {
    Start,
    TurnEnd,
}

/// Take a snapshot of the session's working tree in `cwd`. None outside git, or for a
/// start that already has one (a restarted daemon). Returns the id, the commit, and the
/// files changed since the previous snapshot. Runs after the hook's reply: git may take
/// a while in a large tree, and the shim must not wait (rule 7). `floor`: the first turn
/// number this session may use, after the runs it resumed (U8; 0 for a new one).
pub fn take(dir: &Path, cwd: &Path, at: At, floor: u64) -> Option<(String, String, Vec<String>)> {
    let root = repo_root(cwd).ok()?;
    let sid = session_id(dir);
    let snaps = list(&root, &sid).ok()?;
    let turns: Vec<&Snap> = snaps.iter().filter(|s| s.key().0 == "t").collect();
    let n = match at {
        At::Start if !turns.is_empty() => return None,
        At::Start => floor,
        At::TurnEnd => turns.last().map_or(1, |s| s.key().1 + 1).max(floor),
    };
    let id = format!("t-{n}");
    let parent = turns.last().map(|s| s.commit.as_str());
    let commit = worktree_commit(&root, dir, &format!("belay {sid} {id}"), parent).ok()?;
    git(
        &root,
        &["update-ref", &format!("{}{id}", prefix(&sid)), &commit],
        None,
    )
    .ok()?;
    let files = parent
        .and_then(|p| changed(&root, p, &commit).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|(_, p)| p)
        .take(MAX_FILES)
        .collect();
    Some((id, commit, files))
}

/// Delete a session's snapshot refs (`belay purge`, pruning), so they don't pile up.
/// Best effort: a repo that is gone has no refs to keep.
pub fn delete_refs(dir: &Path) {
    let Some(root) =
        crate::sessions::session_cwd(dir).and_then(|cwd| repo_root(Path::new(&cwd)).ok())
    else {
        return;
    };
    let sid = session_id(dir);
    if sid.is_empty() {
        return;
    }
    for s in list(&root, &sid).unwrap_or_default() {
        let _ = git(
            &root,
            &["update-ref", "-d", &format!("{}{}", prefix(&sid), s.id)],
            None,
        );
    }
}

/// `belay undo [ID] [--yes]` for session `dir`. Without an id, list the snapshots.
pub fn run(dir: &Path, id: Option<&str>, yes: bool) -> Result<i32> {
    crate::paths::users_only("`belay undo`")?;
    let dir = std::fs::canonicalize(dir).with_context(|| format!("open {}", dir.display()))?;
    let cwd = crate::sessions::session_cwd(&dir)
        .with_context(|| format!("session {} has no working directory", dir.display()))?;
    let root = repo_root(Path::new(&cwd)).with_context(|| {
        format!("{cwd} is not in a git repository: belay keeps no snapshots there")
    })?;
    let sid = session_id(&dir);
    // A resumed session's snapshots go on from the runs it continues (U8): list them all.
    let runs: Vec<PathBuf> = crate::store::read_chain(&dir)
        .map(|r| r.into_iter().map(|r| r.dir).collect())
        .unwrap_or_else(|_| vec![dir.clone()]);
    let mut snaps = Vec::new();
    for run in &runs {
        snaps.extend(list(&root, &session_id(run))?);
    }
    snaps.sort_by_key(|s| {
        let (kind, n) = s.key();
        (kind != "t", kind, n)
    });
    snaps.dedup_by(|a, b| a.id == b.id);
    if snaps.is_empty() {
        println!("Session {sid} has no snapshots.");
        return Ok(0);
    }
    let current = worktree_commit(
        &root,
        &dir,
        &format!("belay {sid} before undo to {}", id.unwrap_or("?")),
        None,
    )?;
    let Some(id) = id else {
        print!("{}", render_list(&root, &sid, &snaps, &current));
        return Ok(0);
    };
    let Some(snap) = snaps.iter().find(|s| s.id == id) else {
        bail!("{id} is not a snapshot of session {sid} (`belay undo --session {sid}` lists them)");
    };
    let diff = changed(&root, &current, &snap.commit)?;
    if diff.is_empty() {
        println!("Nothing to undo: the files already match {id}.");
        return Ok(0);
    }
    println!("Files to roll back to {id} ({}):", root.display());
    print!(
        "{}",
        git(&root, &["diff", "--stat", &current, &snap.commit], None)?
    );
    if !yes && !confirm("Roll these files back? [y/N] ")? {
        println!("Nothing changed.");
        return Ok(1);
    }
    let undo_n = snaps
        .iter()
        .filter(|s| s.key().0 == "u")
        .map(|s| s.key().1)
        .max()
        .unwrap_or(0)
        + 1;
    let keep = format!("u-{undo_n}");
    git(
        &root,
        &["update-ref", &format!("{}{keep}", prefix(&sid)), &current],
        None,
    )?;
    restore(&root, &dir, &snap.commit, &diff)?;
    let files: Vec<String> = diff.iter().map(|(_, p)| p.clone()).collect();
    record(&dir, id, &current, files.clone())?;
    println!(
        "Rolled {} file{} back to {id}. The files as they were are kept: `belay undo {keep}` brings them back.",
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    );
    Ok(0)
}

fn render_list(root: &Path, sid: &str, snaps: &[Snap], current: &str) -> String {
    use std::fmt::Write as _;
    let now_s = crate::paths::now_ms() / 1000;
    let mut out = format!("Snapshots of session {sid} ({}):\n", root.display());
    for s in snaps {
        let n = changed(root, &s.commit, current).map_or(0, |d| d.len());
        let what = match s.key().0.as_str() {
            "u" => " (before an undo)",
            _ if s.id == "t-0" => " (session start)",
            _ => "",
        };
        let _ = writeln!(
            out,
            "  {:<5} {:<12} {}{what}",
            s.id,
            crate::commons::ago(now_s.saturating_sub(s.time_s) * 1000),
            match n {
                0 => "same as now".to_string(),
                1 => "1 file differs from now".to_string(),
                n => format!("{n} files differ from now"),
            }
        );
    }
    out.push_str("Roll back with `belay undo <id>`.\n");
    out
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// Write `diff`'s paths back as they are in `commit`: files the snapshot lacks are
/// removed, the others written through a temporary index (`checkout-index`), so the
/// user's index is never read or written.
fn restore(root: &Path, scratch: &Path, commit: &str, diff: &[(char, String)]) -> Result<()> {
    for (status, path) in diff {
        if *status == 'D' {
            let p = root.join(path);
            match std::fs::remove_file(&p) {
                Ok(()) => remove_empty_parents(root, &p),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("remove {}", p.display())),
            }
        }
    }
    let back: Vec<&str> = diff
        .iter()
        .filter(|(s, _)| *s != 'D')
        .map(|(_, p)| p.as_str())
        .collect();
    if back.is_empty() {
        return Ok(());
    }
    let index = scratch.join(format!("undo-index-{}", std::process::id()));
    let result = git(root, &["read-tree", commit], Some(&index)).and_then(|_| {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["checkout-index", "-f", "-z", "--stdin"])
            .env("GIT_INDEX_FILE", &index)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("run git checkout-index")?;
        let mut stdin = child.stdin.take().context("git checkout-index stdin")?;
        for p in &back {
            stdin.write_all(p.as_bytes())?;
            stdin.write_all(b"\0")?;
        }
        drop(stdin);
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "git checkout-index: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    });
    let _ = std::fs::remove_file(&index);
    result
}

/// Directories left empty by a removed file go too, as git would leave them.
fn remove_empty_parents(root: &Path, file: &Path) {
    let mut p: Option<PathBuf> = file.parent().map(Path::to_path_buf);
    while let Some(d) = p {
        if d == root || !d.starts_with(root) || std::fs::remove_dir(&d).is_err() {
            break;
        }
        p = d.parent().map(Path::to_path_buf);
    }
}

/// Record the undo in the session's ledger: through its daemon when it runs (it owns
/// the ledger's numbering), else by a controller that goes on from the ledger's end.
fn record(dir: &Path, id: &str, before: &str, files: Vec<String>) -> Result<()> {
    if crate::sessions::is_live(dir) {
        let req = crate::ipc::Request::Undone {
            id: id.to_string(),
            before: before.to_string(),
            files,
            token: crate::paths::read_token(dir),
        };
        crate::ipc::send(
            &crate::paths::socket_path(dir),
            &req,
            std::time::Duration::from_secs(2),
        )
        .context("record the undo in the live session")?;
        return Ok(());
    }
    let next = crate::store::read_entries(dir)
        .ok()
        .and_then(|e| e.iter().map(|e| e.seq).max())
        .map_or(0, |s| s + 1);
    let mut c = belay_core::Controller::new(
        belay_core::Config::default(),
        belay_core::Capabilities::CLAUDE_CODE,
    );
    c.resume_ids(next, 1, 1);
    let d = c.undone(id, before, files, crate::paths::now_ms());
    crate::store::Store::open(dir)?.persist(&d)
}
