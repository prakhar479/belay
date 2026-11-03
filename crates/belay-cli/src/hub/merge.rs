//! `belay fleet report`, `merge`, `replay`, `restart` and `down`.

use super::*;

/// `belay fleet report`: what a fleet did, from its ledger: tasks, sessions, spend, and
/// the merge plan.
pub fn report(spec: Option<&str>) -> Result<String> {
    use belay_core::fleet::model::TaskState;
    let dir = resolve(spec)?;
    let meta = read_meta(&dir)?;
    let records = read_ledger(&dir);
    let model = replay(&meta, &records);
    let mut out = String::new();
    let span = match (records.first(), records.last()) {
        (Some(a), Some(b)) => crate::report::minutes(b.ts_ms.saturating_sub(a.ts_ms)),
        _ => "0 s".into(),
    };
    let _ = writeln!(
        out,
        "fleet {} · {} · {} · ran {span}",
        meta.id,
        serde_json::to_value(meta.spec.preset)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default(),
        if is_up(&dir) { "up" } else { "down" },
    );
    let tasks: Vec<_> = model.tasks().collect();
    let count = |st: TaskState| tasks.iter().filter(|t| t.state == st).count();
    let lost: u32 = tasks.iter().map(|t| t.attempts).sum();
    let _ = writeln!(
        out,
        "Tasks: {} · done {} · failed {} · blocked {} · dropped {} · in review {} · other {} · attempts that did not count: {lost}",
        tasks.len(),
        count(TaskState::Done),
        count(TaskState::Failed),
        count(TaskState::Blocked),
        count(TaskState::Dropped),
        count(TaskState::Review),
        tasks.len()
            - count(TaskState::Done)
            - count(TaskState::Failed)
            - count(TaskState::Blocked)
            - count(TaskState::Dropped)
            - count(TaskState::Review),
    );
    out.push_str(&render_tasks(&model).replacen("Tasks:\n", "", 1));
    // What the hub moved between worktrees (rule 4: these are in the ledger too).
    let moves: Vec<String> = records
        .iter()
        .filter_map(|r| match &r.body {
            FleetBody::Work { id, work, .. } => match work {
                Work::Brought {
                    task,
                    slot,
                    conflicts,
                } if conflicts.is_empty() => Some(format!("  {id} {task} → {slot}")),
                Work::Brought {
                    task,
                    slot,
                    conflicts,
                } => Some(format!(
                    "  {id} {task} → {slot}, conflicts in {}",
                    conflicts.join(", ")
                )),
                Work::Lifted { task, slot } => Some(format!("  {id} {task} → {slot} (up)")),
                Work::Resumed {
                    task,
                    slot,
                    worktree,
                    partial,
                } => Some(format!(
                    "  {id} {task} → {slot} in {worktree}'s worktree{}",
                    if *partial {
                        ", with the last attempt's work"
                    } else {
                        ""
                    }
                )),
                Work::MoveFailed {
                    task, slot, error, ..
                } => Some(format!("  {id} {task} → {slot} failed: {error}")),
                Work::Snapshot { .. } => None,
            },
            _ => None,
        })
        .collect();
    if !moves.is_empty() {
        let _ = writeln!(out, "Work moved: {}", moves.len());
        for m in moves {
            let _ = writeln!(out, "{m}");
        }
    }
    let restarts: u32 = model.slots().values().map(|s| s.restarts).sum();
    let (tokens, cost) = model.spend();
    let _ = writeln!(
        out,
        "Sessions: {} · restarts {restarts} · {} tokens · {}",
        model.slots().len(),
        crate::report::k(tokens),
        dollars(cost)
    );
    for (id, s) in model.slots() {
        let _ = writeln!(
            out,
            "  {id:<14} {:<8} {} tokens",
            format!("{:?}", s.state).to_lowercase(),
            crate::report::k(s.tokens)
        );
    }
    out.push_str(&merge_plan(&dir, &meta, &model, false)?);
    Ok(out)
}

/// Files a worktree changed from the fleet's base: committed on its branch, staged,
/// unstaged, or new.
pub(super) fn changed_files(wt: &Path, base: &str) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    if let Some(t) = git_out(
        wt,
        &["-c", "core.quotePath=false", "diff", "--name-only", base],
    ) {
        files.extend(t.lines().map(String::from));
    }
    if let Some(t) = git_out(
        wt,
        &[
            "-c",
            "core.quotePath=false",
            "ls-files",
            "--others",
            "--exclude-standard",
        ],
    ) {
        files.extend(t.lines().map(String::from));
    }
    files.sort();
    files.dedup();
    files
}

/// The merge plan, and with `yes`, a commit of each finished worktree's changes on its
/// own branch. belay never merges into the user's branch: it prints the commands.
pub(super) fn merge_plan(dir: &Path, meta: &Meta, model: &Model, yes: bool) -> Result<String> {
    use belay_core::fleet::model::TaskState;
    let mut out = String::new();
    let (Some(repo), Some(base)) = (&meta.repo, &meta.base) else {
        return Ok(out);
    };
    let order: Vec<String> = model.tasks().map(|t| t.id.clone()).collect();
    // Each worktree, with its slot's tasks, in the order the first of them was added.
    let mut rows: Vec<MergeRow> = Vec::new();
    let mut not_picked = 0;
    let mut in_parent = 0;
    for e in fs::read_dir(dir.join("worktrees"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let wt = e.path();
        let name = e.file_name().to_string_lossy().replace("__", "/");
        let files = changed_files(&wt, base);
        if files.is_empty() {
            continue;
        }
        // A worktree's tasks: its slot's, or for a team worktree (`team-<name>`), those of
        // every slot whose role works in that team.
        let team = name.strip_prefix("team-").map(|t| Workspace::Team {
            name: t.to_string(),
        });
        let mine: Vec<&belay_core::fleet::model::Task> = model
            .tasks()
            .filter(|t| {
                t.slot.as_deref().is_some_and(|s| {
                    s == name
                        || model.worktree_name(s).as_deref() == Some(name.as_str())
                        || team
                            .as_ref()
                            .is_some_and(|ws| model.role_of(s).is_some_and(|r| r.workspace == *ws))
                })
            })
            .collect();
        let first = mine
            .iter()
            .filter_map(|t| order.iter().position(|o| *o == t.id))
            .min()
            .unwrap_or(usize::MAX);
        // A part whose work went up into its parent's worktree is merged with it.
        let lifted_into = |t: &belay_core::fleet::model::Task| {
            let p = model.tasks().find(|p| Some(&p.id) == t.parent.as_ref())?;
            if p.best_of
                || p.target.as_deref() == Some("judge")
                || p.slot.as_deref() == Some(name.as_str())
            {
                return None;
            }
            p.slot.clone()
        };
        if !mine.is_empty()
            && mine
                .iter()
                .all(|t| t.state == TaskState::Done && lifted_into(t).is_some())
        {
            in_parent += 1;
            continue;
        }
        // A best-of candidate the judge did not pick is not merged (G9).
        if !mine.is_empty() && mine.iter().all(|t| t.picked == Some(false)) {
            not_picked += 1;
            continue;
        }
        let finished = !mine.is_empty() && mine.iter().all(|t| t.state == TaskState::Done);
        let labels = mine
            .iter()
            .map(|t| format!("{} {}", t.id, format!("{:?}", t.state).to_lowercase()))
            .collect();
        let tasks = mine.iter().map(|t| t.id.clone()).collect();
        rows.push(MergeRow {
            first,
            name,
            wt,
            files,
            labels,
            tasks,
            finished,
        });
    }
    rows.sort_by(|a, b| (a.first, &a.name).cmp(&(b.first, &b.name)));
    if in_parent > 0 {
        let _ = writeln!(
            out,
            "Left out: {in_parent} worktree{} of parts whose work is in their lead's worktree.",
            if in_parent == 1 { "" } else { "s" }
        );
    }
    if not_picked > 0 {
        let _ = writeln!(
            out,
            "Left out: {not_picked} worktree{} of candidates the judge did not pick.",
            if not_picked == 1 { "" } else { "s" }
        );
    }
    if rows.is_empty() {
        let _ = writeln!(out, "Merge plan: no worktree changed anything.");
        return Ok(out);
    }
    let _ = writeln!(
        out,
        "Merge plan ({} worktree{} with changes):",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
    let mut by_file: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (
        i,
        MergeRow {
            name,
            files,
            labels,
            finished,
            ..
        },
    ) in rows.iter().enumerate()
    {
        let shown: Vec<&str> = files.iter().take(4).map(String::as_str).collect();
        let more = if files.len() > 4 {
            format!(" and {} more", files.len() - 4)
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "  {}. {name}{}  {}{more}{}",
            i + 1,
            if labels.is_empty() {
                String::new()
            } else {
                format!(" ({})", labels.join(", "))
            },
            shown.join(", "),
            if *finished {
                ""
            } else {
                "  [not finished: left out]"
            }
        );
        for f in files {
            by_file.entry(f.as_str()).or_default().push(name.as_str());
        }
    }
    for (f, names) in &by_file {
        if names.len() > 1 {
            let _ = writeln!(
                out,
                "  ! {} all change {f}: merge one, then rebase the next on it.",
                names.join(", ")
            );
        }
    }
    if !yes {
        let _ = writeln!(out, "`belay fleet merge --yes` commits each finished worktree's changes to its own branch, and prints the merge commands.");
        return Ok(out);
    }
    let mut merges = Vec::new();
    for MergeRow {
        name,
        wt,
        labels,
        tasks,
        finished,
        ..
    } in &rows
    {
        if !finished {
            continue;
        }
        let branch = branch_name(&meta.id, name);
        let dirty = git_out(wt, &["status", "--porcelain"]).is_some();
        if dirty {
            let summary: Vec<String> = model
                .tasks()
                .filter(|t| tasks.contains(&t.id))
                .map(|t| {
                    format!(
                        "{}: {}",
                        t.id,
                        t.outcome.clone().unwrap_or_else(|| t.title.clone())
                    )
                })
                .collect();
            let msg = format!(
                "{}\n\n{}\n\nbelay fleet {}, {name}.",
                labels.join(", "),
                summary.join("\n"),
                meta.id
            );
            let run = |args: &[&str]| {
                Command::new("git")
                    .arg("-C")
                    .arg(wt)
                    .args(args)
                    .stdin(Stdio::null())
                    .output()
            };
            let markers = Command::new("git")
                .arg("-C")
                .arg(wt)
                .args([
                    "grep",
                    "-l",
                    "--untracked",
                    "-e",
                    "^<<<<<<< ",
                    "-e",
                    "^>>>>>>> ",
                ])
                .stdin(Stdio::null())
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            if !markers.is_empty() {
                let _ = writeln!(
                    out,
                    "  not committing {name}: conflict markers in {}. Resolve them first.",
                    markers.lines().collect::<Vec<_>>().join(", ")
                );
                continue;
            }
            let failed = [&["add", "-A"][..], &["commit", "-q", "-m", &msg]]
                .into_iter()
                .find_map(|args| match run(args) {
                    Ok(o) if o.status.success() => None,
                    Ok(o) => Some(
                        String::from_utf8_lossy(&o.stderr)
                            .lines()
                            .next()
                            .unwrap_or("")
                            .to_string(),
                    ),
                    Err(e) => Some(e.to_string()),
                });
            if let Some(why) = failed {
                let _ = writeln!(out, "  could not commit {name}'s changes: {why}");
                continue;
            }
            let _ = writeln!(out, "  committed {name}'s changes on {branch}.");
        }
        merges.push(format!("git merge {branch}"));
    }
    if !merges.is_empty() {
        let _ = writeln!(
            out,
            "Merge them into your branch, in this order, in {repo}:"
        );
        for m in merges {
            let _ = writeln!(out, "  {m}");
        }
    }
    Ok(out)
}

/// One worktree in the merge plan.
pub(super) struct MergeRow {
    /// Where its slot's first task is on the board: the merge order.
    first: usize,
    name: String,
    wt: PathBuf,
    files: Vec<String>,
    labels: Vec<String>,
    /// Its tasks' ids (a team worktree has several slots' tasks).
    tasks: Vec<String>,
    /// Every task of its slot is done.
    finished: bool,
}

/// `belay fleet merge [--yes]`.
pub fn merge(spec: Option<&str>, yes: bool) -> Result<String> {
    let dir = resolve(spec)?;
    let meta = read_meta(&dir)?;
    if yes && is_up(&dir) {
        bail!("the fleet is up; take it down (`belay fleet down`) before committing its worktrees");
    }
    let model = replay(&meta, &read_ledger(&dir));
    merge_plan(&dir, &meta, &model, yes)
}

/// `belay fleet replay`: the recorded events through the model again, with the fleet's
/// own file (the records must come out the same) or another one (what it would have
/// decided, given the same events) (G7).
pub fn replay_cmd(spec: Option<&str>, file: Option<&Path>) -> Result<String> {
    let dir = resolve(spec)?;
    let meta = read_meta(&dir)?;
    let records = read_ledger(&dir);
    let other = match file {
        Some(f) => Some(load_spec(Some(f), None)?.0),
        None => None,
    };
    let mut m = Model::new(other.clone().unwrap_or_else(|| meta.spec.clone()));
    let mut again = Vec::new();
    for r in &records {
        match &r.body {
            FleetBody::Event { event } => again.extend(m.step(event.clone(), r.ts_ms)),
            // The hub's own work is not the model's: it is passed over, numbers and all.
            FleetBody::Work { .. } => {
                m.note_record(r);
                again.push(r.clone());
            }
            FleetBody::Command { .. } => {}
        }
    }
    let mut out = String::new();
    let tally = |rs: &[FleetRecord]| {
        let mut t: BTreeMap<String, usize> = BTreeMap::new();
        for r in rs {
            if let FleetBody::Command { command, .. } = &r.body {
                let kind = match command {
                    Cmd::Answer { text, .. } if text.starts_with("belay: refused") => {
                        "refusal".to_string()
                    }
                    c => serde_json::to_value(c)
                        .ok()
                        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from))
                        .unwrap_or_default(),
                };
                *t.entry(kind).or_default() += 1;
            }
        }
        t
    };
    let first_diff = records
        .iter()
        .zip(&again)
        .position(|(a, b)| a != b)
        .or((records.len() != again.len()).then(|| records.len().min(again.len())));
    match (&other, first_diff) {
        (None, None) => {
            let _ = writeln!(
                out,
                "The ledger's {} events replay to the same {} records.",
                records
                    .iter()
                    .filter(|r| matches!(r.body, FleetBody::Event { .. }))
                    .count(),
                records.len()
            );
        }
        (None, Some(i)) => {
            let _ = writeln!(out, "The replay differs from the ledger at record {i}:");
            let show = |r: Option<&FleetRecord>| {
                r.and_then(|r| serde_json::to_string(r).ok())
                    .unwrap_or_else(|| "(none)".into())
            };
            let _ = writeln!(
                out,
                "  ledger: {}",
                belay_core::text::truncate_chars(&show(records.get(i)), 300)
            );
            let _ = writeln!(
                out,
                "  replay: {}",
                belay_core::text::truncate_chars(&show(again.get(i)), 300)
            );
        }
        (Some(_), _) => {
            let (a, b) = (tally(&records), tally(&again));
            let kinds: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            let _ = writeln!(
                out,
                "Commands, as recorded and with {}:",
                file.map(|f| f.display().to_string()).unwrap_or_default()
            );
            for k in kinds {
                let (x, y) = (
                    a.get(k).copied().unwrap_or(0),
                    b.get(k).copied().unwrap_or(0),
                );
                let _ = writeln!(
                    out,
                    "  {k:<12} {x:>4} → {y:<4}{}",
                    if x != y { "  *" } else { "" }
                );
            }
            let _ = writeln!(out, "Given the same events; sessions the other file would have started differently could have acted differently.");
        }
    }
    Ok(out)
}

/// `belay fleet restart`: start a fleet's hub again from its ledger (G1).
pub fn restart(spec: Option<&str>) -> Result<String> {
    let dir = resolve(spec)?;
    if is_up(&dir) {
        bail!("the fleet's hub is running; `belay fleet status` shows it");
    }
    supervise::start_hub(&dir, true)?;
    for _ in 0..200 {
        if is_up(&dir) {
            return status(dir.file_name().and_then(|n| n.to_str()));
        }
        thread::sleep(Duration::from_millis(25));
    }
    bail!(
        "the fleet hub did not start; see {}",
        dir.join("hub.log").display()
    )
}

/// Fleet control is the user's: refuse it in a fleet session's shell. The hub also
/// tells sessions apart by their process tree.
pub(super) fn refuse_in_fleet_session() -> Result<()> {
    if std::env::var_os("BELAY_FLEET_SLOT").is_some() {
        bail!("Fleet control is yours to run: use your own terminal, not a belay session. A fleet session asks its lead or you.");
    }
    Ok(())
}

/// `belay fleet down`: stop every session, and wait for the hub to finish.
pub fn down(spec: Option<&str>) -> Result<String> {
    refuse_in_fleet_session()?;
    let dir = resolve(spec)?;
    let said = send_cli(&dir, &HubRequest::Down)?;
    // Refused (a fleet session asked): nothing is going down, so do not wait for it.
    if said.starts_with("belay: refused") {
        bail!("{}", said.trim_start_matches("belay: "));
    }
    for _ in 0..400 {
        if !is_up(&dir) {
            return Ok(format!(
                "{said}\nThe fleet is down.\n{}",
                prune_at_down(&dir)
            ));
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(format!(
        "{said}\nThe hub is still stopping sessions; check `belay fleet status`."
    ))
}

/// A request to the fleet's hub, for stop, spawn, and scale.
pub fn ask(spec: Option<&str>, req: HubRequest) -> Result<String> {
    refuse_in_fleet_session()?;
    let dir = resolve(spec)?;
    send_cli(&dir, &req)
}
