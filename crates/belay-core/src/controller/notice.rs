//! Notices: what belay did, told to the user (rule 5: never to the model). Each applied
//! action makes at most one candidate; a hook gets at most one notice, the most
//! important, with the rest counted ("+2 more summaries"). The `report` setting decides
//! which levels the user sees; every notice is recorded, shown or not, so `belay watch`
//! and `belay report` can tell the whole story.

use super::{Controller, Decision, ReportLevel};
use crate::guard::GuardDecision;
use crate::record::{ActionKind, Body, Notice, NoticeLevel, SteerTrigger};

impl NoticeLevel {
    fn rank(self) -> u8 {
        match self {
            NoticeLevel::Info => 0,
            NoticeLevel::Act => 1,
            NoticeLevel::Warn => 2,
        }
    }
}

impl ReportLevel {
    /// The report setting lets the user see a notice at this level.
    pub fn shows(self, level: NoticeLevel) -> bool {
        match self {
            ReportLevel::Quiet => level == NoticeLevel::Warn,
            ReportLevel::Normal => level != NoticeLevel::Info,
            ReportLevel::Verbose => true,
        }
    }
}

impl Notice {
    /// End a Warn notice about an action with how to say belay was wrong. Without
    /// flags belay cannot learn its false positives, and nobody flagged anything while
    /// the command was not in front of them (retro 2026-11-02). Adding it twice is a
    /// no-op, so a reader of an older ledger can call it too.
    pub fn add_flag_hint(&mut self) {
        let Some(id) = self
            .action
            .as_deref()
            .filter(|_| self.level == NoticeLevel::Warn)
        else {
            return;
        };
        let hint = format!(" Wrong? `belay flag {id}`");
        if !self.text.ends_with(&hint) {
            self.text.push_str(&hint);
        }
    }
}

/// The notice for one applied action, and a plural noun for counting repeats.
pub fn notice_for(
    id: &str,
    action: &ActionKind,
    reason: &str,
    verbose: bool,
) -> (Notice, &'static str) {
    use NoticeLevel::*;
    let (level, text, noun) = match action {
        ActionKind::Guard { path, decision } => match decision {
            GuardDecision::Deny => (
                Warn,
                format!("blocked an edit that may weaken tests in {path}. To let it through once: `belay allow {id}`."),
                "blocked edits",
            ),
            GuardDecision::Ask => (
                Warn,
                format!("asked before an edit that may weaken tests in {path}."),
                "guard checks",
            ),
            GuardDecision::Warn => (
                Warn,
                format!("warned the agent: a change may weaken tests in {path}."),
                "guard warnings",
            ),
        },
        ActionKind::BlockCommand { command } => (
            Warn,
            format!(
                "blocked `{}`: only the user may lift a guard.",
                crate::text::truncate_chars(command, 60)
            ),
            "blocked commands",
        ),
        ActionKind::Steer {
            trigger: SteerTrigger::Loop { signatures, .. },
            ..
        } => (
            Warn,
            format!(
                "the agent is looping: {} failure(s) came back after its fixes. belay told it to change approach.",
                signatures.len()
            ),
            "loop warnings",
        ),
        ActionKind::Steer {
            trigger: SteerTrigger::Cycle { path },
            ..
        } => (
            Warn,
            format!("{path} went back to an earlier state. belay told the agent."),
            "loop warnings",
        ),
        ActionKind::Shape {
            output_id,
            from_tokens,
            to_tokens,
            from_lines,
            to_lines,
        } => {
            let mut text =
                format!("summarized output: {from_lines} → {to_lines} lines (`belay show {output_id}`).");
            if verbose {
                let kept = from_tokens.saturating_sub(*to_tokens);
                text.push_str(&format!(" {kept} tokens kept out."));
            }
            (Act, text, "summaries")
        }
        ActionKind::Recall { session, .. } => (
            Act,
            format!("told the agent how session {session} fixed this failure before."),
            "recalls",
        ),
        ActionKind::MemoryRecalled { ids, compact, .. } => (
            Act,
            if *compact {
                format!(
                    "gave the agent back its task and {} memor{}.",
                    ids.len(),
                    if ids.len() == 1 { "y" } else { "ies" }
                )
            } else {
                format!(
                    "gave the agent {} memor{} from earlier sessions.",
                    ids.len(),
                    if ids.len() == 1 { "y" } else { "ies" }
                )
            },
            "recalls",
        ),
        ActionKind::Fleet { notice, peer, .. } => (
            Act,
            format!("told the agent about a {} from {peer}.", notice.label()),
            "fleet notices",
        ),
        ActionKind::Notes { count, .. } => (
            Act,
            format!("gave the agent the project's {count} note(s)."),
            "notes",
        ),
        ActionKind::Handoff { from, .. } => (
            Act,
            format!("gave the agent the handoff {from} left."),
            "handoffs",
        ),
        ActionKind::Routine { .. } => (Info, "pointed the agent at belay's routine.".into(), "routine lines"),
        ActionKind::Commons { .. } => (Act, reason.trim_end_matches('.').to_string() + ".", "commons notes"),
        ActionKind::Verify { command, .. } if command.is_empty() => (
            Act,
            "source changed and no test or check ran: belay asked the agent to run the tests before it finishes.".into(),
            "verify requests",
        ),
        ActionKind::Verify { command, .. } => (
            Act,
            format!(
                "files changed since the last passing `{}`: belay asked the agent to run it before it finishes.",
                crate::text::truncate_chars(command, 60)
            ),
            "verify requests",
        ),
        ActionKind::Wrap { .. } => (
            Info,
            "ran the test command through `belay run`.".to_string(),
            "wrapped runs",
        ),
        ActionKind::Retry { .. } => (
            Info,
            "told the agent it may retry a call auto mode denied.".to_string(),
            "retries",
        ),
    };
    (
        Notice {
            level,
            text,
            action: Some(id.to_string()),
        },
        noun,
    )
}

/// The most important notice among candidates, with the rest counted. Ties go to the
/// first one. None if there are none.
fn collapse(candidates: &[(Notice, &'static str, u64)]) -> Option<(Notice, u64)> {
    let best = candidates
        .iter()
        .enumerate()
        .max_by_key(|(i, (n, _, _))| (n.level.rank(), std::cmp::Reverse(*i)))
        .map(|(i, _)| i)?;
    let (notice, _, seq) = &candidates[best];
    let mut notice = notice.clone();
    let rest: Vec<&str> = candidates
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != best)
        .map(|(_, (_, noun, _))| *noun)
        .collect();
    if !rest.is_empty() {
        let noun = if rest.iter().all(|n| *n == rest[0]) {
            rest[0]
        } else {
            "actions"
        };
        notice.text = format!("{} (+{} more {noun})", notice.text, rest.len());
    }
    Some((notice, *seq))
}

impl Controller {
    /// Make this hook's notice from the actions `d` applied, record it, and give it to
    /// the connector if the report setting and the agent let the user see it.
    pub(super) fn add_notice(&mut self, d: &mut Decision, now: u64, to_user: bool) {
        let verbose = self.cfg.report == ReportLevel::Verbose;
        let candidates: Vec<(Notice, &'static str, u64)> = d
            .entries
            .iter()
            .filter_map(|e| match &e.body {
                Body::Action {
                    id,
                    action,
                    reason,
                    applied: true,
                    ..
                } => {
                    let (n, noun) = notice_for(id, action, reason, verbose);
                    Some((n, noun, e.seq))
                }
                _ => None,
            })
            .collect();
        let report = self.cfg.report;
        let visible: Vec<_> = candidates
            .iter()
            .filter(|(n, _, _)| report.shows(n.level))
            .cloned()
            .collect();
        let shown = to_user && self.caps.notify_user && !visible.is_empty();
        let Some((mut notice, seq)) = collapse(if visible.is_empty() {
            &candidates
        } else {
            &visible
        }) else {
            return;
        };
        notice.add_flag_hint();
        self.push(
            d,
            now,
            vec![seq],
            Body::Notice {
                notice: notice.clone(),
                shown,
            },
        );
        if shown {
            d.response.notices.push(notice);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(
        level: NoticeLevel,
        text: &str,
        noun: &'static str,
        seq: u64,
    ) -> (Notice, &'static str, u64) {
        (
            Notice {
                level,
                text: text.into(),
                action: None,
            },
            noun,
            seq,
        )
    }

    #[test]
    fn collapse_keeps_the_most_important_and_counts_the_rest() {
        let c = [
            n(NoticeLevel::Act, "summarized a", "summaries", 1),
            n(NoticeLevel::Warn, "blocked", "blocked edits", 2),
            n(NoticeLevel::Act, "summarized b", "summaries", 3),
            n(NoticeLevel::Act, "summarized c", "summaries", 4),
        ];
        let (notice, seq) = collapse(&c).unwrap();
        assert_eq!(seq, 2);
        assert_eq!(notice.text, "blocked (+3 more summaries)");
        let (notice, seq) = collapse(&c[..1]).unwrap();
        assert_eq!((notice.text.as_str(), seq), ("summarized a", 1));
        assert!(collapse(&[]).is_none());
    }

    #[test]
    fn memory_recalls_are_told_as_acts() {
        let start = ActionKind::MemoryRecalled {
            ids: vec!["m-1".into(), "m-2".into()],
            compact: false,
            text: String::new(),
        };
        let (n, noun) = notice_for("a-1", &start, "", false);
        assert_eq!(n.level, NoticeLevel::Act);
        assert_eq!(n.text, "gave the agent 2 memories from earlier sessions.");
        assert_eq!(noun, "recalls");
        let carry = ActionKind::MemoryRecalled {
            ids: vec!["m-1".into()],
            compact: true,
            text: String::new(),
        };
        let (n, _) = notice_for("a-2", &carry, "", false);
        assert_eq!(n.text, "gave the agent back its task and 1 memory.");
    }

    #[test]
    fn warn_notices_end_with_the_flag_hint_once() {
        let guard = ActionKind::Guard {
            path: "t.py".into(),
            decision: GuardDecision::Deny,
        };
        let (mut w, _) = notice_for("a-4", &guard, "", false);
        w.text.push_str(" (+1 more summaries)");
        w.add_flag_hint();
        assert!(
            w.text
                .ends_with("(+1 more summaries) Wrong? `belay flag a-4`"),
            "{}",
            w.text
        );
        let once = w.text.clone();
        w.add_flag_hint();
        assert_eq!(w.text, once);
        let shape = ActionKind::Shape {
            output_id: "o-1".into(),
            from_tokens: 10,
            to_tokens: 5,
            from_lines: 10,
            to_lines: 5,
        };
        let (mut a, _) = notice_for("a-5", &shape, "", false);
        let before = a.text.clone();
        a.add_flag_hint();
        assert_eq!(a.text, before, "only warnings get the hint");
    }

    #[test]
    fn report_levels_filter_notices() {
        use NoticeLevel::*;
        assert!(ReportLevel::Quiet.shows(Warn) && !ReportLevel::Quiet.shows(Act));
        assert!(ReportLevel::Normal.shows(Act) && !ReportLevel::Normal.shows(Info));
        assert!(ReportLevel::Verbose.shows(Info));
    }
}
