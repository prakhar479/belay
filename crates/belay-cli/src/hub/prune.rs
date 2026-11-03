//! `belay fleet prune`: removing worktrees and branches whose work is already in.

use super::*;

/// Whether everything `wt` changed since `base` is already in `repo`'s HEAD: each path
/// it changed (commits, uncommitted edits, new files) is at HEAD with the same bytes,
/// or gone from both. No patch matching (a hunk could match at another place in a
/// repetitive file); a line-ending or filter difference keeps the worktree (the safe
/// side). Nothing is changed.
pub(super) fn changes_in_head(repo: &Path, wt: &Path, base: &str) -> bool {
    let list = |args: &[&str]| -> Option<Vec<String>> {
        let o = Command::new("git")
            .arg("-C")
            .arg(wt)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        Some(
            o.stdout
                .split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .map(|p| String::from_utf8_lossy(p).to_string())
                .collect(),
        )
    };
    let Some(mut paths) = list(&["diff", "--name-only", "-z", "--no-renames", base]) else {
        return false;
    };
    let Some(new) = list(&["ls-files", "-z", "--others", "--exclude-standard"]) else {
        return false;
    };
    paths.extend(new);
    paths
        .iter()
        .all(|p| crate::audit::same_as_head(repo, wt, p))
}

/// Whether merging the worktree as it is now (its commits and uncommitted changes) into
/// the repo's HEAD would change nothing. That is so when the work was taken another way
/// (cherry-picked, applied as a patch) and HEAD has moved on since, which a file-by-file
/// comparison with HEAD misses. The snapshot commit is on no ref; git collects it later.
pub(super) fn merges_to_nothing(repo: &Path, wt: &Path) -> bool {
    // The temporary index lists the repo's files: belay's private home, made if missing
    // (a fresh machine, or CI, has none yet).
    let scratch = paths::home();
    if paths::ensure_private_dir(&scratch).is_err() {
        return false;
    }
    let Ok(snap) = crate::audit::worktree_commit(wt, &scratch, "belay prune check", Some("HEAD"))
    else {
        return false;
    };
    let (Some(merged), Some(head)) = (
        git_out(repo, &["merge-tree", "--write-tree", "HEAD", &snap]),
        git_out(repo, &["rev-parse", "HEAD^{tree}"]),
    ) else {
        return false;
    };
    merged.lines().next() == Some(head.as_str())
}

/// `belay fleet prune`: worktrees of fleets that are down, with no uncommitted changes
/// and no commits beyond the fleet's base. Lists them; removes them with `yes`. With
/// `branches`, also their fleets' merged branches.
pub fn prune(yes: bool, branches: bool) -> Result<String> {
    Ok(prune_dirs(&fleet_dirs(), yes, branches))
}

pub(super) fn prune_dirs(dirs: &[PathBuf], yes: bool, branches: bool) -> String {
    let mut out = String::new();
    let mut counts = (0, 0);
    let mut down = Vec::new();
    for d in dirs {
        if is_up(d) {
            continue;
        }
        let Ok(meta) = read_meta(d) else { continue };
        if let Some(repo) = &meta.repo {
            down.push((meta.id.clone(), PathBuf::from(repo)));
        }
        prune_fleet(d, &meta, yes, &|_| None, &mut out, &mut counts);
    }
    let (removed, kept) = counts;
    let _ = writeln!(
        out,
        "{} {removed} worktree{}, kept {kept}.{}",
        if yes { "Removed" } else { "Would remove" },
        if removed == 1 { "" } else { "s" },
        if yes || removed == 0 {
            ""
        } else {
            " Run `belay fleet prune --yes` to remove them."
        }
    );
    if branches {
        prune_branches(&down, yes, &mut out);
    }
    out
}

/// The worktrees of one fleet that is down: each is removed (with `yes`) when it is
/// clean and has no commits beyond the base, or when all it changed is in the repo's
/// HEAD; otherwise kept, with why. `hold` gives a worktree (by its name) another reason
/// to keep it. A line per worktree goes to `out`; `counts` is (removed, kept). A build
/// dir inside the worktree goes with it.
pub(super) fn prune_fleet(
    d: &Path,
    meta: &Meta,
    yes: bool,
    hold: &dyn Fn(&str) -> Option<String>,
    out: &mut String,
    counts: &mut (usize, usize),
) {
    let (removed, kept) = (&mut counts.0, &mut counts.1);
    let (Some(repo), Some(base)) = (&meta.repo, &meta.base) else {
        return;
    };
    let repo = Path::new(repo);
    let mut wts: Vec<PathBuf> = fs::read_dir(d.join("worktrees"))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    wts.sort();
    {
        for wt in wts {
            let name = wt
                .file_name()
                .map(|n| n.to_string_lossy().replace("__", "/"))
                .unwrap_or_default();
            let branch = branch_name(&meta.id, &name);
            if let Some(w) = hold(&name) {
                *kept += 1;
                let _ = writeln!(out, "keep   {} ({w})", wt.display());
                continue;
            }
            let dirty = git_out(&wt, &["status", "--porcelain"]).is_some();
            let count = |range: String, at: &Path| {
                git_out(at, &["rev-list", "--count", &range])
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(0)
            };
            // Its branch, or a detached HEAD in it, past the base: unmerged commits.
            let ahead =
                count(format!("{base}..{branch}"), repo).max(count(format!("{base}..HEAD"), &wt));
            // Work the user already took (a patch applied, a merge) is not lost.
            let taken = (dirty || ahead > 0)
                && (changes_in_head(repo, &wt, base) || merges_to_nothing(repo, &wt));
            let why = if taken {
                None
            } else if dirty {
                Some("it has uncommitted changes".to_string())
            } else if ahead > 0 {
                Some(format!("{branch} has {ahead} commit(s) not in the base"))
            } else {
                None
            };
            let note = if taken {
                " (its changes are in HEAD)"
            } else {
                ""
            };
            match why {
                Some(w) => {
                    *kept += 1;
                    let _ = writeln!(out, "keep   {} ({w})", wt.display());
                }
                None if yes => {
                    let ok = Command::new("git")
                        .arg("-C")
                        .arg(repo)
                        .args(["worktree", "remove"])
                        .args(taken.then_some("--force"))
                        .arg(&wt)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success());
                    if ok {
                        let _ = git_out(repo, &["branch", "-D", &branch]);
                        *removed += 1;
                        let _ = writeln!(out, "removed {}{note}", wt.display());
                    } else {
                        *kept += 1;
                        let _ =
                            writeln!(out, "keep   {} (git worktree remove failed)", wt.display());
                    }
                }
                None => {
                    *removed += 1;
                    let _ = writeln!(out, "would remove {}{note}", wt.display());
                }
            }
        }
    }
}

/// At `belay fleet down`, once the hub is gone: remove the fleet's worktrees that
/// `belay fleet prune` would, except where a task is not finished (done or dropped).
/// Lists what it removed and what it kept.
pub(super) fn prune_at_down(dir: &Path) -> String {
    use belay_core::fleet::model::TaskState;
    let Ok(meta) = read_meta(dir) else {
        return String::new();
    };
    let model = replay(&meta, &read_ledger(dir));
    // Worktree name → a task in it that is not finished.
    let mut open: BTreeMap<String, String> = BTreeMap::new();
    for t in model.tasks() {
        if matches!(t.state, TaskState::Done | TaskState::Dropped) {
            continue;
        }
        // Where its session works, or where its last attempt stopped (BG0).
        let name = match &t.slot {
            Some(slot) if model.role_of(slot).is_some() => model.worktree_name(slot),
            Some(slot) => Some(slot.clone()),
            None => t.left_in.clone(),
        };
        if let Some(name) = name {
            open.entry(name).or_insert_with(|| t.id.clone());
        }
    }
    let hold = |name: &str| open.get(name).map(|id| format!("task {id} is not done"));
    let mut out = String::new();
    let mut counts = (0, 0);
    prune_fleet(dir, &meta, true, &hold, &mut out, &mut counts);
    if counts == (0, 0) {
        return String::new();
    }
    let _ = writeln!(
        out,
        "Removed {} worktree{}, kept {}.{}",
        counts.0,
        if counts.0 == 1 { "" } else { "s" },
        counts.1,
        if counts.1 == 0 {
            ""
        } else {
            " Merge their work, then `belay fleet prune --yes`."
        }
    );
    out
}

/// The `belay/<fleet>/*` branches of fleets that are down (worktrees go first, so a
/// branch is no longer checked out): one whose tip is in the repo's HEAD is deleted with
/// `git branch -d`, never `-D`; one with commits HEAD lacks is listed and kept.
/// Sandbox branches (`belay/sandbox/*`) are not a fleet's and are never touched.
pub(super) fn prune_branches(down: &[(String, PathBuf)], yes: bool, out: &mut String) {
    let (mut deleted, mut kept) = (0, 0);
    for (id, repo) in down {
        let refs = git_out(
            repo,
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                &format!("refs/heads/belay/{id}/"),
            ],
        )
        .unwrap_or_default();
        for branch in refs.lines().map(str::trim).filter(|b| !b.is_empty()) {
            if branch.starts_with("belay/sandbox/") {
                continue;
            }
            let merged = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["merge-base", "--is-ancestor", branch, "HEAD"])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if !merged {
                let ahead = git_out(repo, &["rev-list", "--count", &format!("HEAD..{branch}")])
                    .and_then(|n| n.parse::<u64>().ok())
                    .unwrap_or(0);
                kept += 1;
                let _ = writeln!(
                    out,
                    "keep   branch {branch} ({ahead} commit{} not in HEAD)",
                    if ahead == 1 { "" } else { "s" }
                );
            } else if !yes {
                deleted += 1;
                let _ = writeln!(out, "would delete branch {branch} (merged)");
            } else if git_ok(repo, &["branch", "-d", branch]) {
                deleted += 1;
                let _ = writeln!(out, "deleted branch {branch} (merged)");
            } else {
                // Checked out in a worktree that was kept, or git refused: it stays.
                kept += 1;
                let _ = writeln!(out, "keep   branch {branch} (git branch -d failed)");
            }
        }
    }
    let _ = writeln!(
        out,
        "{} {deleted} merged branch{}, kept {kept}.{}",
        if yes { "Deleted" } else { "Would delete" },
        if deleted == 1 { "" } else { "es" },
        if yes || deleted == 0 {
            ""
        } else {
            " Run `belay fleet prune --branches --yes` to delete them."
        }
    );
}

pub(super) fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
