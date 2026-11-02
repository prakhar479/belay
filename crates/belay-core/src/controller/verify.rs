//! Verify before done (U6.1). At the end of a turn, if source files changed after the
//! last passing test or check run, belay keeps the agent going once, with the command to
//! run. Once per turn, and never again for the same unchanged state, so belay cannot
//! loop with itself. A fleet session is left alone: its hub runs the task's check.

use super::{Controller, Decision, Feature};
use crate::record::ActionKind;

/// Files whose change does not call for a test run.
const NOT_SOURCE: [&str; 6] = [".md", ".markdown", ".rst", ".txt", ".adoc", ".org"];

/// Whether a change to `path` calls for a test run (docs do not).
pub(crate) fn is_source(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    !NOT_SOURCE.iter().any(|ext| lower.ends_with(ext))
}

/// Whether a test or check run passed: failures in its output or a failed exit say no;
/// a clean exit or the runner's own pass summary say yes; else it is not known.
/// `exit_failed`: None when the exit code is not known.
pub(crate) fn run_passed(
    has_failures: bool,
    exit_failed: Option<bool>,
    summary_pass: bool,
) -> Option<bool> {
    if has_failures || exit_failed == Some(true) {
        Some(false)
    } else if exit_failed == Some(false) || summary_pass {
        Some(true)
    } else {
        None
    }
}

/// What verify before done tracks in a session. The session brief keeps one per
/// command, so "passed after the last edit" means the same in both.
#[derive(Debug, Clone, Default)]
pub(crate) struct VerifyState {
    /// Source-file changes seen: a clock that only edits move.
    edits: u64,
    /// The edit clock at the last passing test or check run.
    passed_at: Option<u64>,
    /// The last test or check command the session ran, and the seq of its output.
    command: Option<(String, u64)>,
    /// The edit clock when belay last kept the agent going.
    nudged_at: Option<u64>,
    /// belay kept the agent going in this turn already.
    this_turn: bool,
}

impl VerifyState {
    /// A file changed (an edit tool, or a shell command).
    pub(crate) fn on_edit(&mut self, path: &str) {
        if is_source(path) {
            self.edits += 1;
        }
    }

    /// A test or check run of `command` (its output entry `out`). `passed`: None when
    /// the run's result is not known, which proves nothing.
    pub(crate) fn on_run(&mut self, command: &str, passed: Option<bool>, out: u64) {
        self.command = Some((command.to_string(), out));
        if passed == Some(true) {
            self.passed_at = Some(self.edits);
        }
    }

    /// The user wrote again: a new turn.
    pub(super) fn on_prompt(&mut self) {
        self.this_turn = false;
    }

    /// Source files changed since the last passing run (or with no pass yet).
    pub(crate) fn changed_since_pass(&self) -> bool {
        self.edits > 0 && self.passed_at.is_none_or(|p| self.edits > p)
    }

    /// The command to ask for, and the seq of its last output, if the turn should go on.
    fn wanted(&self) -> Option<(String, u64)> {
        let changed = self.changed_since_pass();
        let again = self.this_turn || self.nudged_at == Some(self.edits);
        if !changed || again {
            return None;
        }
        self.command.clone()
    }

    /// For the state digest.
    pub(super) fn digest(&self) -> String {
        format!("{self:?}")
    }
}

impl Controller {
    /// At the end of a turn: keep the agent going if files changed since the last
    /// passing run of its tests. `ev` is the turn-end event.
    pub(super) fn verify_at_end(&mut self, d: &mut Decision, now: u64, ev: u64) {
        if !self.on(Feature::Verify) || self.in_fleet {
            return;
        }
        let Some((command, out)) = self.verify.wanted() else {
            return;
        };
        self.verify.this_turn = true;
        self.verify.nudged_at = Some(self.verify.edits);
        let text = format!(
            "belay: files changed since the last passing run of `{command}`. Run it before you finish."
        );
        let reason = match self.verify.passed_at {
            Some(_) => format!("Files changed after `{command}` last passed, and the turn ended."),
            None => {
                format!("Files changed and `{command}` has not passed since, and the turn ended.")
            }
        };
        let action = ActionKind::Verify {
            command,
            text: text.clone(),
        };
        let supported = self.supports(&action);
        let applied = self.deliver && supported;
        self.record(d, now, vec![ev, out], action, reason, applied, !supported);
        if applied {
            d.response.continue_turn = Some(text);
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::record::{ActionKind, Body};
    use crate::{Capabilities, Config, Controller, Event, Feature, ToolKind};

    fn ctl() -> Controller {
        Controller::new(Config::default(), Capabilities::CLAUDE_CODE)
    }

    fn run(c: &mut Controller, cmd: &str, output: &str, failed: bool, now: u64) {
        c.handle(
            Event::ToolCompleted {
                tool_use_id: format!("r{now}"),
                tool: "Bash".into(),
                kind: ToolKind::Shell,
                input: json!({ "command": cmd }),
                output: output.into(),
                failed,
            },
            now,
        );
    }

    fn pass(c: &mut Controller, now: u64) {
        run(c, "pytest", "===== 3 passed in 0.10s =====", false, now);
    }

    fn fail(c: &mut Controller, now: u64) {
        let out = "FAILED tests/test_a.py::test_x - assert 1 == 2\n===== 1 failed in 0.10s =====";
        run(c, "pytest", out, true, now);
    }

    fn edit(c: &mut Controller, path: &str, now: u64) {
        c.handle(
            Event::FileEdited {
                tool_use_id: format!("e{now}"),
                path: path.into(),
                after_hash: Some(format!("h{now}")),
            },
            now,
        );
    }

    fn stop(c: &mut Controller, now: u64) -> Option<String> {
        let d = c.handle(Event::TurnEnded, now);
        let action = d.entries.iter().find_map(|e| match &e.body {
            Body::Action {
                action: ActionKind::Verify { .. },
                reason,
                ..
            } => Some((reason.clone(), e.causes.clone())),
            _ => None,
        });
        assert_eq!(action.is_some(), d.response.continue_turn.is_some());
        if let Some((reason, causes)) = action {
            assert!(
                reason.ends_with('.') && causes.len() == 2,
                "{reason} {causes:?}"
            );
            let notice = d.entries.iter().any(|e| {
                matches!(&e.body, Body::Notice { notice, .. } if notice.level == crate::NoticeLevel::Act)
            });
            assert!(notice, "an Act notice goes with it");
        }
        d.response.continue_turn
    }

    fn prompt(c: &mut Controller, now: u64) {
        c.handle(Event::PromptSubmitted { chars: 5 }, now);
    }

    #[test]
    fn an_edit_after_the_last_pass_keeps_the_turn_going_once() {
        let mut c = ctl();
        pass(&mut c, 1);
        edit(&mut c, "src/app.py", 2);
        let why = stop(&mut c, 3).expect("continues");
        assert_eq!(
            why,
            "belay: files changed since the last passing run of `pytest`. Run it before you finish."
        );
        // Never twice in a row: the next stop passes though nothing changed.
        assert_eq!(stop(&mut c, 4), None);
        // A new turn with the same unchanged state: still nothing.
        prompt(&mut c, 5);
        assert_eq!(stop(&mut c, 6), None);
        // A new edit in a new turn: once more.
        prompt(&mut c, 7);
        edit(&mut c, "src/app.py", 8);
        assert!(stop(&mut c, 9).is_some());
        // The agent ran it and it passed: the stop passes.
        pass(&mut c, 10);
        prompt(&mut c, 11);
        assert_eq!(stop(&mut c, 12), None);
    }

    #[test]
    fn at_most_once_per_turn_even_after_more_edits() {
        let mut c = ctl();
        fail(&mut c, 1);
        edit(&mut c, "src/app.py", 2);
        assert!(stop(&mut c, 3).is_some());
        edit(&mut c, "src/app.py", 4);
        assert_eq!(stop(&mut c, 5), None);
    }

    #[test]
    fn nothing_without_a_known_command_or_an_edit() {
        let mut c = ctl();
        edit(&mut c, "src/app.py", 1);
        assert_eq!(stop(&mut c, 2), None, "no test command known");
        let mut c = ctl();
        pass(&mut c, 1);
        assert_eq!(stop(&mut c, 2), None, "no edit since the pass");
        edit(&mut c, "README.md", 3);
        assert_eq!(stop(&mut c, 4), None, "docs are not source");
        // A failing run after the edit is not a pass: the agent is asked.
        let mut c = ctl();
        pass(&mut c, 1);
        edit(&mut c, "src/app.py", 2);
        fail(&mut c, 3);
        assert!(stop(&mut c, 4).is_some());
    }

    #[test]
    fn off_paused_fleet_and_unsupported_take_no_turn() {
        let setup = |c: &mut Controller| {
            pass(c, 1);
            edit(c, "src/app.py", 2);
        };
        let mut off = Controller::new(
            Config {
                verify: false,
                ..Config::default()
            },
            Capabilities::CLAUDE_CODE,
        );
        setup(&mut off);
        assert_eq!(stop(&mut off, 3), None);
        let mut paused = ctl();
        paused.pause(Some(Feature::Verify), "user", 0).unwrap();
        setup(&mut paused);
        assert_eq!(stop(&mut paused, 3), None);
        let mut fleet = ctl();
        fleet.set_in_fleet(true);
        setup(&mut fleet);
        assert_eq!(stop(&mut fleet, 3), None);
        // An agent that cannot continue a turn: recorded, not applied.
        let mut caps = Capabilities::CLAUDE_CODE;
        caps.continue_turn = false;
        let mut c = Controller::new(Config::default(), caps);
        setup(&mut c);
        let d = c.handle(Event::TurnEnded, 3);
        assert!(d.response.continue_turn.is_none());
        assert!(d.entries.iter().any(|e| matches!(
            &e.body,
            Body::Action {
                action: ActionKind::Verify { .. },
                applied: false,
                unsupported: true,
                ..
            }
        )));
    }
}
