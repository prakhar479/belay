//! Commons, learning, and card bookkeeping for the session (C1-C4, F2).

use super::*;

impl Controller {
    /// Record a finding this session published (C3). The daemon stores it.
    pub fn shared(&mut self, f: &crate::commons::Finding, now_ms: u64) -> Decision {
        let mut d = Decision::default();
        let event = Event::FindingShared {
            id: f.id.clone(),
            kind: f.kind.clone(),
            text: f.text.clone(),
            tags: f.tags.clone(),
        };
        self.push(&mut d, now_ms, vec![], Body::Event { event });
        d
    }

    /// Record what this session asked the commons and was given (C3).
    pub fn asked(&mut self, query: &str, ids: Vec<String>, now_ms: u64) -> Decision {
        let mut d = Decision::default();
        let event = Event::FindingsRead {
            query: crate::text::truncate_chars(&crate::text::redact(query), 300),
            ids,
        };
        self.push(&mut d, now_ms, vec![], Body::Event { event });
        d
    }

    /// A commons message to `recipients` (card names, resolved by the daemon from `to`).
    /// Decides the kind (given; an answer to whoever asked last; a question when the
    /// text asks; else a notice), cleans the text, applies the rate limits, and records
    /// it either way. Ok: the kind and text to send.
    pub fn send(
        &mut self,
        to: &str,
        recipients: Vec<String>,
        kind: Option<String>,
        text: &str,
        now_ms: u64,
    ) -> (Decision, Result<(String, String), String>) {
        use crate::commons::{KINDS, MAX_MESSAGE};
        let mut d = Decision::default();
        let text = crate::text::truncate_chars(&crate::text::redact(text.trim()), MAX_MESSAGE);
        let kind = match kind {
            Some(k) => k,
            None if recipients.len() == 1
                && self.asked_by.as_deref() == Some(recipients[0].as_str()) =>
            {
                "answer".into()
            }
            None if text.trim_end().ends_with('?') => "question".into(),
            None => "notice".into(),
        };
        let verdict = if !KINDS.contains(&kind.as_str()) {
            Err(format!(
                "`{kind}` is not a message kind: use {}.",
                KINDS.join(", ")
            ))
        } else if text.is_empty() {
            Err("a message needs text.".into())
        } else if recipients.is_empty() {
            Err(format!(
                "no live session is `{to}` (`belay sessions` lists them)."
            ))
        } else {
            self.limiter.take(&recipients, now_ms)
        };
        if verdict.is_ok() && kind == "answer" {
            self.asked_by = None;
        }
        let event = Event::MessageSent {
            to: to.to_string(),
            recipients,
            kind: kind.clone(),
            text: text.clone(),
            refused: verdict.as_ref().err().cloned(),
        };
        self.push(&mut d, now_ms, vec![], Body::Event { event });
        (d, verdict.map(|()| (kind, text)))
    }

    /// The card at `now` (deterministic for the same events).
    pub fn card(&self, now_ms: u64) -> crate::commons::Card {
        let (started_ms, agent, cwd) =
            self.started
                .clone()
                .unwrap_or((0, String::new(), String::new()));
        let (top, repo) = match self.fleet.workspace() {
            Some((top, common)) => (
                top.to_string(),
                common
                    .trim_end_matches('/')
                    .trim_end_matches("/.git")
                    .to_string(),
            ),
            None => (cwd.clone(), cwd),
        };
        let gap = self.cfg.idle_gap_minutes * 60_000;
        let name = self
            .said
            .name
            .clone()
            .unwrap_or_else(|| self.identity.default_name.clone());
        crate::commons::Card {
            card: crate::commons::CARD_FORMAT,
            id: self.identity.id.clone(),
            name,
            agent,
            mode: self.paused_label(),
            started_ms,
            repo,
            top,
            fleet: self.identity.fleet.clone(),
            slot: self.identity.slot.clone(),
            said: self.said.clone(),
            status: crate::commons::status(&self.facts, now_ms, gap),
            // The command goes to other sessions' boards: no secrets.
            last_test: self.last_test.clone().map(|mut t| {
                t.command = crate::text::redact(&t.command).into_owned();
                t
            }),
            files: self.fleet.recent_edits(crate::commons::MAX_FILES),
            tool_calls: self.card_counts.0,
            loops: self.card_counts.1,
            blocks: self.card_counts.2,
            last_activity_ms: self.facts.last_activity_ms,
            ended: false,
            sandbox: self.sandbox.clone(),
        }
    }

    /// What this hook's event and decision say about the session, for its card.
    pub(super) fn note_for_card(&mut self, event: &Event, d: &Decision, now: u64) {
        match event {
            Event::Peer { .. }
            | Event::Workspace { .. }
            | Event::Message { .. }
            | Event::CardSet { .. } => {}
            _ => self.facts.last_activity_ms = now,
        }
        match event {
            Event::PromptSubmitted { .. } => {
                self.facts.turn_open = true;
                self.verify.on_prompt();
            }
            Event::TurnEnded => self.facts.turn_open = false,
            Event::ToolCompleted { failed, .. } => {
                self.card_counts.0 += 1;
                if !failed {
                    self.facts.last_progress_ms = now;
                }
            }
            Event::FileEdited { .. } => self.facts.last_progress_ms = now,
            _ => {}
        }
        self.note_actions(d, now);
    }

    /// Steers and applied blocks in `d`, for the card.
    pub(super) fn note_actions(&mut self, d: &Decision, now: u64) {
        for e in &d.entries {
            if let Body::Action {
                id,
                action,
                applied,
                ..
            } = &e.body
            {
                match action {
                    ActionKind::Steer { .. } => {
                        self.card_counts.1 += 1;
                        self.facts.last_steer_ms = Some(now);
                    }
                    ActionKind::Guard { decision, path }
                        if *applied && *decision != GuardDecision::Warn =>
                    {
                        self.card_counts.2 += 1;
                        self.facts.last_deny_ms = Some(now);
                        self.memory_on_guard(format!("{id} blocked an edit of {path}."));
                    }
                    _ => {}
                }
            }
        }
    }
}
