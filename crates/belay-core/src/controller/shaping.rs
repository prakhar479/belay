//! Perceiving outputs: store, loop check, summary, and the end of a turn.

use super::*;

/// The costly, pure part of handling an output: hash, failures, and summary.
#[derive(Debug, Clone)]
pub struct Prepared {
    hash: String,
    lines: usize,
    tokens: usize,
    failures: Vec<Failure>,
    /// The summary at `level`, if the output is large enough and the summary pays off.
    shaped: Option<Shaped>,
    level: u8,
}

/// Prepare a test run's output for `observe`. Pure, so the daemon can run it without its
/// lock.
pub fn prepare(output: &str, level: u8, cfg: &Config) -> Prepared {
    prepare_for(output, None, level, cfg, true)
}

/// `prepare` with a hash the caller already has, and failures read only where loops
/// use them (test and build runs): a 16 MB file view is not parsed for nothing.
pub(super) fn prepare_for(
    output: &str,
    hash: Option<String>,
    level: u8,
    cfg: &Config,
    failures: bool,
) -> Prepared {
    let tokens = estimate_tokens(output);
    let shaped = if tokens >= cfg.shape_threshold_tokens {
        shape(output, level, cfg.max_summary_ratio)
    } else {
        None
    };
    Prepared {
        hash: hash.unwrap_or_else(|| sha256_hex(output)),
        lines: output.lines().count(),
        tokens,
        failures: if failures {
            extract_failures(output)
        } else {
            Vec::new()
        },
        shaped,
        level,
    }
}

impl Controller {
    /// Store an output, look for loops, and shape it. Returns the summary to show, if
    /// the shape action applies. `can_replace`: the caller can put the summary in place.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn observe(
        &mut self,
        d: &mut Decision,
        now: u64,
        cause: u64,
        command: &str,
        kind: OutputKind,
        output: String,
        p: Prepared,
        can_replace: Replace,
    ) -> Option<String> {
        // A Bash write before the run in the same call (`python3 - <<EOF … EOF; pytest`)
        // is an edit for loop detection: agents edit mostly through Bash (R1.3).
        if writes_files(command) {
            self.loops.note_edit();
        }
        // Learning (F2) needs the run as written: a pipe or `-x` hides what it shows.
        let complete = crate::learn::complete_run(command);
        let summary_pass =
            crate::learn::summary_run(command) && crate::perception::signature::shows_pass(&output);
        // Loops and re-runs compare normalized keys: agents vary pipes and flags (R1.3).
        let key = command_key(command);
        let command = key.as_str();
        // Re-running the same command right after a summary, with no edit in between,
        // means the summary hid something the agent wanted: treat it as a re-open.
        if let Some(last) = self.last_summary.take() {
            if last.command == command && self.loops.edits() == last.edits {
                let what = format!("re-ran the command behind {} with no edit", last.output_id);
                if let Some(change) = self.session.on_reopen(last.kind, &what) {
                    self.adapt(d, now, cause, change);
                }
            }
        }
        let output_id = format!("o-{}", self.next_output);
        self.next_output += 1;
        self.outputs.insert(output_id.clone(), kind);
        d.blobs.push(Blob {
            hash: p.hash.clone(),
            content: output,
        });
        let out = self.push(
            d,
            now,
            vec![cause],
            Body::Output {
                id: output_id.clone(),
                blob: p.hash,
                output_kind: kind,
                lines: p.lines,
                tokens: p.tokens,
            },
        );

        // Loops. All failures that reach the threshold in one run share one steer. Only
        // test and build runs count: a file view or log dump that contains failure text
        // is not a run that failed (checkpoint C2).
        let failures = if matches!(kind, OutputKind::Test | OutputKind::Build) {
            p.failures
        } else {
            Vec::new()
        };
        // Values the failures show, by run, for the expected-to-actual rule (E4).
        if matches!(kind, OutputKind::Test | OutputKind::Build) {
            let pairs: Vec<(String, String)> = failures
                .iter()
                .flat_map(|f| crate::guard::value_pairs(&f.message))
                .collect();
            if pairs.is_empty() {
                self.failure_values.remove(command);
            } else if self.failure_values.len() < 64 || self.failure_values.contains_key(command) {
                self.failure_values.insert(command.to_string(), pairs);
            }
        }
        // The card (C1): how the last run went.
        if matches!(kind, OutputKind::Test | OutputKind::Build) {
            let passed = verify::run_passed(!failures.is_empty(), self.run_failed, summary_pass);
            match passed {
                Some(true) => {
                    self.facts.last_pass_ms = Some(now);
                    self.facts.last_run_failed = false;
                }
                Some(false) => self.facts.last_run_failed = true,
                None => {}
            }
            self.verify.on_run(&key, passed, out);
            self.last_test = Some(crate::commons::LastTest {
                command: key.clone(),
                passed,
                failures: failures.len(),
                at_ms: now,
            });
            self.memory_on_run(&failures, passed);
        }
        // Shared learning (F2): fixes learned here, and fixes others learned.
        if matches!(kind, OutputKind::Test | OutputKind::Build) && self.on(Feature::Memory) {
            let passed = self.run_failed == Some(false) && complete || summary_pass;
            for f in self.learning.on_run(&key, &failures, passed) {
                self.push(
                    d,
                    now,
                    vec![out],
                    Body::Learned {
                        signature: f.failure.signature,
                        message: f.failure.message,
                        test: f.failure.test,
                        command: f.command,
                        files: f.files,
                        change: f.change,
                    },
                );
            }
            if let Some((failure, fix)) = self.learning.recall(&failures) {
                self.memory_used_fix(&fix);
                let text = self.learning.recall_text(&fix, now);
                let applied = self.says();
                let reason = format!(
                    "Session {} fixed `{}` before.",
                    fix.session,
                    crate::text::truncate_chars(&failure.summary(), 80)
                );
                let action = ActionKind::Recall {
                    signature: failure.signature,
                    session: fix.session,
                    text: text.clone(),
                };
                self.record(
                    d,
                    now,
                    vec![out],
                    action,
                    reason,
                    applied,
                    !self.caps.inject_context,
                );
                if applied {
                    d.response.add_context.push(text);
                }
            } else {
                self.recall_fix_memory(d, out, &failures, now);
            }
        }
        let mut looped: Vec<(String, String, u64, u64)> = Vec::new();
        let active = self.active_ms;
        // The rules answer "same failure?" from the signature alone, so a judgment is
        // worth recording only when a decider can answer it differently (Phase 4).
        let judge = self.broker.has_decider_for("same_failure");
        for obs in self.loops.on_run(command, &failures, active) {
            if let Some(prev) = obs.previous_message.as_deref().filter(|_| judge) {
                let q = Question::SameFailure {
                    test: obs.failure.test.as_deref(),
                    a: prev,
                    b: &obs.failure.message,
                };
                let (answer, source) = self.broker.ask(&q);
                let body = Body::Judgment {
                    question: q.name().into(),
                    version: q.version(),
                    subject: obs.failure.signature.clone(),
                    answer,
                    source,
                };
                self.push(d, now, vec![out], body);
            }
            if let Some(LoopSignal::Loop {
                signature,
                summary,
                edits,
                first_ms,
            }) = obs.signal
            {
                looped.push((signature, summary, edits, first_ms));
            }
        }
        if !looped.is_empty() {
            let edits = looped.iter().map(|l| l.2).min().unwrap_or(0);
            let summaries: Vec<&str> = looped.iter().map(|l| l.1.as_str()).collect();
            let signatures: Vec<String> = looped.iter().map(|l| l.0.clone()).collect();
            // Both ends on the active clock (idle gaps capped).
            let first = looped.iter().map(|l| l.3).min().unwrap_or(active);
            let duration_ms = Some(active.saturating_sub(first));
            let text = if looped.len() == 1 {
                format!(
                    "belay: this failure is unchanged after {edits} fix attempts (edit, then run): {}. A different approach may be needed.",
                    summaries[0]
                )
            } else {
                format!(
                    "belay: {} failures are unchanged after {edits} fix attempts (edit, then run): {}. A different approach may be needed.",
                    looped.len(),
                    summaries.join("; ")
                )
            };
            let reason = format!(
                "Failure signature(s) {} recurred after {edits} fix attempts.",
                signatures.join(", ")
            );
            let trigger = SteerTrigger::Loop {
                signatures,
                duration_ms,
            };
            self.steer(d, now, out, trigger, text, reason);
        }

        // Shaping (computed by `prepare`, possibly outside the daemon's lock).
        if !self.on(Feature::Shape) {
            return None;
        }
        let level = p.level;
        let s = p.shaped?;
        let text = format!(
            "{}\n[belay: {} lines → {} · full output: belay show {}]",
            s.text, s.from_lines, s.to_lines, output_id
        );
        let reason = format!(
            "{} output was {} tokens; the summary keeps every failure line (level {level}).",
            kind.label(),
            s.from_tokens
        );
        let action = ActionKind::Shape {
            output_id: output_id.clone(),
            from_tokens: s.from_tokens,
            to_tokens: estimate_tokens(&text),
            from_lines: s.from_lines,
            to_lines: s.to_lines,
        };
        let supported = match can_replace {
            Replace::Now(s) => s,
            Replace::Deferred => {
                self.deferred = Some(PendingShape {
                    action,
                    reason,
                    text: text.clone(),
                    kind,
                    out,
                    command: command.to_string(),
                    output_id,
                });
                return Some(text);
            }
        };
        if !self.act_if(d, now, out, action, reason, supported) {
            return None;
        }
        self.shown(d, now, out, command, output_id, kind);
        Some(text)
    }

    /// A summary reached the agent: adapt, and remember it for the re-run signal.
    pub(super) fn shown(
        &mut self,
        d: &mut Decision,
        now: u64,
        out: u64,
        command: &str,
        output_id: String,
        kind: OutputKind,
    ) {
        if let Some(change) = self.session.on_shaped(kind) {
            self.adapt(d, now, out, change);
        }
        self.last_summary = Some(LastSummary {
            command: command.to_string(),
            output_id,
            kind,
            edits: self.loops.edits(),
        });
    }

    /// Record a deferred shape against the tool result the agent saw. If `belay run`
    /// printed the original (the daemon answered too late), the summary was not applied.
    pub(super) fn confirm_shape(
        &mut self,
        d: &mut Decision,
        now: u64,
        ev: u64,
        p: PendingShape,
        seen: &str,
    ) {
        if seen.trim_end().ends_with(p.text.trim_end()) {
            self.record(d, now, vec![p.out, ev], p.action, p.reason, true, false);
            self.shown(d, now, p.out, &p.command, p.output_id, p.kind);
        } else {
            let reason =
                "belay run printed the original output, so the agent did not see this summary.";
            self.record(
                d,
                now,
                vec![p.out, ev],
                p.action,
                reason.into(),
                false,
                false,
            );
        }
    }

    /// A turn ended. A wrapped call with no tool result was stopped: its summary was not
    /// seen. Drop every wrapped entry, so the map cannot grow (R0.7).
    pub(super) fn end_turn(&mut self, d: &mut Decision, now: u64, ev: u64) {
        for (_, w) in std::mem::take(&mut self.wrapped) {
            // No tool result: denied (auto mode may call a rewrite a bypass) or stopped.
            // One strike: run this command unwrapped from now on.
            if !w.filtered {
                self.no_wrap.insert(command_key(&w.command));
            }
            if !w.context.is_empty() {
                self.push(
                    d,
                    now,
                    vec![ev],
                    Body::Undelivered {
                        actions: w.context_actions,
                        reason: "No tool result arrived (the call was stopped), so the agent did not get this.".into(),
                    },
                );
            }
            if let Some(p) = w.pending {
                let reason = "No tool result arrived (the call was stopped), so the agent did not see this summary.";
                self.record(
                    d,
                    now,
                    vec![p.out, ev],
                    p.action,
                    reason.into(),
                    false,
                    false,
                );
            }
        }
    }

    /// `belay run` ran a wrapped command and asks for the text to print.
    /// Returns None when this call was not wrapped: print the original output.
    pub fn filter(
        &mut self,
        tool_use_id: &str,
        output: String,
        now_ms: u64,
    ) -> (Decision, Option<String>) {
        let Some(level) = self.filter_level(tool_use_id) else {
            return (Decision::default(), None);
        };
        let p = prepare(&output, level, &self.cfg);
        self.filter_prepared(tool_use_id, output, p, None, now_ms)
    }

    /// The shape level for a pending `belay run` report, or None if there is nothing to
    /// filter. With `prepare`, the daemon does the heavy work without holding its lock.
    pub fn filter_level(&self, tool_use_id: &str) -> Option<u8> {
        let w = self.wrapped.get(tool_use_id).filter(|w| !w.filtered)?;
        Some(self.session.shape_level(classify(&w.command).kind))
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// `filter` with the output already prepared.
    pub fn filter_prepared(
        &mut self,
        tool_use_id: &str,
        output: String,
        p: Prepared,
        exit_code: Option<i32>,
        now_ms: u64,
    ) -> (Decision, Option<String>) {
        self.tick(now_ms);
        let mut d = Decision::default();
        let Some(w) = self.wrapped.get_mut(tool_use_id) else {
            return (d, None);
        };
        if w.filtered {
            return (d, None);
        }
        w.filtered = true;
        let (command, cause) = (w.command.clone(), w.action_seq);
        let kind = classify(&command).kind;
        // `belay run` knows the exit code: a wrapped run can show a pass (F2).
        self.run_failed = exit_code.map(|c| c != 0);
        let text = self.observe(
            &mut d,
            now_ms,
            cause,
            &command,
            kind,
            output,
            p,
            Replace::Deferred,
        );
        self.run_failed = None;
        self.note_actions(&d, now_ms);
        // `belay run` prints no notice: the user hears of it in the ledger and watch.
        self.add_notice(&mut d, now_ms, false);
        if let Some(w) = self.wrapped.get_mut(tool_use_id) {
            w.pending = self.deferred.take();
            w.context = std::mem::take(&mut d.response.add_context);
            if !w.context.is_empty() {
                w.context_actions = d
                    .entries
                    .iter()
                    .filter_map(|e| match &e.body {
                        Body::Action {
                            id, applied: true, ..
                        } => Some(id.clone()),
                        _ => None,
                    })
                    .collect();
            }
        }
        (d, text)
    }
}
