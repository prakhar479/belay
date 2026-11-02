//! Memory in sessions (U4b): the handoff a session leaves, the recall at its start, the
//! carry pack after a compaction, and fixes recalled from memories. The daemon reads and
//! writes the store; the controller gets memories as data and records what it gives.

use super::{Controller, Decision, Feature};
use crate::event::Event;
use crate::memory::{self, HostText, Kind, Memory};
use crate::perception::Failure;
use crate::record::{ActionKind, Body};

/// Guard lines a carry pack keeps, at most (the newest).
const MAX_GUARD_LINES: usize = 10;
/// Memories a session remembers using, at most.
const MAX_USED: usize = 20;

/// What the controller keeps for memory in this session.
#[derive(Debug, Default)]
pub(super) struct MemoryState {
    /// The project's and the user's memories, from the daemon before the start.
    candidates: Vec<Memory>,
    /// Memories this session was given, oldest first.
    used: Vec<Memory>,
    /// The latest test or build run's first failure; None once a run passed.
    open_failure: Option<Failure>,
    /// Guard blocks and the user's decisions on them, oldest first.
    guard: Vec<String>,
    /// Fix memories already recalled, by signature.
    fixes_recalled: std::collections::BTreeSet<String>,
    /// What the agent loads on its own: memories it states are not given again.
    host: Vec<HostText>,
}

impl MemoryState {
    /// Take up another run's conversation: its open failure and guard history.
    pub(super) fn take_conversation(&mut self, other: MemoryState) {
        self.open_failure = other.open_failure;
        self.guard = other.guard;
    }

    /// The state that decisions depend on, for `Controller::state_digest`.
    pub(super) fn digest(&self) -> String {
        let used: Vec<&str> = self.used.iter().map(|m| m.id.as_str()).collect();
        let host: Vec<(&str, String)> = self
            .host
            .iter()
            .map(|h| (h.label.as_str(), crate::text::sha256_hex(&h.text)))
            .collect();
        format!(
            "{used:?} {:?} {:?} {:?} {host:?}",
            self.open_failure, self.guard, self.fixes_recalled
        )
    }
}

/// A recall's reason, with the memories left out because the agent already loads them
/// joined on: still one sentence (rule 4).
fn with_left_out(reason: String, left_out: &[String]) -> String {
    match memory::host_left_out(left_out) {
        Some(s) => format!("{}; {s}", reason.trim_end_matches('.')),
        None => reason,
    }
}

impl Controller {
    /// The project's and the user's memories, for the recall at the start and for
    /// failures that match a fix. The daemon calls it before the session starts.
    pub fn set_memories(&mut self, memories: Vec<Memory>) {
        self.mem.candidates = memories;
    }

    /// What the agent loads on its own at its start (its instruction files, its own
    /// memory), as data. A memory one of them already states is not recalled again.
    pub fn set_host_context(&mut self, host: Vec<HostText>) {
        self.mem.host = host;
    }

    /// The handoff this session leaves now (at its end, or before a compaction), for the
    /// daemon to keep in `project`'s memory. None when there is nothing to hand over.
    pub fn handoff_memory(&self, project: &str, now_ms: u64) -> Option<Memory> {
        if !self.on(Feature::Memory) {
            return None;
        }
        let card = self.card(now_ms);
        memory::handoff_from(&card, self.mem.open_failure.as_ref(), project, now_ms)
    }

    fn project_key(&self) -> String {
        match self.learning.project() {
            "" => self.card(0).repo,
            p => p.to_string(),
        }
    }

    fn use_memory(&mut self, m: &Memory) {
        if self.mem.used.iter().any(|u| u.id == m.id) {
            return;
        }
        if self.mem.used.len() >= MAX_USED {
            self.mem.used.remove(0);
        }
        self.mem.used.push(m.clone());
    }

    /// At the start: the newest handoff another session left here and the best memories,
    /// within `recall_tokens`. Returns whether a handoff was given (the commons one is
    /// then left out, so the agent reads one handoff).
    pub(super) fn recall_at_start(&mut self, d: &mut Decision, cause: u64, now: u64) -> bool {
        if !self.on(Feature::Memory) || self.mem.candidates.is_empty() {
            return false;
        }
        let project = self.project_key();
        let candidates = std::mem::take(&mut self.mem.candidates);
        let (fresh, left_out) = memory::without_host(&candidates, &self.mem.host);
        let fresh: Vec<Memory> = fresh.into_iter().cloned().collect();
        let r = memory::recall(
            &fresh,
            &project,
            &self.identity.id,
            self.cfg.recall_tokens,
            now,
        );
        let gave_handoff = r.handoff.is_some();
        if r.is_empty() {
            self.mem.candidates = candidates;
            return false;
        }
        let ids = r.ids();
        let text = memory::recall_text(&r, now);
        let used: Vec<Memory> = r.handoff.into_iter().chain(r.memories).cloned().collect();
        let applied = self.deliver && self.caps.start_inject;
        let reason = with_left_out(
            format!(
                "{} memor{} for this project fit the recall budget.",
                ids.len(),
                if ids.len() == 1 { "y" } else { "ies" }
            ),
            &left_out,
        );
        let action = ActionKind::MemoryRecalled {
            ids,
            compact: false,
            text: text.clone(),
        };
        self.record(
            d,
            now,
            vec![cause],
            action,
            reason,
            applied,
            !self.caps.start_inject,
        );
        // Fixes stay for failures that match them.
        self.mem.candidates = candidates
            .into_iter()
            .filter(|m| m.kind == Kind::Fix)
            .collect();
        if applied {
            for m in &used {
                self.use_memory(m);
            }
            d.response.start_context.push(text);
        }
        applied && gave_handoff
    }

    /// The same conversation on a fresh context. After a compaction, the carry pack:
    /// what this session is on and was given that the summary may have dropped. It
    /// adds to the context; what came before stays as the agent has it (rule 3).
    pub(super) fn on_context_restarted(&mut self, d: &mut Decision, event: Event, now: u64) {
        let compact = matches!(&event, Event::ContextRestarted { source } if source == "compact");
        let cause = self.push(d, now, vec![], Body::Event { event });
        if !compact || !self.caps.compact_source {
            return;
        }
        let reason = "The conversation was compacted: this session's task, failure, and memories go with it.".to_string();
        self.carry_pack(d, cause, now, reason);
    }

    /// The carry pack: what this session is on and was given, for a context that may
    /// have lost it (a compaction, or a resumed conversation).
    pub(super) fn carry_pack(&mut self, d: &mut Decision, cause: u64, now: u64, reason: String) {
        if !self.on(Feature::Memory) {
            return;
        }
        // The agent loads its own files again on a fresh context: those go without us.
        let (used, left_out) = memory::without_host(&self.mem.used, &self.mem.host);
        let used: Vec<Memory> = used.into_iter().cloned().collect();
        let carry = memory::Carry {
            task: self.said.task.as_deref(),
            failure: self.mem.open_failure.as_ref(),
            guard: &self.mem.guard,
            used: &used,
            resumed: self.restored,
        };
        let Some(text) = memory::carry_text(&carry, self.cfg.recall_tokens) else {
            return;
        };
        let reason = with_left_out(reason, &left_out);
        let ids: Vec<String> = used.iter().map(|m| m.id.clone()).collect();
        let applied = self.deliver && self.caps.start_inject;
        let action = ActionKind::MemoryRecalled {
            ids,
            compact: true,
            text: text.clone(),
        };
        self.record(
            d,
            now,
            vec![cause],
            action,
            reason,
            applied,
            !self.caps.start_inject,
        );
        if applied {
            d.response.start_context.push(text);
        }
    }

    /// A test or build run: its first failure stays open until a run passes.
    pub(super) fn memory_on_run(&mut self, failures: &[Failure], passed: Option<bool>) {
        if let Some(f) = failures
            .iter()
            .find(|f| !f.summary_only)
            .or(failures.first())
        {
            self.mem.open_failure = Some(f.clone());
        } else if passed == Some(true) {
            self.mem.open_failure = None;
        }
    }

    /// A guard block, or the user's decision on one, for the carry pack.
    pub(super) fn memory_on_guard(&mut self, line: String) {
        if self.mem.guard.len() >= MAX_GUARD_LINES {
            self.mem.guard.remove(0);
        }
        self.mem.guard.push(line);
    }

    /// A learned fix was recalled for a failure: the session used that memory.
    pub(super) fn memory_used_fix(&mut self, fix: &crate::learn::KnownFix) {
        self.use_memory(&memory::fix_memory(fix));
    }

    /// A failure whose signature a Fix memory names (one kept with `belay memory add
    /// --kind fix`, or migrated): tell the agent, once per failure.
    pub(super) fn recall_fix_memory(
        &mut self,
        d: &mut Decision,
        cause: u64,
        failures: &[Failure],
        now: u64,
    ) {
        let hit = failures.iter().filter(|f| !f.summary_only).find_map(|f| {
            if self.mem.fixes_recalled.contains(&f.signature) {
                return None;
            }
            self.mem
                .candidates
                .iter()
                .filter(|m| m.is_live() && m.kind == Kind::Fix)
                .filter(|m| m.signature.as_deref() == Some(f.signature.as_str()))
                .max_by_key(|m| (m.vouched(), m.created_ms))
                .map(|m| (f.clone(), m.clone()))
        });
        let Some((failure, m)) = hit else { return };
        self.mem.fixes_recalled.insert(failure.signature.clone());
        let text = format!(
            "belay: memory {} says how this failure was fixed before: {} Check that it fits here.",
            m.id, m.text
        );
        let applied = self.says();
        let reason = format!(
            "Memory {} names the signature of `{}`.",
            m.id,
            crate::text::truncate_chars(&failure.summary(), 80)
        );
        let action = ActionKind::Recall {
            signature: failure.signature,
            session: m.id.clone(),
            text: text.clone(),
        };
        self.record(
            d,
            now,
            vec![cause],
            action,
            reason,
            applied,
            !self.caps.inject_context,
        );
        if applied {
            self.use_memory(&m);
            d.response.add_context.push(text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::Capabilities;
    use crate::controller::{Config, Response};
    use crate::memory::{Provenance, Scope};
    use crate::record::Entry;
    use serde_json::json;

    const PYTEST: &str = include_str!("../../../../fixtures/outputs/pytest_fail.txt");

    fn ctl(cfg: Config) -> Controller {
        let mut c = Controller::new(cfg, Capabilities::CLAUDE_CODE);
        c.set_identity(crate::commons::Identity {
            id: "s2".into(),
            default_name: "repo-2".into(),
            ..Default::default()
        });
        c.set_learning("/repo", "/repo", vec![], vec![]);
        c
    }

    fn start() -> Event {
        Event::SessionStarted {
            agent: "claude-code".into(),
            cwd: "/repo".into(),
            transcript_path: None,
            mode: None,
        }
    }

    fn failing_run() -> Event {
        Event::ToolCompleted {
            tool_use_id: "t".into(),
            tool: "Bash".into(),
            kind: crate::event::ToolKind::Shell,
            input: json!({ "command": "pytest" }),
            output: PYTEST.into(),
            failed: true,
        }
    }

    fn mem(id: &str, kind: Kind, session: Option<&str>, text: &str, at: u64) -> Memory {
        let mut m = Memory::new(
            id,
            Scope::Project,
            kind,
            Provenance::Observed,
            Some("/repo"),
            text,
            &[],
            &[],
            at,
        );
        m.session = session.map(String::from);
        m
    }

    fn recalled(entries: &[Entry]) -> Vec<(Vec<String>, bool, bool)> {
        entries
            .iter()
            .filter_map(|e| match &e.body {
                Body::Action {
                    action: ActionKind::MemoryRecalled { ids, compact, .. },
                    applied,
                    ..
                } => Some((ids.clone(), *compact, *applied)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_start_gets_the_last_handoff_and_memories_but_not_its_own_or_fixes() {
        let mut c = ctl(Config {
            commons: true,
            ..Config::default()
        });
        c.set_memories(vec![
            mem(
                "m-1",
                Kind::Handoff,
                Some("s1"),
                "From repo-1. Open failure: test_a.",
                1,
            ),
            mem("m-2", Kind::Handoff, Some("s2"), "From repo-2. its own.", 5),
            mem("m-3", Kind::Gotcha, None, "pytest needs DB_URL", 2),
            mem("m-4", Kind::Fix, None, "fixed by a.py", 3),
        ]);
        // The commons handoff would say the same: the agent reads one.
        c.set_handoff(Some(crate::commons::Handoff {
            from: "repo-1".into(),
            ended_ms: 1,
            text: Some("done".into()),
            task: None,
            files: vec![],
            last_test: None,
            looping: false,
            blocked: false,
        }));
        let d = c.handle(start(), 10);
        let ctx = d.response.start_context.join("\n");
        assert!(
            ctx.contains("The last session's handoff (m-1") && ctx.contains("test_a"),
            "{ctx}"
        );
        assert!(
            ctx.contains("m-3 (gotcha, by belay): pytest needs DB_URL"),
            "{ctx}"
        );
        assert!(!ctx.contains("m-2") && !ctx.contains("m-4"), "{ctx}");
        assert!(d
            .response
            .add_context
            .iter()
            .all(|t| !t.contains("last session")));
        assert_eq!(
            recalled(&d.entries),
            [(vec!["m-1".to_string(), "m-3".to_string()], false, true)]
        );
    }

    #[test]
    fn memory_off_or_a_small_budget_recalls_less() {
        let all = vec![
            mem("m-1", Kind::Handoff, Some("s1"), &"h ".repeat(100), 1),
            mem("m-3", Kind::Gotcha, None, "pytest needs DB_URL", 2),
        ];
        let mut c = ctl(Config {
            memory: false,
            ..Config::default()
        });
        c.set_memories(all.clone());
        let d = c.handle(start(), 10);
        assert!(d.response.start_context.is_empty() && recalled(&d.entries).is_empty());

        let mut c = ctl(Config {
            recall_tokens: 20,
            ..Config::default()
        });
        c.set_memories(all);
        let d = c.handle(start(), 10);
        assert_eq!(
            recalled(&d.entries),
            [(vec!["m-3".to_string()], false, true)]
        );
    }

    #[test]
    fn a_compaction_carries_the_task_failure_guard_and_memories() {
        let mut c = ctl(Config::default());
        c.set_memories(vec![mem(
            "m-3",
            Kind::Gotcha,
            None,
            "pytest needs DB_URL",
            2,
        )]);
        c.handle(start(), 1);
        c.handle(
            Event::CardSet {
                name: None,
                task: Some("fix the orders api".into()),
                note: None,
                offers: None,
                topics: None,
                handoff: None,
            },
            2,
        );
        c.handle(failing_run(), 3);
        c.memory_on_guard("a-9 blocked an edit of tests/test_api.py.".into());
        // Another source (`clear` maps to a new conversation, not here): nothing.
        let d = c.handle(
            Event::ContextRestarted {
                source: "resume".into(),
            },
            4,
        );
        assert_eq!(d.response, Response::default());
        let d = c.handle(
            Event::ContextRestarted {
                source: "compact".into(),
            },
            5,
        );
        let ctx = d.response.start_context.join("\n");
        assert!(ctx.starts_with("belay: carried over"), "{ctx}");
        assert!(ctx.contains("- Task: fix the orders api"), "{ctx}");
        assert!(
            ctx.contains("- Open failure: tests/test_api.py::test_get_order"),
            "{ctx}"
        );
        assert!(ctx.contains("- Guard: a-9 blocked"), "{ctx}");
        assert!(ctx.contains("- m-3 (gotcha): pytest needs DB_URL"), "{ctx}");
        assert_eq!(recalled(&d.entries)[0].0, ["m-3"]);
        assert!(recalled(&d.entries)[0].1, "compact");

        // An agent that does not say why its context restarted: no carry pack.
        let mut caps = Capabilities::CLAUDE_CODE;
        caps.compact_source = false;
        let mut c = Controller::new(Config::default(), caps);
        c.handle(start(), 1);
        c.handle(failing_run(), 2);
        let d = c.handle(
            Event::ContextRestarted {
                source: "compact".into(),
            },
            3,
        );
        assert!(d.response.start_context.is_empty());
    }

    #[test]
    fn the_handoff_names_the_open_failure_until_a_run_passes() {
        let mut c = ctl(Config::default());
        c.handle(start(), 1);
        c.handle(failing_run(), 2);
        let h = c.handoff_memory("/repo", 3).expect("a handoff");
        assert_eq!(
            (h.kind, h.provenance),
            (Kind::Handoff, Provenance::Observed)
        );
        assert_eq!(h.session.as_deref(), Some("s2"));
        assert!(h.signature.is_some());
        assert!(
            h.text.starts_with("From repo-2.")
                && h.text.contains("Last test: `pytest` failed")
                && h.text
                    .contains("Open failure: tests/test_api.py::test_get_order"),
            "{}",
            h.text
        );
        c.memory_on_run(&[], Some(true));
        let h = c.handoff_memory("/repo", 4).unwrap();
        assert!(!h.text.contains("Open failure"), "{}", h.text);
        let off = ctl(Config {
            memory: false,
            ..Config::default()
        });
        assert!(off.handoff_memory("/repo", 4).is_none());
    }

    #[test]
    fn a_fix_memory_with_the_failures_signature_is_recalled_once() {
        let sig = crate::perception::extract_failures(PYTEST)[0]
            .signature
            .clone();
        let mut fix = mem("m-7", Kind::Fix, None, "set DB_URL in conftest.py", 1);
        fix.signature = Some(sig);
        let mut c = ctl(Config::default());
        c.set_memories(vec![fix]);
        c.handle(start(), 1);
        let d = c.handle(failing_run(), 2);
        let said = d.response.add_context.join("\n");
        assert!(
            said.contains("memory m-7 says how this failure was fixed before: set DB_URL"),
            "{said}"
        );
        let d = c.handle(failing_run(), 3);
        assert!(!d.response.add_context.join("\n").contains("m-7"), "once");
    }

    #[test]
    fn a_memory_the_host_already_states_is_not_recalled_or_carried() {
        let mut c = ctl(Config::default());
        c.set_memories(vec![
            mem("m-3", Kind::Gotcha, None, "pytest needs DB_URL", 2),
            mem(
                "m-5",
                Kind::Fact,
                None,
                "Run make check before you say a task is done.",
                3,
            ),
        ]);
        c.set_host_context(vec![HostText {
            label: "CLAUDE.md".into(),
            text: "# Rules\n- run `make check` before you say a task is done\n".into(),
        }]);
        let d = c.handle(start(), 10);
        let ctx = d.response.start_context.join("\n");
        assert!(ctx.contains("m-3") && !ctx.contains("m-5"), "{ctx}");
        assert_eq!(
            recalled(&d.entries),
            [(vec!["m-3".to_string()], false, true)]
        );
        let reason = d
            .entries
            .iter()
            .find_map(|e| match &e.body {
                Body::Action {
                    action: ActionKind::MemoryRecalled { .. },
                    reason,
                    ..
                } => Some(reason.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            reason,
            "1 memory for this project fit the recall budget; 1 memory was left out: CLAUDE.md already says it."
        );

        // A memory given in the session, which the agent's files state: the carry pack
        // leaves it to them.
        c.mem.used.push(mem(
            "m-5",
            Kind::Fact,
            None,
            "Run make check before you say a task is done.",
            3,
        ));
        let d = c.handle(
            Event::ContextRestarted {
                source: "compact".into(),
            },
            11,
        );
        let ctx = d.response.start_context.join("\n");
        assert!(ctx.contains("m-3") && !ctx.contains("m-5"), "{ctx}");
        assert_eq!(recalled(&d.entries)[0].0, ["m-3"]);
    }
}
