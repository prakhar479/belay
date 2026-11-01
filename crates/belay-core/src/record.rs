//! Ledger records. The ledger is append-only and is the single source of truth.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::broker::{Answer, Source};
use crate::event::Event;
use crate::guard::GuardDecision;
use crate::perception::OutputKind;

fn is_false(b: &bool) -> bool {
    !b
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub seq: u64,
    pub ts_ms: u64,
    /// Sequence numbers of the entries that caused this one. `belay why` follows them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<u64>,
    pub body: Body,
}

/// Ledger format version. A ledger without a `format` entry is version 1.
/// Bump it only for a change that older readers would misread, and say so.
/// 2 (0.2): no modes. `FeatureChanged` replaces `ModeChanged`; `SessionStarted.mode`
/// is gone (both still read).
pub const LEDGER_FORMAT: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    /// Always the first entry: seq 0, or in a resumed run's ledger (U8) the next seq after
    /// the earlier run's. Keep this shape stable: readers check it first.
    Format { version: u32 },
    /// An event as received. Large text is replaced with `<blob:HASH>`.
    Event { event: Event },
    /// A stored original tool output. `id` is what `belay show` takes.
    Output {
        id: String,
        blob: String,
        output_kind: OutputKind,
        lines: usize,
        tokens: usize,
    },
    Judgment {
        question: String,
        version: u32,
        subject: String,
        answer: Answer,
        source: Source,
    },
    /// `applied` is false when the agent lacks the capability (`unsupported`), or when the
    /// reply reached no one (D9). 0.1 ledgers also have it false in look mode.
    Action {
        id: String,
        action: ActionKind,
        reason: String,
        applied: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        unsupported: bool,
    },
    Adaptation {
        knob: String,
        from: String,
        to: String,
        reason: String,
    },
    /// A 0.1 mode change (`look`, `optimize`). Read from old ledgers; never written.
    ModeChanged { from: String, to: String },
    /// The user paused or resumed a feature (`belay pause`, `belay resume`). `feature`
    /// is a config key or `all`; `from` and `to` are values or `paused`.
    FeatureChanged {
        feature: String,
        from: String,
        to: String,
        by: String,
    },
    /// The user allowed, once, the edit that guard action `action` blocked.
    Override { action: String },
    /// The reply carrying these applied actions came after the shim gave up, so the
    /// agent never got them (D9). Readers treat them as not applied.
    Undelivered {
        actions: Vec<String>,
        reason: String,
    },
    /// Config values that differ from the defaults, recorded at session start.
    Config { overrides: BTreeMap<String, String> },
    /// A failure went away after edits: a fix this session learned (F2, L1).
    Learned {
        signature: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        test: Option<String>,
        command: String,
        files: Vec<String>,
        /// What the fix changed: a short, redacted diff of the edits that landed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        change: Option<String>,
    },
    /// The working tree kept as a private git commit (`refs/belay/<session>/<id>`), at
    /// the session's start (`t-0`) and each turn's end (`t-N`) (U6.2). With `undo`, the
    /// user's `belay undo` back to snapshot `id`; `commit` is then the tree as it was
    /// before the undo, so the undo can be undone. `files`: what changed since the
    /// previous snapshot, or what the undo restored or removed.
    Snapshot {
        id: String,
        commit: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
        reason: String,
        #[serde(default, skip_serializing_if = "is_false")]
        undo: bool,
    },
    /// This session continues an earlier one's conversation (U8): its state was restored
    /// from `from_session`'s ledger, up to entry `from_seq`. Ids go on after that ledger's.
    /// `conversation`: the agent's id for it, when known. Readers follow the link to show
    /// one story across runs.
    Resumed {
        from_session: String,
        from_seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conversation: Option<String>,
    },
    /// What the user was told about this hook's actions (at most one per hook; rule 5:
    /// never the model). `shown`: the report setting and the agent let it reach the
    /// user; `belay watch` shows every notice. Causes: the action it is about.
    Notice {
        #[serde(flatten)]
        notice: Notice,
        #[serde(default, skip_serializing_if = "is_false")]
        shown: bool,
    },
}

/// Read actions that an `undelivered` entry names as not applied, with a note in the
/// reason (D9). The ledger itself is never changed.
pub fn settle_undelivered(entries: &mut [Entry]) {
    let mut notes: BTreeMap<String, String> = BTreeMap::new();
    for e in entries.iter() {
        if let Body::Undelivered { actions, reason } = &e.body {
            // Late replies (D9) and rewrites the agent ignored (I6, F8) both land here.
            let note = if reason.contains("timeout") || reason == "late" {
                " [not delivered: the reply came too late]".to_string()
            } else {
                format!(" [not applied: {}]", reason.trim_end_matches('.'))
            };
            for a in actions {
                notes.insert(a.clone(), note.clone());
            }
        }
    }
    if notes.is_empty() {
        return;
    }
    for e in entries.iter_mut() {
        if let Body::Action {
            id,
            applied,
            reason,
            ..
        } = &mut e.body
        {
            if let Some(note) = notes.get(id).filter(|_| *applied) {
                *applied = false;
                reason.push_str(note);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionKind {
    Shape {
        output_id: String,
        from_tokens: usize,
        to_tokens: usize,
        from_lines: usize,
        to_lines: usize,
    },
    Steer {
        trigger: SteerTrigger,
        text: String,
    },
    Guard {
        path: String,
        decision: GuardDecision,
    },
    /// A Bash call run through `belay run`, so its output can be summarized even on failure.
    Wrap {
        tool_use_id: String,
    },
    /// Tell the agent it may retry a call that auto mode denied because belay had
    /// wrapped it in `belay run` (R3.7). The retry runs unwrapped.
    Retry {
        tool_use_id: String,
    },
    /// A command the agent may not run (it would lift a guard).
    BlockCommand {
        command: String,
    },
    /// Tell the agent at its start that other belay sessions are on this machine, and
    /// how to find them and say what it does (commons, C1).
    Commons {
        others: usize,
        here: usize,
        /// Findings published for this repo (C3).
        #[serde(default)]
        findings: usize,
        text: String,
    },
    /// Tell the agent at its start what the last session in this repo left (C4).
    Handoff {
        from: String,
        text: String,
    },
    /// Tell the agent how another session fixed the failure it hits again (F2, L3).
    Recall {
        signature: String,
        session: String,
        text: String,
    },
    /// The project's notes, kept by the user, at session start (F2, L5).
    Notes {
        count: usize,
        text: String,
    },
    /// Memories put in the agent's context (U4b): at its start (a handoff and the best
    /// memories), or after a compaction (the carry pack, `compact`).
    MemoryRecalled {
        ids: Vec<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        compact: bool,
        text: String,
    },
    /// Tell the agent what another belay session did that affects it (F1).
    Fleet {
        notice: crate::fleet::NoticeKind,
        peer: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "on", rename_all = "snake_case")]
pub enum SteerTrigger {
    /// One steer covers every failure that reached the threshold in the same run.
    Loop {
        signatures: Vec<String>,
        /// First occurrence of the oldest failure to the steer, idle gaps capped
        /// (`idle_gap_minutes`). None in older ledgers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    Cycle {
        path: String,
    },
}

/// A note for the user about what belay did or saw. Reporting shows it through
/// user-only channels; it never reaches the model (rule 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub level: NoticeLevel,
    pub text: String,
    /// The action id this notice is about, if any (`a-N`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeLevel {
    /// belay acted on the session.
    Act,
    /// Something the user should look at.
    Warn,
    Info,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undelivered_actions_read_as_not_applied() {
        let action = |id: &str| Entry {
            seq: 0,
            ts_ms: 0,
            causes: vec![],
            body: Body::Action {
                id: id.into(),
                action: ActionKind::Wrap {
                    tool_use_id: "t".into(),
                },
                reason: "Wrapped.".into(),
                applied: true,
                unsupported: false,
            },
        };
        let late = Entry {
            seq: 2,
            ts_ms: 0,
            causes: vec![0],
            body: Body::Undelivered {
                actions: vec!["a-1".into()],
                reason: "late".into(),
            },
        };
        let mut entries = vec![action("a-1"), action("a-2"), late];
        settle_undelivered(&mut entries);
        assert!(
            matches!(&entries[0].body, Body::Action { applied: false, reason, .. } if reason.ends_with("too late]"))
        );
        assert!(matches!(
            &entries[1].body,
            Body::Action { applied: true, .. }
        ));
    }
}
