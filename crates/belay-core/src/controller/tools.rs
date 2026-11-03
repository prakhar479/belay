//! Tool requests: the guard, wrapping, denials, and edits.

use super::belay_cmd::*;
use super::*;

impl Controller {
    /// Only the user may lift a guard. Block the agent's own `belay allow`.
    /// The match is on the command text, so a determined agent can get around it;
    /// the guard is advisory and every allow is in the ledger.
    pub(super) fn on_tool_requested(&mut self, d: &mut Decision, event: Event, now: u64) {
        let (command, tool_use_id, rewrite_ok) = match &event {
            Event::ToolRequested {
                kind: ToolKind::Shell,
                input,
                tool_use_id,
                rewrite_ok,
                ..
            } => (
                input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                tool_use_id.clone(),
                *rewrite_ok,
            ),
            _ => (String::new(), String::new(), false),
        };
        let ev = self.push(d, now, vec![], Body::Event { event });
        // Rewriting every expected snapshot makes a failing suite pass by definition: say
        // so, so the agent (and the ledger) checks what changed (E5). It never blocks.
        let guard_on = self.on(Feature::Guard);
        if let Some(form) = crate::perception::snapshot_update(&command).filter(|_| guard_on) {
            let message = format!(
                "belay: `{form}` rewrites expected snapshots or golden files. Check that only the intended ones change."
            );
            let action = ActionKind::Guard {
                path: "snapshots".into(),
                decision: GuardDecision::Warn,
            };
            if self.act(d, now, ev, action, message.clone()) {
                d.response.add_context.push(message);
            }
        }
        let bypass = if !guard_on {
            None
        } else if self.in_fleet {
            crate::fleet::fleet_bypass(&command)
        } else {
            None
        };
        // A shell command that writes where the fleet role may not (G4).
        let bypass = bypass.or_else(|| {
            let rules = self.fleet_rules.as_ref().filter(|_| guard_on)?;
            if !writes_files(&command) {
                return None;
            }
            crate::perception::write_targets_where(&command, |_| true)
                .iter()
                .find_map(|p| {
                    rules
                        .refuse_shell_write(p)
                        .map(|why| format!("it writes {p}, and {why}"))
                })
        });
        if let Some(why) = bypass {
            let message = format!(
                "belay: in a fleet, this session may not run that: {why}. Use `belay task` for your task, and ask your lead or the user for the rest."
            );
            let action = ActionKind::BlockCommand { command };
            if self.act(d, now, ev, action, message.clone()) {
                d.response.permission = Some(Permission::Deny(message));
            }
            return;
        }
        let message = if runs_belay_allow(&command) {
            "belay: only the user can allow a blocked edit. Ask the user to run `! belay allow`, then retry the edit.".to_string()
        } else if runs_belay(&command, "undo") {
            "belay: only the user rolls files back. Ask the user to run `belay undo` in their own terminal.".to_string()
        } else if runs_belay_sandbox_rm(&command) {
            "belay: only the user removes a sandbox. Ask the user to run `belay sandbox rm` in their own terminal.".to_string()
        } else if runs_belay_bg_user(&command) {
            "belay: only the user stops or takes over a background session. Ask the user to run `belay bg stop` or `belay bg attach` in their own terminal.".to_string()
        } else if runs_belay_fleet_prune(&command) {
            "belay: only the user prunes fleet worktrees and branches. Ask the user to run `belay fleet prune --yes` in their own terminal.".to_string()
        } else if runs_belay(&command, "pause") || runs_belay(&command, "resume") {
            // Pausing would lift every guard at once.
            "belay: only the user pauses or resumes belay. Ask the user if belay should pause."
                .to_string()
        } else if runs_belay_notes_review(&command) {
            "belay: only the user reviews project notes and learned fixes. Your `belay note` waits for the user's review.".to_string()
        } else {
            self.maybe_wrap(d, now, ev, tool_use_id, command, rewrite_ok);
            return;
        };
        let action = ActionKind::BlockCommand { command };
        if self.act(d, now, ev, action, message.clone()) {
            d.response.permission = Some(Permission::Deny(message));
        }
    }

    /// Failed runs cannot be shaped after the fact (S2). Run test commands through
    /// `belay run`, which asks for the summary before the agent sees the output.
    pub(super) fn maybe_wrap(
        &mut self,
        d: &mut Decision,
        now: u64,
        cause: u64,
        tool_use_id: String,
        command: String,
        rewrite_ok: bool,
    ) {
        let info = classify(&command);
        if !self.on(Feature::WrapTests)
            || info.kind != OutputKind::Test
            || info.show_target.is_some()
            || tool_use_id.is_empty()
        {
            return;
        }
        // `belay run` is a subprocess. Claude Code keeps only the working directory between
        // Bash calls (environment, aliases, and options do not persist), so only cd/pushd/
        // popd would be lost; `source venv/bin/activate && …` is safe to wrap (R1.1).
        // `bash -c` runs here-docs, subshells, and multi-line scripts as the agent's shell
        // would. The parser splits every line and `{ …; }` group, so a cd anywhere is
        // found (R0.5); one inside `( … )` or a here-doc body also refuses (conservative).
        // A leading `cd DIR &&` stays outside the wrapper (see shell::leading_cd).
        let (prefix, _) = shell::leading_cd(&command);
        let line = shell::parse(&command[prefix..]);
        let changes_state = line.segments.iter().any(|seg| {
            matches!(
                seg.first().map(String::as_str),
                Some("cd" | "pushd" | "popd")
            )
        });
        if changes_state {
            return;
        }
        let reason = if self.rewrite_ignored {
            Some("Not wrapped: Claude Code ignored belay's command rewrite earlier in this session (check it with scripts/conformance.py).")
        } else if self.no_wrap.contains(&command_key(&command)) {
            Some("Not wrapped: a wrapped run of this command got no result before (denied or stopped).")
        } else {
            None
        };
        if let Some(reason) = reason {
            let action = ActionKind::Wrap {
                tool_use_id: tool_use_id.clone(),
            };
            self.record(d, now, vec![cause], action, reason.into(), false, false);
            return;
        }
        let reason = if rewrite_ok {
            "Test command wrapped in `belay run`, so its output can be summarized even when it fails.".to_string()
        } else {
            // The connector's `rewrite_ok` covers all three (live: a `for` loop around
            // cargo test was blamed on the permission mode).
            "Not wrapped: a rewrite was not safe here. The permission mode can prompt, the call runs in the background, or belay could not find the runner."
                .to_string()
        };
        let action = ActionKind::Wrap {
            tool_use_id: tool_use_id.clone(),
        };
        let supported = self.caps.rewrite_input && rewrite_ok;
        let (seq, action_id) = (self.next_seq, format!("a-{}", self.next_action));
        if self.act_if(d, now, cause, action, reason, supported) {
            d.response.wrap = true;
            self.wrapped.insert(
                tool_use_id,
                Wrapped {
                    command,
                    action_id,
                    action_seq: seq,
                    filtered: false,
                    pending: None,
                    context: Vec::new(),
                    context_actions: Vec::new(),
                },
            );
        }
    }

    /// Auto mode denied a call. If belay had wrapped it, the wrapper may be why: its
    /// classifier sometimes calls `belay run` a bypass (R3.7). Stop wrapping the command,
    /// and tell the agent it may retry; the retry runs unwrapped.
    pub(super) fn on_tool_denied(&mut self, d: &mut Decision, event: Event, now: u64) {
        let Event::ToolDenied { tool_use_id, .. } = &event else {
            return;
        };
        let tool_use_id = tool_use_id.clone();
        let ev = self.push(d, now, vec![], Body::Event { event });
        let Some(w) = self.wrapped.remove(&tool_use_id) else {
            return;
        };
        self.no_wrap.insert(command_key(&w.command));
        let reason = "Auto mode denied a call that belay had wrapped in `belay run`; the agent may retry it unwrapped.";
        let action = ActionKind::Retry { tool_use_id };
        let supported = self.supports(&action);
        let applied = self.deliver && supported;
        self.record(
            d,
            now,
            vec![ev, w.action_seq],
            action,
            reason.into(),
            applied,
            !supported,
        );
        d.response.retry = applied;
    }

    pub(super) fn on_file_edited(&mut self, d: &mut Decision, event: Event, now: u64) {
        let (path, hash) = match &event {
            Event::FileEdited {
                path, after_hash, ..
            } => (path.clone(), after_hash.clone()),
            _ => return,
        };
        let ev = self.push(d, now, vec![], Body::Event { event });
        let Some(hash) = hash else { return };
        if let Some(LoopSignal::Cycle { path }) = self.loops.on_edit(&path, &hash) {
            let text = format!("belay: {path} is back to an earlier version (edit, revert, edit).");
            let reason = format!("Content hash of {path} repeated an earlier state.");
            self.steer(d, now, ev, SteerTrigger::Cycle { path }, text, reason);
        }
    }
}
