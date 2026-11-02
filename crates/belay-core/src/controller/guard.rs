//! The test guard's path through the controller: edits an agent proposes (the guard
//! can block them) and files a shell command changed (it can only warn).
//!
//! Precision (P0.1): only files inside the project are judged; a removal that a later
//! edit of the same turn shows to be a move is not a weakening. A shell command's
//! removals wait for the command's end, so a move across its files raises nothing; an
//! edit's removals only warn, and a later edit that proves a move withdraws the warning
//! (a `guard_withdrawn` judgment).

use std::collections::{BTreeMap, BTreeSet};

use super::{features, Blob, Controller, Decision, EditKey, Feature, Permission};
use super::{GuardSetting, MAX_EDIT_BLOB_BYTES};
use crate::broker::{Answer, Question, Source};
use crate::event::Event;
use crate::guard::{self, items, GuardDecision};
use crate::loops::LoopSignal;
use crate::record::{ActionKind, Body, SteerTrigger};
use crate::text::sha256_hex;

/// Test files whose first text a session keeps for the shell-write guard.
const MAX_SHELL_BASELINES: usize = 64;
/// Items one turn remembers as added, and warnings it may still withdraw.
const MAX_TURN_ITEMS: usize = 4096;
const MAX_PENDING: usize = 64;

type ItemKey = (String, String);

#[derive(Debug, Clone, Default)]
pub(super) struct GuardState {
    /// Test files a shell command changed: their text when this session first saw
    /// them. Removing a test the session itself added is not weakening.
    baseline: BTreeMap<String, String>,
    /// The project's top directory (the worktree): files outside it are not tests.
    top: Option<String>,
    /// Definitions (name, body hash) edits added in this turn.
    added: BTreeSet<ItemKey>,
    /// Edit warnings for removed tests a later edit of this turn may show moved.
    pending: Vec<Pending>,
    /// Shell-write warnings waiting for the end of their command.
    held: Vec<Held>,
}

#[derive(Debug, Clone)]
struct Pending {
    action: u64,
    path: String,
    removed: BTreeSet<ItemKey>,
}

#[derive(Debug, Clone)]
struct Held {
    tool_use_id: String,
    path: String,
    judgment: u64,
    reasons: Vec<String>,
    old: String,
    new: String,
    removed: BTreeSet<ItemKey>,
}

/// A judged change that weakens tests.
pub(super) struct Judged {
    judgment: u64,
    reasons: Vec<String>,
    /// Removed tests that explain every reason: a later edit may show them moved.
    removed: Option<BTreeSet<ItemKey>>,
    /// The old text the judgment used (moved definitions cut out).
    old: String,
}

/// Every reason says something was removed: what a move looks like in one file.
fn removal_only(reasons: &[String]) -> bool {
    reasons
        .iter()
        .all(|r| matches!(guard::rule_of(r), "assertions_removed" | "tests_removed"))
}

/// Definitions in `text`, by key, with whether each holds test code.
fn keyed(text: &str) -> BTreeMap<ItemKey, bool> {
    items::items(text)
        .into_iter()
        .map(|it| {
            let (a, t) = guard::counts(&text[it.span.clone()]);
            (it.key(), a + t > 0)
        })
        .collect()
}

impl Controller {
    /// Guard bookkeeping before an event is handled: the project top, the end of a
    /// shell command (its held warnings), and the turn boundary.
    pub(super) fn guard_before(&mut self, d: &mut Decision, event: &Event, now: u64) {
        let same_command = |h: &Held| match event {
            Event::FileChanged { tool_use_id, .. } => *tool_use_id == h.tool_use_id,
            _ => false,
        };
        if self.guard.held.iter().any(|h| !same_command(h)) {
            self.release_held(d, now);
        }
        match event {
            Event::Workspace { top, .. } if !top.is_empty() => {
                self.guard.top = Some(top.clone());
            }
            Event::PromptSubmitted { .. } | Event::TurnEnded => {
                self.guard.added.clear();
                self.guard.pending.clear();
            }
            _ => {}
        }
    }

    /// The path relative to the project, or None when it lies outside: a draft in
    /// `/tmp` is not one of the project's tests. A relative path, or any path while the
    /// top is not known, counts as inside.
    fn project_path<'a>(&self, path: &'a str) -> Option<&'a str> {
        let top = self
            .guard
            .top
            .as_deref()
            .or(Some(self.policy.root.as_str()).filter(|r| !r.is_empty()));
        let Some(top) = top.filter(|_| path.starts_with('/')) else {
            return Some(path);
        };
        let top = top.trim_end_matches('/');
        let rest = path.strip_prefix(top)?;
        (rest.is_empty() || rest.starts_with('/')).then(|| rest.trim_start_matches('/'))
    }

    pub(super) fn on_edit_proposed(&mut self, d: &mut Decision, event: Event, now: u64) {
        let Event::EditProposed {
            tool_use_id,
            path,
            old_text,
            new_text,
        } = event
        else {
            return;
        };
        let (old_hash, new_hash) = (sha256_hex(&old_text), sha256_hex(&new_text));
        let key: EditKey = (path.clone(), old_hash.clone(), new_hash.clone());
        let stored = Event::EditProposed {
            tool_use_id,
            path: path.clone(),
            old_text: format!("<blob:{old_hash}>"),
            new_text: format!("<blob:{new_hash}>"),
        };
        // Keep the texts only where the guard reads them, and only test-sized ones: an
        // edit of a lockfile or a data file is not worth a copy on disk (I4, F4).
        if guard::judges(&path, &old_text, &new_text) {
            for (hash, content) in [(old_hash, &old_text), (new_hash, &new_text)] {
                if content.len() <= MAX_EDIT_BLOB_BYTES {
                    d.blobs.push(Blob {
                        hash,
                        content: content.clone(),
                    });
                }
            }
        }
        let ev = self.push(d, now, vec![], Body::Event { event: stored });
        // A fleet role edits only within its scope, and not what another session in its
        // worktree claimed (F5c).
        if !self.on(Feature::Guard) {
            return;
        }
        if let Some(why) = self.fleet_rules.as_ref().and_then(|r| r.refuse(&path)) {
            let message = format!(
                "belay: in this fleet, this session may not edit {path}: {why}. Ask your lead (`belay task blocked`) if it must change."
            );
            let decision = self.effective_guard(GuardDecision::Deny);
            let action = ActionKind::Guard {
                path: path.clone(),
                decision,
            };
            if self.act(d, now, ev, action, message.clone()) {
                match decision {
                    GuardDecision::Warn => d.response.add_context.push(message),
                    _ => d.response.permission = Some(Permission::Deny(message)),
                }
            }
            return;
        }
        let Some(judged) = self.judge_test_edit(d, now, ev, &path, &old_text, &new_text) else {
            return;
        };
        let (j, reasons) = (judged.judgment, judged.reasons);

        // A removal may be half of a move: never block it, warn until the turn tells.
        let decision = match judged.removed {
            Some(_) => GuardDecision::Warn,
            None => self.effective_guard(self.session.guard),
        };
        // U6.3: rules the user allowed twice only warn for the rest of the session.
        let rules = features::rules_of(&reasons);
        let relaxed = decision != GuardDecision::Warn && self.guard_fb.warns(&rules);
        let decision = if relaxed {
            GuardDecision::Warn
        } else {
            decision
        };
        if let Some((id, override_seq)) = self.allowed.remove(&key) {
            let reason = format!("Allowed once by the user (`belay allow {id}`).");
            let action = ActionKind::Guard { path, decision };
            let a = self.next_action_id();
            self.push(
                d,
                now,
                vec![j, override_seq],
                Body::Action {
                    id: a,
                    action,
                    reason,
                    applied: false,
                    unsupported: false,
                },
            );
            return;
        }
        let blocking = decision != GuardDecision::Warn;
        let message = if blocking {
            format!(
                "belay: this edit may weaken tests in {path}: {}. If the user wants it anyway, they can run `! belay allow` and you can retry the same edit.",
                reasons.join("; ")
            )
        } else {
            format!(
                "belay: this edit may weaken tests in {path}: {}.",
                reasons.join("; ")
            )
        };
        let reason = if relaxed {
            format!(
                "{message} It warns instead of blocking: the user allowed {} twice this session.",
                rules.join(", ")
            )
        } else if judged.removed.is_some() {
            format!(
                "{message} It warns instead of blocking: the tests may have moved to another file."
            )
        } else {
            message.clone()
        };
        let action = ActionKind::Guard {
            path: path.clone(),
            decision,
        };
        let id = format!("a-{}", self.next_action);
        let applied = self.act(d, now, j, action, reason);
        if let Some(removed) = judged.removed {
            if self.guard.pending.len() < MAX_PENDING {
                let action = d.entries.last().map_or(j, |e| e.seq);
                self.guard.pending.push(Pending {
                    action,
                    path,
                    removed,
                });
            }
        }
        if applied {
            if blocking {
                self.blocked.insert(id.clone(), (key, rules));
                self.last_blocked = Some(id);
            }
            match decision {
                GuardDecision::Ask => d.response.permission = Some(Permission::Ask(message)),
                GuardDecision::Deny => d.response.permission = Some(Permission::Deny(message)),
                GuardDecision::Warn => d.response.add_context.push(message),
            }
        }
    }

    /// Ask whether an edit to a test file weakens it. Returns the judgment when it
    /// does. Files that are not tests, or lie outside the project, are not judged.
    fn judge_test_edit(
        &mut self,
        d: &mut Decision,
        now: u64,
        cause: u64,
        path: &str,
        old: &str,
        new: &str,
    ) -> Option<Judged> {
        let rel = self.project_path(path)?;
        let is_test = guard::judges(rel, old, new) || self.policy.is_test(path);
        let protected = self.policy.protected_by(path).map(str::to_string);
        if !is_test && protected.is_none() {
            return None;
        }
        // A removed extra copy of a test that stays (a duplicate) takes nothing away.
        let old = items::without_removed_duplicates(old, new);
        let old = old.as_str();
        // Definitions this edit removes and adds. A removed one that an earlier edit of
        // this turn added elsewhere moved: cut it from the old text.
        // A test whose name stays in the file, changed, did not move.
        let (before, after) = (keyed(old), keyed(new));
        let kept: BTreeSet<&str> = after.keys().map(|(n, _)| n.as_str()).collect();
        let removed: BTreeSet<ItemKey> = before
            .iter()
            .filter(|(k, tests)| **tests && !kept.contains(k.0.as_str()))
            .map(|(k, _)| k.clone())
            .collect();
        let (moved, rest): (BTreeSet<_>, BTreeSet<_>) = removed
            .into_iter()
            .partition(|k| self.guard.added.contains(k));
        let old = if moved.is_empty() {
            old.to_string()
        } else {
            items::strip(old, &moved)
        };
        let old = old.as_str();
        for k in after.keys().filter(|k| !before.contains_key(*k)) {
            if self.guard.added.len() < MAX_TURN_ITEMS {
                self.guard.added.insert(k.clone());
            }
        }
        self.withdraw_moved(d, now, cause);

        let q = Question::TestWeakened { path, old, new };
        let (answer, source) = if is_test {
            self.broker.ask(&q)
        } else {
            (Answer::Neutral, Source::Rule)
        };
        // E4: the edit makes the test expect what the failing run produced.
        let pairs: Vec<(String, String)> =
            self.failure_values.values().flatten().cloned().collect();
        let answer = match (answer, guard::expected_to_actual(old, new, &pairs)) {
            (Answer::Weakened { mut reasons }, Some(r)) => {
                reasons.push(r);
                Answer::Weakened { reasons }
            }
            (_, Some(r)) => Answer::Weakened { reasons: vec![r] },
            (a, None) => a,
        };
        // E7: the repo asks for a look at every edit of a protected path.
        let protected_too = protected.is_some();
        let answer = match (answer, protected) {
            (a, None) => a,
            (Answer::Weakened { mut reasons }, Some(pat)) => {
                reasons.push(format!("the path is protected by .belay.toml (`{pat}`)"));
                Answer::Weakened { reasons }
            }
            (_, Some(pat)) => Answer::Weakened {
                reasons: vec![format!("the path is protected by .belay.toml (`{pat}`)")],
            },
        };
        let (name, version) = (q.name(), q.version());
        let j = self.push(
            d,
            now,
            vec![cause],
            Body::Judgment {
                question: name.into(),
                version,
                subject: path.to_string(),
                answer: answer.clone(),
                source,
            },
        );
        let Answer::Weakened { reasons } = answer else {
            return None;
        };
        // The removed tests explain every reason when the text without them weakens
        // nothing: they may have moved, and a later edit may show where.
        let removed = (!rest.is_empty() && !protected_too && removal_only(&reasons))
            .then(|| {
                let without = items::strip(old, &rest);
                let fine = !matches!(
                    guard::assess_path(path, &without, new),
                    Answer::Weakened { .. }
                );
                fine.then_some(rest)
            })
            .flatten();
        Some(Judged {
            judgment: j,
            reasons,
            removed,
            old: old.to_string(),
        })
    }

    /// Withdraw this turn's warnings whose removed tests all turned up elsewhere.
    fn withdraw_moved(&mut self, d: &mut Decision, now: u64, cause: u64) {
        let added = &self.guard.added;
        let (moved, keep): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.guard.pending)
            .into_iter()
            .partition(|p| p.removed.is_subset(added));
        self.guard.pending = keep;
        for p in moved {
            self.withdrawn(d, now, vec![p.action, cause], p.path);
        }
    }

    /// Record that a guard finding was a move, not a weakening.
    fn withdrawn(&mut self, d: &mut Decision, now: u64, causes: Vec<u64>, path: String) {
        self.push(
            d,
            now,
            causes,
            Body::Judgment {
                question: "guard_withdrawn".into(),
                version: 1,
                subject: path,
                answer: Answer::Neutral,
                source: Source::Rule,
            },
        );
    }

    /// A shell command changed a test file (R1.4). The change already happened, so the
    /// guard can only warn: the agent hears it in context.
    pub(super) fn on_file_changed(&mut self, d: &mut Decision, event: Event, now: u64) {
        let Event::FileChanged {
            tool_use_id,
            path,
            old_text,
            new_text,
        } = event
        else {
            return;
        };
        let (old_hash, new_hash) = (sha256_hex(&old_text), sha256_hex(&new_text));
        let stored = Event::FileChanged {
            tool_use_id: tool_use_id.clone(),
            path: path.clone(),
            old_text: format!("<blob:{old_hash}>"),
            new_text: format!("<blob:{new_hash}>"),
        };
        let ev = self.push(d, now, vec![], Body::Event { event: stored });
        d.blobs.push(Blob {
            hash: old_hash,
            content: old_text.clone(),
        });
        d.blobs.push(Blob {
            hash: new_hash.clone(),
            content: new_text.clone(),
        });
        // Edit, revert, edit through the shell is a cycle too.
        if let Some(LoopSignal::Cycle { path }) = self.loops.on_edit(&path, &new_hash) {
            let text = format!("belay: {path} is back to an earlier version (edit, revert, edit).");
            let reason = format!("Content hash of {path} repeated an earlier state.");
            self.steer(d, now, ev, SteerTrigger::Cycle { path }, text, reason);
        }
        // Judged against the file as this session first saw it, when that shows no
        // weakening: dogfood workers were warned for deleting a probe test they had
        // added themselves a few calls before. A file the session created had nothing.
        let base = match self.guard.baseline.get(&path) {
            Some(b) => b.clone(),
            None => {
                if self.guard.baseline.len() < MAX_SHELL_BASELINES {
                    self.guard.baseline.insert(path.clone(), old_text.clone());
                }
                old_text.clone()
            }
        };
        let from_start_fine = base != old_text
            && self.policy.protected_by(&path).is_none()
            && !matches!(
                self.broker.ask(&Question::TestWeakened {
                    path: &path,
                    old: &base,
                    new: &new_text
                }),
                (Answer::Weakened { .. }, _)
            );
        if !self.on(Feature::Guard) {
            return;
        }
        let old = if from_start_fine { &base } else { &old_text };
        let Some(judged) = self.judge_test_edit(d, now, ev, &path, old, &new_text) else {
            return;
        };
        // Removed tests may reappear in another file the same command wrote: wait for
        // the command's end.
        if let Some(removed) = judged.removed {
            self.guard.held.push(Held {
                tool_use_id,
                path,
                judgment: judged.judgment,
                reasons: judged.reasons,
                old: judged.old,
                new: new_text,
                removed,
            });
            return;
        }
        self.warn_shell(d, now, judged.judgment, path, &judged.reasons);
    }

    /// The command that wrote the held files ended: warn for those whose removed tests
    /// did not turn up in another file, and withdraw the rest.
    fn release_held(&mut self, d: &mut Decision, now: u64) {
        for h in std::mem::take(&mut self.guard.held) {
            let moved: BTreeSet<ItemKey> = h
                .removed
                .iter()
                .filter(|k| self.guard.added.contains(*k))
                .cloned()
                .collect();
            if moved.is_empty() {
                self.warn_shell(d, now, h.judgment, h.path, &h.reasons);
                continue;
            }
            let old = items::strip(&h.old, &moved);
            match guard::assess_path(&h.path, &old, &h.new) {
                Answer::Weakened { reasons } => {
                    self.warn_shell(d, now, h.judgment, h.path, &reasons)
                }
                _ => self.withdrawn(d, now, vec![h.judgment], h.path),
            }
        }
    }

    fn warn_shell(&mut self, d: &mut Decision, now: u64, j: u64, path: String, reasons: &[String]) {
        let message = format!(
            "belay: the last command changed {path} and may have weakened tests: {}. If that was not intended, restore the file.",
            reasons.join("; ")
        );
        let action = ActionKind::Guard {
            path,
            decision: GuardDecision::Warn,
        };
        if self.act(d, now, j, action, message.clone()) {
            d.response.add_context.push(message);
        }
    }

    /// Downgrade the wanted guard decision to the `guard` setting and to what the agent
    /// supports. Call only while the guard is on.
    pub(super) fn effective_guard(&self, wanted: GuardDecision) -> GuardDecision {
        let wanted = match self.guard_setting() {
            GuardSetting::Warn => GuardDecision::Warn,
            _ => wanted,
        };
        match wanted {
            GuardDecision::Ask if !self.caps.ask => {
                if self.caps.deny {
                    GuardDecision::Deny
                } else {
                    GuardDecision::Warn
                }
            }
            GuardDecision::Deny if !self.caps.deny => GuardDecision::Warn,
            other => other,
        }
    }
}

#[cfg(test)]
mod tests;
