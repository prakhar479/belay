//! The hub's own work on worktrees: snapshots, left-work notes, moving work between a worktree and the repo.

use super::*;

impl Hub {
    /// Write a record of the hub's own work on worktrees (rule 4). Returns its seq.
    pub(super) fn write_work(&mut self, work: Work, reason: String, causes: Vec<u64>) -> u64 {
        let r = self.model.record_work(work, reason, causes, now_ms());
        if let Ok(line) = serde_json::to_string(&r) {
            let _ = writeln!(self.ledger, "{line}");
        }
        r.seq
    }

    /// Keep each newly done task's tree: the one taken before its check ran, or, for a
    /// task done without a check, its worktree now. `cause` is the step that did it.
    pub(super) fn snapshot_done(&mut self, cause: u64) {
        use belay_core::fleet::model::TaskState;
        let done: Vec<(String, Option<String>)> = self
            .model
            .tasks()
            .filter(|t| t.state == TaskState::Done && !self.done_trees.contains_key(&t.id))
            .map(|t| (t.id.clone(), t.slot.clone()))
            .collect();
        for (tid, slot) in done {
            let tree = self.pending_trees.remove(&tid).or_else(|| {
                let wt = slot
                    .as_ref()
                    .and_then(|s| self.worktrees.get(s).cloned().or_else(|| self.slot_dir(s)))?;
                let root = crate::audit::repo_root(&wt).ok()?;
                let scratch = self.dir.join("scratch");
                let _ = paths::ensure_private_dir(&scratch);
                crate::audit::worktree_tree(&root, &scratch).ok()
            });
            let Some(tree) = tree else {
                // No worktree to keep (a shared checkout, a slot that is gone): its
                // dependents take the worktree as it is, as before.
                self.done_trees.insert(tid, (String::new(), cause));
                continue;
            };
            let reason = format!(
                "{tid} is done: belay keeps its worktree as it was, for the tasks that depend on it."
            );
            let seq = self.write_work(
                Work::Snapshot {
                    task: tid.clone(),
                    tree: tree.clone(),
                },
                reason,
                vec![cause],
            );
            self.done_trees.insert(tid, (tree, seq));
        }
    }

    /// A task's next attempt starts in the worktree where the last one stopped (BG0):
    /// say so, when that attempt left changes there, and record it (rule 4).
    pub(super) fn left_work_note(&mut self, slot: &str, text: &str) -> Option<String> {
        let tid = text.strip_prefix("Task ")?.split(':').next()?.to_string();
        let s = self.model.slots().get(slot)?;
        let from = s.worktree_of.clone()?;
        if s.task.as_deref() != Some(tid.as_str()) {
            return None;
        }
        let wt = self.worktrees.get(slot)?.clone();
        let base = self.meta.base.clone()?;
        let dirty = git_out(&wt, &["status", "--porcelain"]).is_some();
        let ahead = git_out(&wt, &["rev-list", "--count", &format!("{base}..HEAD")])
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(0);
        let partial = dirty || ahead > 0;
        let reason = if partial {
            format!("{tid}'s last attempt stopped in {from}'s worktree before it was done and left changes; {slot} goes on from them.")
        } else {
            format!("{tid}'s last attempt stopped in {from}'s worktree and left no changes; {slot} starts there.")
        };
        let cause = self.running;
        self.write_work(
            Work::Resumed {
                task: tid.clone(),
                slot: slot.to_string(),
                worktree: from,
                partial,
            },
            reason,
            vec![cause],
        );
        partial.then(|| {
            "An earlier attempt at this task stopped before it was done (a usage limit, a crash, or a hub restart). Its partial, unchecked work is in this worktree: `git status` and `git log` show it. Review it, keep what is right, and finish the task.".to_string()
        })
    }

    /// A worktree lacks the work of the tasks its task depends on (a fresh worker starts
    /// at the fleet's base; a running sub-lead has only its own area): bring their
    /// changes in (3-way merge, new files copied) before the task, each task's once. The note says what came in, or what
    /// could not. A judge's parts are not merged: it compares them.
    pub(super) fn bring_in_dependencies(&mut self, slot: &str, text: &str) -> Option<String> {
        let tid = text.strip_prefix("Task ")?.split(':').next()?.to_string();
        let task = self.model.tasks().find(|t| t.id == tid)?.clone();
        let holds = self
            .model
            .slots()
            .get(slot)
            .is_some_and(|s| s.task.as_deref() == Some(tid.as_str()));
        if !holds || task.depends_on.is_empty() {
            return None;
        }
        if self.model.role_of(slot).map(|r| &r.workspace) != Some(&Workspace::WorktreePerWorker) {
            return None;
        }
        let target = self.worktrees.get(slot)?.clone();
        // Its dependencies and theirs, each once, the earliest first.
        let mut want: Vec<String> = Vec::new();
        let mut stack: Vec<String> = task.depends_on.clone();
        while let Some(d) = stack.pop() {
            if want.contains(&d) {
                continue;
            }
            if let Some(dt) = self.model.tasks().find(|t| t.id == d) {
                // Its own parts come up on their own; its own earlier work is there.
                if dt.parent.as_deref() == Some(tid.as_str()) || dt.slot.as_deref() == Some(slot) {
                    continue;
                }
                // A best-of task's work is its winner's, not every candidate's.
                if dt.best_of {
                    if let Some(win) = self
                        .model
                        .tasks()
                        .find(|c| c.parent.as_deref() == Some(d.as_str()) && c.picked == Some(true))
                    {
                        stack.push(win.id.clone());
                    }
                    continue;
                }
                stack.extend(dt.depends_on.clone());
                want.push(d);
            }
        }
        let order: Vec<String> = self
            .model
            .tasks()
            .filter(|t| want.contains(&t.id))
            .filter(|t| !self.integrated.contains(&(slot.to_string(), t.id.clone())))
            .map(|t| t.id.clone())
            .collect();
        let (mut brought, mut failed) = (Vec::new(), Vec::new());
        let mut conflicted: Vec<String> = Vec::new();
        for dep in order {
            let moved = self.move_work(&dep, &target, false);
            // A move that failed is tried again for this slot's next task.
            if moved.is_ok() {
                self.integrated.insert((slot.to_string(), dep.clone()));
            }
            let causes: Vec<u64> = self
                .done_trees
                .get(&dep)
                .map(|(_, s)| *s)
                .into_iter()
                .collect();
            match &moved {
                Ok(m) if m.moved => {
                    let reason =
                        format!("{tid} depends on {dep}: its work came into {slot}'s worktree.");
                    self.write_work(
                        Work::Brought {
                            task: dep.clone(),
                            slot: slot.to_string(),
                            conflicts: m.conflicts.clone(),
                        },
                        reason,
                        causes,
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    let reason = format!("{tid} depends on {dep}, whose work could not be moved.");
                    self.write_work(
                        Work::MoveFailed {
                            task: dep.clone(),
                            slot: slot.to_string(),
                            up: false,
                            error: e.clone(),
                        },
                        reason,
                        causes,
                    );
                }
            }
            match moved {
                Ok(m) if m.conflicts.is_empty() && m.moved => brought.push(dep),
                Ok(m) if m.moved => {
                    conflicted.extend(m.conflicts.iter().cloned());
                    brought.push(dep);
                }
                Ok(_) => {}
                Err(e) => failed.push(format!("{dep}: {e}")),
            }
        }
        let mut note = Vec::new();
        if !brought.is_empty() {
            let who: Vec<String> = brought
                .iter()
                .map(|d| {
                    let slot = self
                        .model
                        .tasks()
                        .find(|t| t.id == *d)
                        .and_then(|t| t.slot.clone());
                    format!("{d} ({})", slot.unwrap_or_default())
                })
                .collect();
            note.push(format!(
                "belay brought the work of {} into this worktree before your task.",
                who.join(", ")
            ));
        }
        if !conflicted.is_empty() {
            conflicted.sort();
            conflicted.dedup();
            note.push(format!(
                "Their changes conflicted in {}: resolve the conflict markers there first.",
                conflicted.join(", ")
            ));
        }
        if !failed.is_empty() {
            note.push(format!("belay could not bring in: {}.", failed.join("; ")));
        }
        eprintln!("belay fleet hub: {slot} for {tid}: {}", note.join(" "));
        (!note.is_empty()).then(|| note.join("\n"))
    }

    /// Move task `tid`'s own work into the worktree `target`, by a 3-way merge of trees:
    /// the tree the task started at, its worktree now, and the target now. Commit
    /// objects only, no ref moves. `up` (into a lead's worktree): a conflict is an error,
    /// nothing is written; else the merge is written with conflict markers, and the
    /// files are named. Without `git merge-tree --write-tree` (git < 2.40), a patch.
    pub(super) fn move_work(
        &self,
        tid: &str,
        target: &Path,
        up: bool,
    ) -> std::result::Result<Moved, String> {
        let task = self
            .model
            .tasks()
            .find(|t| t.id == tid)
            .ok_or("no such task")?;
        let src = task
            .slot
            .as_ref()
            .and_then(|s| self.worktrees.get(s).cloned().or_else(|| self.slot_dir(s)))
            .ok_or("its worktree is gone")?;
        // Whole worktrees: `git apply` in a subdirectory would skip what is outside it.
        let top = |p: &Path| crate::audit::repo_root(p).unwrap_or_else(|_| p.to_path_buf());
        let (src, target) = (top(&src), top(target));
        let target = target.as_path();
        if src == target {
            return Ok(Moved::default());
        }
        let scratch = self.dir.join("scratch");
        let _ = paths::ensure_private_dir(&scratch);
        let snap =
            |wt: &Path| crate::audit::worktree_tree(wt, &scratch).map_err(|e| format!("{e:#}"));
        let base = match self.bases.get(tid) {
            Some((_, tree)) => tree.clone(),
            None => {
                let c = self
                    .meta
                    .base
                    .clone()
                    .ok_or("the fleet has no base commit")?;
                git_out(&src, &["rev-parse", &format!("{c}^{{tree}}")]).ok_or("no base tree")?
            }
        };
        // A done task's work is its tree at done time: a stream worker that went on to
        // its next task must not pass half-done edits along.
        let from = match self.done_trees.get(tid) {
            Some((tree, _)) if !tree.is_empty() => tree.clone(),
            _ => snap(&src)?,
        };
        let into = snap(target)?;
        if from == base {
            return Ok(Moved::default());
        }
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(target)
                .args(args)
                .env("GIT_AUTHOR_NAME", "belay")
                .env("GIT_AUTHOR_EMAIL", "belay@localhost")
                .env("GIT_COMMITTER_NAME", "belay")
                .env("GIT_COMMITTER_EMAIL", "belay@localhost")
                .stdin(Stdio::null())
                .output()
        };
        let commit = |tree: &str, parent: Option<&str>| -> std::result::Result<String, String> {
            let mut args = vec!["commit-tree", tree, "-m", "belay"];
            if let Some(p) = parent {
                args.extend(["-p", p]);
            }
            let o = git(&args).map_err(|e| e.to_string())?;
            o.status
                .success()
                .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .ok_or_else(|| String::from_utf8_lossy(&o.stderr).trim().to_string())
        };
        let b = commit(&base, None)?;
        let (theirs, ours) = (commit(&from, Some(&b))?, commit(&into, Some(&b))?);
        let mb = format!("--merge-base={b}");
        let merged = git(&[
            "merge-tree",
            "--write-tree",
            "--name-only",
            &mb,
            &ours,
            &theirs,
        ])
        .map_err(|e| e.to_string())?;
        let code = merged.status.code();
        if !matches!(code, Some(0) | Some(1)) {
            // No usable merge-tree: the patch, as before (from the worktree as it is now:
            // a patch cannot start from a stored tree).
            return self
                .move_work_by_patch(&src, &base, target)
                .map(|moved| Moved {
                    moved,
                    conflicts: vec![],
                });
        }
        let text = String::from_utf8_lossy(&merged.stdout).to_string();
        let mut lines = text.lines();
        let tree = lines.next().unwrap_or("").trim().to_string();
        let conflicts: Vec<String> = if code == Some(1) {
            let mut c: Vec<String> = lines
                .take_while(|l| !l.is_empty())
                .map(String::from)
                .collect();
            c.sort();
            c.dedup();
            c
        } else {
            vec![]
        };
        if up && !conflicts.is_empty() {
            return Err(format!(
                "it conflicts with your changes in {} (its work is in {})",
                conflicts.join(", "),
                src.display()
            ));
        }
        if tree == into {
            return Ok(Moved::default());
        }
        let diff =
            git(&["diff-tree", "-p", "--binary", &into, &tree]).map_err(|e| e.to_string())?;
        if !diff.status.success() {
            return Err(String::from_utf8_lossy(&diff.stderr)
                .lines()
                .next()
                .unwrap_or("git diff-tree failed")
                .to_string());
        }
        let mut apply = Command::new("git")
            .arg("-C")
            .arg(target)
            .args(["apply", "--whitespace=nowarn", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        if let Some(mut i) = apply.stdin.take() {
            let _ = i.write_all(&diff.stdout);
        }
        let out = apply.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr)
                .lines()
                .next()
                .unwrap_or("git apply failed")
                .to_string());
        }
        Ok(Moved {
            moved: true,
            conflicts,
        })
    }

    /// The patch way (older git): the diff from `base`, and new files copied.
    pub(super) fn move_work_by_patch(
        &self,
        src: &Path,
        base: &str,
        target: &Path,
    ) -> std::result::Result<bool, String> {
        let diff = Command::new("git")
            .arg("-C")
            .arg(src)
            .args([
                "diff",
                "--binary",
                "--no-ext-diff",
                "--no-color",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                base,
            ])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        let mut moved = false;
        if !diff.stdout.is_empty() {
            let mut apply = Command::new("git")
                .arg("-C")
                .arg(target)
                .args(["apply", "--whitespace=nowarn", "-"])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| e.to_string())?;
            if let Some(mut i) = apply.stdin.take() {
                let _ = i.write_all(&diff.stdout);
            }
            let out = apply.wait_with_output().map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .next()
                    .unwrap_or("git apply failed")
                    .to_string());
            }
            moved = true;
        }
        let new = untracked(src);
        for f in &new {
            let to = target.join(f);
            if to.exists() {
                continue;
            }
            if let Some(p) = to.parent() {
                fs::create_dir_all(p).map_err(|e| e.to_string())?;
            }
            fs::copy(src.join(f), &to).map_err(|e| format!("copy {f}: {e}"))?;
            moved = true;
        }
        Ok(moved)
    }

    /// A part that is done brings its work up, into the worktree of the session that
    /// holds its parent task (a lead that split its task). A best-of's candidates are
    /// left apart: the judge picks one.
    pub(super) fn bring_up_done_parts(&mut self) {
        use belay_core::fleet::model::TaskState;
        let done: Vec<(String, String)> = self
            .model
            .tasks()
            .filter(|t| t.state == TaskState::Done && !self.lifted.contains(&t.id))
            .filter_map(|t| Some((t.id.clone(), t.parent.clone()?)))
            .collect();
        for (tid, parent) in done {
            self.lifted.insert(tid.clone());
            let Some(p) = self.model.tasks().find(|t| t.id == parent).cloned() else {
                continue;
            };
            if p.best_of || p.target.as_deref() == Some("judge") {
                continue;
            }
            let Some(target) = p.slot.as_ref().and_then(|s| self.worktrees.get(s)).cloned() else {
                continue;
            };
            let holder = p.slot.clone().unwrap_or_default();
            let causes: Vec<u64> = self
                .done_trees
                .get(&tid)
                .map(|(_, s)| *s)
                .into_iter()
                .collect();
            match self.move_work(&tid, &target, true) {
                Ok(m) if m.moved => {
                    eprintln!("belay fleet hub: {tid}'s work is in {holder}'s worktree now");
                    let reason = format!(
                        "{tid} is done, a part of {parent}: its work went up to {holder}'s worktree."
                    );
                    self.write_work(
                        Work::Lifted {
                            task: tid.clone(),
                            slot: holder.clone(),
                        },
                        reason,
                        causes,
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    let reason = format!(
                        "{tid} is done, a part of {parent}, but its work could not go up to {holder}'s worktree."
                    );
                    self.write_work(
                        Work::MoveFailed {
                            task: tid.clone(),
                            slot: holder.clone(),
                            up: true,
                            error: e.clone(),
                        },
                        reason,
                        causes,
                    );
                    // The lead hears it: its whole check cannot pass without this part.
                    let text = format!(
                        "belay could not bring {tid}'s work into your worktree: {e}. Merge it by hand, then go on."
                    );
                    let line = serde_json::json!({
                        "type": "user",
                        "message": {"role": "user", "content": format!("[belay fleet: notice from belay]\n{text}")},
                    });
                    self.write_line(&holder, &line);
                    inbox(&self.dir, &format!("{holder}: {text}"));
                }
            }
        }
    }

    /// With the audit on: the slot's worktree tree when it gets a task, to diff at check
    /// time. The task id is the delivered text's first words (`Task t-2: …`).
    pub(super) fn record_base(&mut self, slot: &str, text: &str) {
        let Some(task) = text
            .strip_prefix("Task ")
            .and_then(|t| t.split(':').next())
            .map(String::from)
        else {
            return;
        };
        let Some(wt) = self.worktrees.get(slot) else {
            return;
        };
        let Ok(root) = crate::audit::repo_root(wt) else {
            return;
        };
        let scratch = self.dir.join("scratch");
        let _ = paths::ensure_private_dir(&scratch);
        // Once per task: a retry to the same session (or a lead's message that looks like
        // a task) must not move the start past the first attempt's work.
        let holds =
            self.model.slots().get(slot).and_then(|s| s.task.as_deref()) == Some(task.as_str());
        if !holds || self.bases.contains_key(&task) {
            return;
        }
        if let Ok(tree) = crate::audit::worktree_tree(&root, &scratch) {
            self.bases.insert(task, (root, tree));
        }
    }
}

/// What moving a task's work did: whether anything changed, and the files left with
/// conflict markers.
#[derive(Debug, Default)]
pub(super) struct Moved {
    moved: bool,
    conflicts: Vec<String>,
}
