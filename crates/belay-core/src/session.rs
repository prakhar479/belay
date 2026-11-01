//! Session model: the small, readable state that decisions depend on.
//! Scope is one session. Nothing carries over.

use std::collections::{BTreeMap, BTreeSet};

use crate::guard::GuardDecision;
use crate::perception::OutputKind;

pub const MAX_LEVEL: u8 = 3;
pub const DEFAULT_LEVEL: u8 = 2;

/// One knob change, with a one-sentence reason for the ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub knob: String,
    pub from: String,
    pub to: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct SessionModel {
    /// Wanted test-guard strictness. The controller downgrades it per capabilities.
    pub guard: GuardDecision,
    levels: BTreeMap<OutputKind, u8>,
    unreopened: BTreeMap<OutputKind, u32>,
    tighten_after: u32,
    reads: Reads,
}

/// Most distinct (path, content) pairs `Reads` remembers between compactions. Past it,
/// new pairs are counted but not remembered: a repeat of one of them goes unseen.
const MAX_TRACKED_READS: usize = 4096;

/// File reads since the last compaction, by path and content hash (U5.1). A read is
/// repeated when the same content of the same path was read before, with no compaction
/// in between: the agent already had it in context. Data for a later step (U5.2); belay
/// does nothing with it yet. The report replays the ledger through this too.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reads {
    seen: BTreeSet<(String, String)>,
    /// Reads in the session.
    pub total: u64,
    /// Reads of content already read since the last compaction.
    pub repeated: u64,
}

impl Reads {
    /// Count a read of `path` whose content hashes to `hash`. True if it repeats one.
    pub fn on_read(&mut self, path: &str, hash: &str) -> bool {
        self.total += 1;
        let key = (path.to_string(), hash.to_string());
        let repeated = self.seen.contains(&key);
        if repeated {
            self.repeated += 1;
        } else if self.seen.len() < MAX_TRACKED_READS {
            self.seen.insert(key);
        }
        repeated
    }

    /// The context was compacted or replaced: earlier reads are no longer in it.
    pub fn on_compacted(&mut self) {
        self.seen.clear();
    }
}

impl SessionModel {
    pub fn new(tighten_after: u32) -> Self {
        Self {
            guard: GuardDecision::Ask,
            // Only test, build, and install output starts on: summarizing code, diffs, or
            // search hits the agent asked to read would hide what it needs (C1; R1.5 and
            // R1.6 will decide what else is safe).
            levels: OutputKind::ALL
                .iter()
                .map(|k| {
                    (
                        *k,
                        if k.shaped_by_default() {
                            DEFAULT_LEVEL
                        } else {
                            0
                        },
                    )
                })
                .collect(),
            unreopened: BTreeMap::new(),
            tighten_after,
            reads: Reads::default(),
        }
    }

    pub fn shape_level(&self, kind: OutputKind) -> u8 {
        // Every kind is in `levels`; an unknown one is not shaped (C1).
        self.levels.get(&kind).copied().unwrap_or(0)
    }

    pub fn set_level(&mut self, kind: OutputKind, level: u8) {
        self.levels.insert(kind, level);
    }

    pub fn levels(&self) -> &BTreeMap<OutputKind, u8> {
        &self.levels
    }

    pub fn reads(&self) -> &Reads {
        &self.reads
    }

    pub fn reads_mut(&mut self) -> &mut Reads {
        &mut self.reads
    }

    /// Call after a summary reached the agent. Tightens after `tighten_after`
    /// summaries of a kind with no re-open.
    pub fn on_shaped(&mut self, kind: OutputKind) -> Option<Change> {
        let count = {
            let n = self.unreopened.entry(kind).or_insert(0);
            *n += 1;
            *n
        };
        if count < self.tighten_after {
            return None;
        }
        self.unreopened.insert(kind, 0);
        let from = self.shape_level(kind);
        if from == 0 || from >= MAX_LEVEL {
            return None;
        }
        let to = from + 1;
        self.levels.insert(kind, to);
        Some(Change {
            knob: format!("shape_level.{}", kind.label()),
            from: from.to_string(),
            to: to.to_string(),
            reason: format!(
                "Tightened {} summaries: the agent did not re-open the last {} summaries.",
                kind.label(),
                self.tighten_after
            ),
        })
    }

    /// Call when the agent re-opens a summary. Loosens that kind one level.
    /// `what` completes "the agent …", for example "re-opened o-4".
    pub fn on_reopen(&mut self, kind: OutputKind, what: &str) -> Option<Change> {
        self.unreopened.insert(kind, 0);
        let from = self.shape_level(kind);
        if from == 0 {
            return None;
        }
        let to = from - 1;
        self.levels.insert(kind, to);
        let verb = if to == 0 { "Stopped" } else { "Loosened" };
        Some(Change {
            knob: format!("shape_level.{}", kind.label()),
            from: from.to_string(),
            to: to.to_string(),
            reason: format!("{verb} {} summaries: the agent {what}.", kind.label()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tightens_after_quiet_summaries() {
        let mut s = SessionModel::new(3);
        assert!(s.on_shaped(OutputKind::Test).is_none());
        assert!(s.on_shaped(OutputKind::Test).is_none());
        let c = s.on_shaped(OutputKind::Test).unwrap();
        assert_eq!((c.from.as_str(), c.to.as_str()), ("2", "3"));
        assert_eq!(s.shape_level(OutputKind::Test), 3);
        assert_eq!(s.shape_level(OutputKind::Build), DEFAULT_LEVEL);
    }

    #[test]
    fn same_content_read_again_is_repeated_until_a_compaction() {
        let mut r = Reads::default();
        assert!(!r.on_read("a.rs", "h1"));
        assert!(!r.on_read("b.rs", "h1"));
        assert!(r.on_read("a.rs", "h1"));
        // The file changed: a new read, not a repeat.
        assert!(!r.on_read("a.rs", "h2"));
        r.on_compacted();
        assert!(!r.on_read("a.rs", "h1"));
        assert!(r.on_read("a.rs", "h1"));
        assert_eq!((r.total, r.repeated), (6, 2));
    }

    #[test]
    fn reopen_loosens_and_resets_count() {
        let mut s = SessionModel::new(2);
        s.on_shaped(OutputKind::Test);
        let c = s.on_reopen(OutputKind::Test, "re-opened o-4").unwrap();
        assert!(c.reason.contains("o-4"));
        assert_eq!(s.shape_level(OutputKind::Test), 1);
        assert!(s.on_shaped(OutputKind::Test).is_none());
    }
}
