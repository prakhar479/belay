//! The session brief (V1): what a session changed, what it checked, and what a reviewer
//! should look at, from the ledger alone (and the git diff the caller adds). Nothing in
//! it comes from the agent's own claims.
//!
//! "Passed after the last edit" is verify before done's rule (`controller::verify`): one
//! `VerifyState` per command, fed the same edits and runs.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::audit::{audit, FileChange};
use crate::controller::verify::{is_source, run_passed, VerifyState};
use crate::event::Event;
use crate::guard::GuardDecision;
use crate::perception::{command_key, extract_failures, OutputKind};
use crate::record::{settle_undelivered, ActionKind, Body, Entry};
use crate::text::truncate_chars;

/// One file the git diff shows changed since the session's start snapshot.
#[derive(Debug, Clone)]
pub struct GitChange {
    /// Repo-relative path, with the old and new text (None: absent on that side).
    pub change: FileChange,
    pub added: u64,
    pub removed: u64,
}

/// What the brief is built from. `blobs` gives a stored text by hash.
pub struct Input<'a> {
    pub session: &'a str,
    pub entries: &'a [Entry],
    pub blobs: &'a dyn Fn(&str) -> Option<String>,
    /// The repo root (or the session's directory): paths under it are shown relative.
    pub root: Option<&'a str>,
    /// The git diff from the session's start snapshot to now, when there is one.
    pub git: Option<Vec<GitChange>>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Brief {
    pub session: String,
    /// From the first entry to the last, as `belay report` counts it.
    pub duration_ms: Option<u64>,
    /// Model tokens, when the caller knows them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    pub changed: Vec<Changed>,
    pub checked: Vec<Check>,
    /// Most important first.
    pub look_at: Vec<LookAt>,
    pub belay: Did,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq)]
pub struct Tokens {
    pub prompt: u64,
    pub output: u64,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Changed {
    pub path: String,
    /// Lines added and removed, from git (None without it).
    pub added: Option<u64>,
    pub removed: Option<u64>,
    /// The ledger entry of the session's last edit of it, if the ledger saw one.
    pub last_edit: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// Its last run passed, and no source file changed after it.
    PassedAfterLastEdit,
    /// It passed, but source files changed after that run.
    NotRunSinceLastEdit,
    /// Its last run failed.
    Failing,
    /// Its last run's result is not known.
    Unknown,
}

impl CheckStatus {
    pub fn mark(self) -> &'static str {
        match self {
            Self::PassedAfterLastEdit => "✓",
            Self::NotRunSinceLastEdit => "⚠",
            Self::Failing => "✗",
            Self::Unknown => "?",
        }
    }

    pub fn text(self) -> &'static str {
        match self {
            Self::PassedAfterLastEdit => "passed after the last edit",
            Self::NotRunSinceLastEdit => "not run since the last edit",
            Self::Failing => "failing",
            Self::Unknown => "result not known",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Check {
    /// The command, normalized as loops and verify compare it.
    pub command: String,
    pub status: CheckStatus,
    pub runs: u64,
    /// The last run's output (`belay show o-N`).
    pub output: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LookAt {
    /// guard, verify, test_change, check_config, dependencies, suppression,
    /// changed_after_pass, unchecked.
    pub kind: String,
    pub text: String,
    /// What shows it: an action (a-N), an output (o-N), an entry number, or `git`.
    pub evidence: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq)]
pub struct Did {
    pub summaries: u64,
    pub tokens_kept_out: u64,
    pub guard_blocks: u64,
    pub loops: u64,
    pub memories_recalled: u64,
}

impl Brief {
    /// The three lines the exit summary shows: Changed, Checked, Look at.
    pub fn headline(&self) -> [String; 3] {
        let changed = if self.changed.is_empty() {
            "Changed: nothing".to_string()
        } else {
            let (a, r) = self.changed.iter().fold((None, None), |(a, r), c| {
                (sum(a, c.added), sum(r, c.removed))
            });
            let counts = match (a, r) {
                (Some(a), Some(r)) => format!(" (+{a} −{r})"),
                _ => String::new(),
            };
            format!("Changed: {}{counts}", plural(self.changed.len(), "file"))
        };
        let checked = if self.checked.is_empty() {
            "Checked: no test or check ran".to_string()
        } else if self.checked.len() <= MAX_NAMED {
            let parts: Vec<String> = self
                .checked
                .iter()
                .map(|c| {
                    let cmd = truncate_chars(&c.command, MAX_COMMAND);
                    match c.status {
                        CheckStatus::PassedAfterLastEdit => format!("✓ {cmd}"),
                        s => format!("{} {cmd} ({})", s.mark(), s.text()),
                    }
                })
                .collect();
            format!("Checked: {}", parts.join(" · "))
        } else {
            // Many commands: how many in each state, worst first.
            let parts: Vec<String> = [
                CheckStatus::Failing,
                CheckStatus::NotRunSinceLastEdit,
                CheckStatus::Unknown,
                CheckStatus::PassedAfterLastEdit,
            ]
            .iter()
            .filter_map(|s| {
                let n = self.checked.iter().filter(|c| c.status == *s).count();
                (n > 0).then(|| format!("{} {n} {}", s.mark(), s.text()))
            })
            .collect();
            format!(
                "Checked: {} commands: {}",
                self.checked.len(),
                parts.join(" · ")
            )
        };
        let look = match self.look_at.len() {
            0 => "Look at: nothing flagged".to_string(),
            n => format!("Look at: {}", plural(n, "item")),
        };
        [changed, checked, look]
    }
}

/// The headline names this many commands; more are counted by state.
const MAX_NAMED: usize = 3;
/// A command in the headline is cut to this many characters.
const MAX_COMMAND: usize = 48;

fn sum(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    Some(a.unwrap_or(0) + b?)
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

/// A path relative to `root` when it is under it.
fn relative(root: Option<&str>, path: &str) -> String {
    root.map(|r| r.trim_end_matches('/'))
        .and_then(|r| path.strip_prefix(r)?.strip_prefix('/'))
        .unwrap_or(path)
        .to_string()
}

/// What one run of a test or check showed.
struct Run {
    command: String,
    passed: Option<bool>,
}

/// The run an output entry is, if it is one: its command and result.
fn run_of(e: &Entry, by_seq: &BTreeMap<u64, &Entry>, blob: &str, input: &Input) -> Option<Run> {
    let cause = by_seq.get(e.causes.first()?)?;
    // A direct run's event knows the exit; a `belay run` report's does not ([Verify] as
    // in restore: the exit code of a `belay run` is not in the ledger).
    let (command, failed) = match &cause.body {
        Body::Event {
            event: Event::ToolCompleted { input, failed, .. },
        } => (input["command"].as_str()?.to_string(), Some(*failed)),
        Body::Action {
            action: ActionKind::Wrap { .. },
            ..
        } => {
            let ev = by_seq.get(cause.causes.first()?)?;
            match &ev.body {
                Body::Event {
                    event: Event::ToolRequested { input, .. },
                } => (input["command"].as_str()?.to_string(), None),
                _ => return None,
            }
        }
        _ => return None,
    };
    let output = (input.blobs)(blob).unwrap_or_default();
    let summary_pass =
        crate::learn::summary_run(&command) && crate::perception::signature::shows_pass(&output);
    let passed = run_passed(!extract_failures(&output).is_empty(), failed, summary_pass);
    Some(Run {
        command: command_key(&command),
        passed,
    })
}

/// Build the brief.
pub fn build(input: &Input) -> Brief {
    let mut entries = input.entries.to_vec();
    settle_undelivered(&mut entries);
    let by_seq: BTreeMap<u64, &Entry> = entries.iter().map(|e| (e.seq, e)).collect();
    let allowed: BTreeSet<&str> = entries
        .iter()
        .filter_map(|e| match &e.body {
            Body::Override { action } => Some(action.as_str()),
            _ => None,
        })
        .collect();
    let overrides: BTreeSet<u64> = entries
        .iter()
        .filter(|e| matches!(e.body, Body::Override { .. }))
        .map(|e| e.seq)
        .collect();

    // Edits and runs, in ledger order.
    let mut states: BTreeMap<String, (VerifyState, Option<bool>, u64, String)> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut last_edit: BTreeMap<String, u64> = BTreeMap::new();
    // Edited files' first and last texts, where the ledger has them (shell writes).
    let mut texts: BTreeMap<String, (Option<String>, Option<String>)> = BTreeMap::new();
    let mut last_pass: Option<u64> = None;
    let mut look: Vec<(u8, LookAt)> = Vec::new();
    let mut did = Did::default();
    for e in &entries {
        match &e.body {
            Body::Event { event } => {
                let path = match event {
                    Event::FileEdited { path, .. } => path,
                    Event::FileChanged {
                        path,
                        old_text,
                        new_text,
                        ..
                    } => {
                        let rel = relative(input.root, path);
                        let unblob = |t: &str| match t
                            .strip_prefix("<blob:")
                            .and_then(|t| t.strip_suffix('>'))
                        {
                            Some(h) => (input.blobs)(h),
                            None => Some(t.to_string()),
                        };
                        let slot = texts.entry(rel).or_insert_with(|| (unblob(old_text), None));
                        slot.1 = unblob(new_text);
                        path
                    }
                    _ => continue,
                };
                for (s, ..) in states.values_mut() {
                    s.on_edit(path);
                }
                last_edit.insert(relative(input.root, path), e.seq);
            }
            Body::Snapshot {
                files, undo: false, ..
            } => {
                for f in files {
                    last_edit.entry(f.clone()).or_insert(e.seq);
                }
            }
            Body::Output {
                id,
                blob,
                output_kind: OutputKind::Test | OutputKind::Build,
                ..
            } => {
                let Some(run) = run_of(e, &by_seq, blob, input) else {
                    continue;
                };
                if !states.contains_key(&run.command) {
                    order.push(run.command.clone());
                }
                let slot = states
                    .entry(run.command.clone())
                    .or_insert_with(|| (VerifyState::default(), None, 0, String::new()));
                slot.0.on_run(&run.command, run.passed, e.seq);
                slot.1 = run.passed;
                slot.2 += 1;
                slot.3 = id.clone();
                if run.passed == Some(true) {
                    last_pass = Some(e.seq);
                }
            }
            Body::Action {
                id,
                action,
                applied,
                ..
            } => match action {
                // The note on the edit the user allowed: the first action says it all.
                ActionKind::Guard { .. } if e.causes.iter().any(|c| overrides.contains(c)) => {}
                ActionKind::Guard { path, decision } => {
                    let path = relative(input.root, path);
                    let (what, blocks) = match decision {
                        GuardDecision::Deny => ("blocked", true),
                        GuardDecision::Ask => ("asked about", true),
                        GuardDecision::Warn => ("warned about", false),
                    };
                    if blocks && *applied {
                        did.guard_blocks += 1;
                    }
                    let user = if allowed.contains(id.as_str()) {
                        "; the user allowed it"
                    } else if blocks {
                        "; not allowed"
                    } else {
                        ""
                    };
                    let not = if *applied { "" } else { " (not applied)" };
                    let text = format!("the test guard {what} an edit to {path}{user}{not}");
                    look.push((0, item("guard", text, id)));
                }
                ActionKind::BlockCommand { command } => {
                    if *applied {
                        did.guard_blocks += 1;
                    }
                    let text = format!("belay blocked the command `{command}`");
                    look.push((0, item("guard", text, id)));
                }
                ActionKind::Verify { command, .. } => {
                    let text = format!(
                        "the turn ended with files changed since `{command}` last passed; belay asked for a run"
                    );
                    look.push((1, item("verify", text, id)));
                }
                ActionKind::Shape {
                    from_tokens,
                    to_tokens,
                    ..
                } if *applied => {
                    did.summaries += 1;
                    did.tokens_kept_out += from_tokens.saturating_sub(*to_tokens) as u64;
                }
                ActionKind::Steer { .. } if *applied => did.loops += 1,
                ActionKind::MemoryRecalled { ids, .. } if *applied => {
                    did.memories_recalled += ids.len() as u64;
                }
                ActionKind::Recall { .. } if *applied => did.memories_recalled += 1,
                _ => {}
            },
            _ => {}
        }
    }

    // Changed: the ledger's edits and snapshots, and the git diff.
    let mut changed: BTreeMap<String, Changed> = BTreeMap::new();
    for (path, seq) in &last_edit {
        changed.insert(
            path.clone(),
            Changed {
                path: path.clone(),
                last_edit: Some(*seq),
                ..Changed::default()
            },
        );
    }
    for g in input.git.iter().flatten() {
        let c = changed
            .entry(g.change.path.clone())
            .or_insert_with(|| Changed {
                path: g.change.path.clone(),
                ..Changed::default()
            });
        c.added = Some(g.added);
        c.removed = Some(g.removed);
    }
    let evidence = |path: &str| {
        last_edit
            .get(path)
            .map_or_else(|| "git".to_string(), u64::to_string)
    };

    // The audit's rules, over the git diff, or over what the ledger has without git.
    let changes: Vec<FileChange> = match &input.git {
        Some(g) => g.iter().map(|g| g.change.clone()).collect(),
        None => changed
            .keys()
            .map(|p| {
                let (old, new) = texts.get(p).cloned().unwrap_or((None, None));
                FileChange {
                    path: p.clone(),
                    old,
                    new,
                }
            })
            .collect(),
    };
    let report = audit(&changes, &|_| false);
    for t in report.tests.iter().filter(|t| !t.weakened.is_empty()) {
        let text = format!(
            "{} may weaken tests: {}",
            t.path,
            t.weakened.join("; ").trim_end_matches('.')
        );
        look.push((0, item("test_change", text, &evidence(&t.path))));
    }
    for c in &report.config {
        let text = if c.weaker.is_empty() {
            format!("check or CI config changed: {}", c.path)
        } else {
            format!(
                "check or CI config changed: {} ({})",
                c.path,
                c.weaker.join(", ")
            )
        };
        look.push((2, item("check_config", text, &evidence(&c.path))));
    }
    for path in &report.dependencies {
        let side = |pick: fn(&FileChange) -> &Option<String>| {
            changes
                .iter()
                .find(|c| &c.path == path)
                .and_then(|c| pick(c).as_deref())
                .map(|t| dependency_names(path, t))
                .unwrap_or_default()
        };
        let added: Vec<String> = side(|c| &c.new)
            .difference(&side(|c| &c.old))
            .cloned()
            .collect();
        let text = if added.is_empty() {
            format!("dependency manifest changed: {path}")
        } else {
            format!(
                "dependency manifest changed: {path} (added {})",
                added.join(", ")
            )
        };
        look.push((3, item("dependencies", text, &evidence(path))));
    }
    for s in &report.suppressions {
        let why = if s.has_reason {
            ""
        } else {
            ", no reason given"
        };
        let text = format!("suppression added: {}:{} ({}{why})", s.path, s.line, s.kind);
        look.push((4, item("suppression", text, &evidence(&s.path))));
    }
    match last_pass {
        Some(pass) => {
            for (path, seq) in &last_edit {
                if *seq > pass && is_source(path) {
                    let text = format!("{path} changed after the last passing run");
                    look.push((5, item("changed_after_pass", text, &seq.to_string())));
                }
            }
        }
        // Runs that never passed: Checked says so already.
        None if !states.is_empty() => {}
        None => {
            if let Some(seq) = last_edit
                .iter()
                .filter(|(p, _)| is_source(p))
                .map(|(_, s)| *s)
                .max()
            {
                let text = "source files changed and no test or check ran".to_string();
                look.push((5, item("unchecked", text, &seq.to_string())));
            }
        }
    }
    // Stable: by importance, then in the order found.
    look.sort_by_key(|(rank, _)| *rank);

    let checked = order
        .into_iter()
        .filter_map(|cmd| {
            let (state, last, runs, output) = states.remove(&cmd)?;
            let status = match (last, state.changed_since_pass()) {
                (Some(false), _) => CheckStatus::Failing,
                (Some(true), false) => CheckStatus::PassedAfterLastEdit,
                (Some(true), true) => CheckStatus::NotRunSinceLastEdit,
                (None, _) => CheckStatus::Unknown,
            };
            Some(Check {
                command: cmd,
                status,
                runs,
                output,
            })
        })
        .collect();

    let first = entries.iter().map(|e| e.ts_ms).filter(|t| *t > 0).min();
    let last = entries.iter().map(|e| e.ts_ms).max();
    Brief {
        session: input.session.to_string(),
        duration_ms: match (first, last) {
            (Some(a), Some(b)) if b >= a => Some(b - a),
            _ => None,
        },
        tokens: None,
        changed: changed.into_values().collect(),
        checked,
        look_at: look.into_iter().map(|(_, l)| l).collect(),
        belay: did,
    }
}

fn item(kind: &str, text: String, evidence: &str) -> LookAt {
    LookAt {
        kind: kind.into(),
        text,
        evidence: evidence.into(),
    }
}

/// Dependency names a manifest declares, where that is cheap to read. Lockfiles and
/// build scripts give none: the brief then names the file only.
pub fn dependency_names(path: &str, text: &str) -> BTreeSet<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let mut out = BTreeSet::new();
    match name {
        "package.json" => {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
                return out;
            };
            for key in [
                "dependencies",
                "devDependencies",
                "peerDependencies",
                "optionalDependencies",
            ] {
                if let Some(m) = v[key].as_object() {
                    out.extend(m.keys().cloned());
                }
            }
        }
        "Cargo.toml" | "pyproject.toml" => {
            let mut section = String::new();
            let mut in_array = false;
            for line in text.lines() {
                let line = line.split('#').next().unwrap_or("").trim();
                if let Some(h) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                    section = h.trim_matches(['[', ']']).to_string();
                    in_array = false;
                    // `[dependencies.serde]`
                    if let Some((head, dep)) = section.rsplit_once('.') {
                        if head.ends_with("dependencies") && name == "Cargo.toml" {
                            out.insert(dep.trim_matches('"').to_string());
                        }
                    }
                    continue;
                }
                let key_section = section.ends_with("dependencies")
                    && !(name == "pyproject.toml"
                        && matches!(
                            section.as_str(),
                            "project.optional-dependencies" | "dependency-groups"
                        ));
                if key_section {
                    if let Some(c) = re!(r"^([A-Za-z0-9_.-]+)\s*=").captures(line) {
                        if &c[1] != "python" {
                            out.insert(c[1].to_string());
                        }
                    }
                    continue;
                }
                // PEP 621 arrays of requirement strings.
                let arrays = name == "pyproject.toml"
                    && (section == "project"
                        || section == "project.optional-dependencies"
                        || section == "dependency-groups");
                if !arrays {
                    continue;
                }
                let starts = section != "project" || line.starts_with("dependencies");
                if starts && line.contains('[') {
                    in_array = true;
                }
                if in_array {
                    for c in re!(r#"["']([A-Za-z0-9][A-Za-z0-9._-]*)"#).captures_iter(line) {
                        out.insert(c[1].to_ascii_lowercase());
                    }
                    if line.contains(']') {
                        in_array = false;
                    }
                }
            }
        }
        "go.mod" => {
            let mut block = false;
            for line in text.lines().map(str::trim) {
                if line.starts_with("require (") {
                    block = true;
                } else if block && line.starts_with(')') {
                    block = false;
                } else if let Some(c) = re!(r"^(?:require\s+)?([\w.\-/]+)\s+v\d")
                    .captures(line)
                    .filter(|_| block || line.starts_with("require"))
                {
                    out.insert(c[1].to_string());
                }
            }
        }
        "Gemfile" => {
            for c in re!(r#"(?m)^\s*gem\s+['"]([^'"]+)"#).captures_iter(text) {
                out.insert(c[1].to_string());
            }
        }
        n if n.starts_with("requirements") && n.ends_with(".txt") => {
            for line in text.lines().map(str::trim) {
                if let Some(c) = re!(r"^([A-Za-z0-9][A-Za-z0-9._-]*)").captures(line) {
                    out.insert(c[1].to_ascii_lowercase());
                }
            }
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests;
