//! Resume (U8): a resumed conversation continues the same belay story. The daemon reads
//! the earlier runs' ledgers; the controller replays them through its normal handlers,
//! with what they produce thrown away, so its state is as if those runs happened here.
//! Nothing reaches the agent and nothing is written while replaying (rules 3 and 5).

use std::collections::{BTreeMap, BTreeSet};

use super::{Controller, Decision};
use crate::event::Event;
use crate::record::{ActionKind, Body, Entry, LEDGER_FORMAT};
use crate::text::sha256_hex;

/// The number after `prefix` in an id like `a-12`.
fn id_number(id: &str, prefix: &str) -> Option<u64> {
    id.strip_prefix(prefix)?.parse().ok()
}

/// The text a `<blob:HASH>` placeholder stands for, when the store still has it.
fn unblob(text: &mut String, blobs: &dyn Fn(&str) -> Option<String>) {
    let Some(hash) = text
        .strip_prefix("<blob:")
        .and_then(|t| t.strip_suffix('>'))
    else {
        return;
    };
    if let Some(original) = blobs(hash) {
        *text = original;
    }
}

/// An event as the agent sent it: large texts back in place of their blob names.
fn resolved(event: &Event, blobs: &dyn Fn(&str) -> Option<String>) -> Event {
    let mut ev = event.clone();
    match &mut ev {
        Event::ToolCompleted { output, .. } => unblob(output, blobs),
        Event::EditProposed {
            old_text, new_text, ..
        }
        | Event::FileChanged {
            old_text, new_text, ..
        } => {
            unblob(old_text, blobs);
            unblob(new_text, blobs);
        }
        _ => {}
    }
    ev
}

/// The highest ids a ledger used: `(seq, a-N, o-N)`.
pub fn max_ids(ledger: &[Entry]) -> (u64, u64, u64) {
    let mut m = (0, 0, 0);
    for e in ledger {
        m.0 = m.0.max(e.seq);
        match &e.body {
            Body::Action { id, .. } => m.1 = m.1.max(id_number(id, "a-").unwrap_or(0)),
            Body::Output { id, .. } => m.2 = m.2.max(id_number(id, "o-").unwrap_or(0)),
            _ => {}
        }
    }
    m
}

impl Controller {
    /// Replay recorded runs (oldest first; a chain of resumed runs one after another) so
    /// the session model, loop detector, failure values, blocked and allowed edits, shape
    /// levels, paused features, memory, and wrapped runs are as if they happened here.
    /// `blobs` gives a blob's text by hash (outputs and edits are stored as `<blob:…>`).
    ///
    /// Not only events change state: the user's `allow` and `pause`, late replies (D9),
    /// carried-over shape levels, and `belay run` reports are replayed from their entries
    /// too. Ids are kept in step with the ledger, so `a-N` names the same edit after a
    /// restart (`belay allow a-N`), and continue after its highest ones.
    pub fn restore(&mut self, ledger: &[Entry], blobs: &dyn Fn(&str) -> Option<String>) {
        // Actions an `undelivered` entry names: they were sent, and the reply came late.
        let named: BTreeSet<&str> = ledger
            .iter()
            .filter_map(|e| match &e.body {
                Body::Undelivered { actions, .. } => Some(actions.iter().map(String::as_str)),
                _ => None,
            })
            .flatten()
            .collect();
        // The user's allows: the guard action an allowed edit gets is never applied.
        let overrides: BTreeSet<u64> = ledger
            .iter()
            .filter(|e| matches!(e.body, Body::Override { .. }))
            .map(|e| e.seq)
            .collect();
        // Wrap actions by seq: an output caused by one is a `belay run` report.
        let wraps: BTreeMap<u64, String> = ledger
            .iter()
            .filter_map(|e| match &e.body {
                Body::Action {
                    action: ActionKind::Wrap { tool_use_id },
                    ..
                } => Some((e.seq, tool_use_id.clone())),
                _ => None,
            })
            .collect();
        // Seqs this replay gave each action id, and the undelivered entries it made itself.
        let mut seq_of: BTreeMap<String, u64> = BTreeMap::new();
        let mut made_undelivered: Vec<Vec<String>> = Vec::new();
        let mut replayed = false;
        for (i, e) in ledger.iter().enumerate() {
            match &e.body {
                Body::Event { event } => {
                    let group: Vec<&Entry> = ledger[i + 1..]
                        .iter()
                        .take_while(|g| !matches!(g.body, Body::Event { .. }))
                        .collect();
                    self.sync_ids(e.seq, &group);
                    // A reply that came too late was handled with delivery off: its
                    // actions are recorded as not applied, though the agent supports them.
                    // Two are never applied anyway: a wrap belay chose not to make, and
                    // the guard's note on an edit the user allowed.
                    let late = group.iter().any(|g| match &g.body {
                        Body::Action {
                            id,
                            action,
                            applied: false,
                            unsupported: false,
                            ..
                        } => {
                            !named.contains(id.as_str())
                                && !matches!(action, ActionKind::Wrap { .. })
                                && !g.causes.iter().any(|c| overrides.contains(c))
                        }
                        _ => false,
                    });
                    let d = self.handle_with(resolved(event, blobs), e.ts_ms, !late);
                    self.note_replayed(&d, &mut seq_of, &mut made_undelivered);
                    replayed = true;
                }
                Body::Override { action } => {
                    self.next_seq = e.seq;
                    let _ = self.allow(Some(action), e.ts_ms);
                }
                Body::FeatureChanged { .. } => self.restore_paused(std::slice::from_ref(e)),
                Body::Adaptation { knob, to, .. } => {
                    // Levels carried over from earlier sessions come from outside the
                    // events; one the replay made itself is set again to the same value.
                    if let (Some(label), Ok(level)) =
                        (knob.strip_prefix("shape_level."), to.parse::<u8>())
                    {
                        if let Some(kind) = crate::perception::OutputKind::from_label(label) {
                            self.session
                                .set_level(kind, level.min(crate::session::MAX_LEVEL));
                        }
                    }
                }
                Body::Undelivered { actions, .. } => {
                    if let Some(at) = made_undelivered.iter().position(|a| a == actions) {
                        made_undelivered.remove(at);
                    } else {
                        let seqs = actions.iter().filter_map(|a| seq_of.get(a).copied());
                        let seqs: Vec<u64> = seqs.collect();
                        self.forget_undelivered(actions, &seqs);
                    }
                }
                Body::Output { blob, .. } => {
                    let Some(tool_use_id) = e.causes.first().and_then(|c| wraps.get(c)) else {
                        continue;
                    };
                    let Some(output) = blobs(blob) else { continue };
                    let group: Vec<&Entry> = ledger[i..]
                        .iter()
                        .take_while(|g| g.seq == e.seq || !matches!(g.body, Body::Event { .. }))
                        .collect();
                    self.sync_ids(e.seq, &group);
                    // [Verify] the exit code of a `belay run` is not in the ledger: the
                    // replayed report knows only what its output shows.
                    let (d, _) = self.filter(tool_use_id, output, e.ts_ms);
                    self.note_replayed(&d, &mut seq_of, &mut made_undelivered);
                }
                _ => {}
            }
        }
        let (seq, action, output) = max_ids(ledger);
        if !ledger.is_empty() {
            self.resume_ids(seq + 1, action + 1, output + 1);
        }
        self.restored |= replayed;
    }

    /// Start numbering where the ledger did for this entry and the ones it caused, so
    /// replayed ids match recorded ones even if the replay makes more or fewer entries.
    fn sync_ids(&mut self, seq: u64, group: &[&Entry]) {
        self.next_seq = seq;
        let first = |prefix: &str| {
            group
                .iter()
                .filter_map(|g| match &g.body {
                    Body::Action { id, .. } if prefix == "a-" => id_number(id, prefix),
                    Body::Output { id, .. } if prefix == "o-" => id_number(id, prefix),
                    _ => None,
                })
                .min()
        };
        if let Some(a) = first("a-") {
            self.next_action = a;
        }
        if let Some(o) = first("o-") {
            self.next_output = o;
        }
    }

    fn note_replayed(
        &self,
        d: &Decision,
        seq_of: &mut BTreeMap<String, u64>,
        made_undelivered: &mut Vec<Vec<String>>,
    ) {
        for e in &d.entries {
            match &e.body {
                Body::Action { id, .. } => {
                    seq_of.insert(id.clone(), e.seq);
                }
                Body::Undelivered { actions, .. } => made_undelivered.push(actions.clone()),
                _ => {}
            }
        }
    }

    /// Whether this controller continues earlier runs (`restore` replayed events).
    pub fn is_restored(&self) -> bool {
        self.restored
    }

    /// Record that this session continues `from_session`'s conversation, restored up to
    /// its entry `from_seq`. In a new ledger (`new_ledger`), the format entry comes first;
    /// its seq continues the earlier ledger's, so entry numbers stay unique in the chain.
    pub fn resumed(
        &mut self,
        from_session: &str,
        from_seq: u64,
        conversation: Option<String>,
        new_ledger: bool,
        now_ms: u64,
    ) -> Decision {
        let mut d = Decision::default();
        if new_ledger {
            let version = LEDGER_FORMAT;
            self.push(&mut d, now_ms, vec![], Body::Format { version });
        }
        let body = Body::Resumed {
            from_session: from_session.to_string(),
            from_seq,
            conversation,
        };
        self.push(&mut d, now_ms, vec![], body);
        d
    }

    /// `/resume` in the same process switched to a conversation an earlier session ran
    /// (U8). The switch already cleared the old conversation's state; take up the
    /// target's failures, loops, and guard history. Its blocked edits stay its own: their
    /// ids name another ledger's actions.
    pub fn restore_conversation(
        &mut self,
        ledger: &[Entry],
        blobs: &dyn Fn(&str) -> Option<String>,
        from_session: &str,
        conversation: Option<String>,
        now_ms: u64,
    ) -> Decision {
        let mut earlier = Controller::new(self.cfg.clone(), self.caps);
        earlier.set_policy(self.policy.clone());
        earlier.restore(ledger, blobs);
        self.loops = earlier.loops;
        self.failure_values = earlier.failure_values;
        self.mem.take_conversation(earlier.mem);
        if self.said.task.is_none() {
            self.said.task = earlier.said.task;
        }
        let from_seq = max_ids(ledger).0;
        self.resumed(from_session, from_seq, conversation, false, now_ms)
    }

    /// A hash of the state decisions depend on, serialized in a fixed order. Two
    /// controllers with equal digests decide the next event the same way.
    pub fn state_digest(&self) -> String {
        let mut s = String::new();
        let mut put = |name: &str, value: String| {
            s.push_str(name);
            s.push('=');
            s.push_str(&value);
            s.push('\n');
        };
        put(
            "ids",
            format!(
                "{} {} {}",
                self.next_seq, self.next_action, self.next_output
            ),
        );
        put("paused", format!("{:?}", self.paused));
        put("session", format!("{:?}", self.session));
        put("loops", format!("{:?}", self.loops));
        put(
            "blocked",
            format!("{:?} {:?}", self.blocked, self.last_blocked),
        );
        put("allowed", format!("{:?}", self.allowed));
        put("wrapped", format!("{:?}", self.wrapped));
        put("failure_values", format!("{:?}", self.failure_values));
        put("rewrite_ignored", format!("{:?}", self.rewrite_ignored));
        put("no_wrap", format!("{:?}", self.no_wrap));
        put("shell_baseline", format!("{:?}", self.shell_baseline));
        put("last_summary", format!("{:?}", self.last_summary));
        put("clock", format!("{:?} {}", self.last_ms, self.active_ms));
        put("memory", self.mem.digest());
        put("card", format!("{:?} {:?}", self.said, self.started));
        sha256_hex(&s)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use crate::{
        Capabilities, Config, Controller, Decision, Entry, Event, Feature, Permission, ToolKind,
    };

    const PYTEST: &str = include_str!("../../../../fixtures/outputs/pytest_fail.txt");

    /// What a daemon keeps: the ledger (through JSON, as on disk) and the blobs.
    #[derive(Default)]
    struct Store {
        ledger: Vec<Entry>,
        blobs: BTreeMap<String, String>,
    }

    impl Store {
        fn keep(&mut self, d: &Decision) {
            for b in &d.blobs {
                self.blobs.insert(b.hash.clone(), b.content.clone());
            }
            for e in &d.entries {
                let line = serde_json::to_string(e).unwrap();
                self.ledger.push(serde_json::from_str(&line).unwrap());
            }
        }

        fn restored(&self) -> Controller {
            let mut c = ctl();
            c.restore(&self.ledger, &|h| self.blobs.get(h).cloned());
            c
        }
    }

    fn ctl() -> Controller {
        Controller::new(Config::default(), Capabilities::CLAUDE_CODE)
    }

    fn start() -> Event {
        Event::SessionStarted {
            agent: "claude-code".into(),
            cwd: "/repo".into(),
            transcript_path: None,
            mode: None,
        }
    }

    fn pytest(id: &str) -> Event {
        Event::ToolCompleted {
            tool_use_id: id.into(),
            tool: "Bash".into(),
            kind: ToolKind::Shell,
            input: json!({ "command": "pytest" }),
            output: PYTEST.into(),
            failed: true,
        }
    }

    fn edited(hash: &str) -> Event {
        Event::FileEdited {
            tool_use_id: format!("e{hash}"),
            path: "src/app.py".into(),
            after_hash: Some(hash.into()),
        }
    }

    fn weakening() -> Event {
        Event::EditProposed {
            tool_use_id: "w".into(),
            path: "tests/test_api.py".into(),
            old_text: "assert a == 1\nassert b == 2\n".into(),
            new_text: "assert a == 1\n".into(),
        }
    }

    fn task(t: &str) -> Event {
        Event::CardSet {
            name: None,
            task: Some(t.into()),
            note: None,
            offers: None,
            topics: None,
            handoff: None,
        }
    }

    fn steered(d: &Decision) -> bool {
        d.response
            .add_context
            .iter()
            .any(|t| t.contains("unchanged after 3 fix attempts"))
    }

    /// U8: a run that failed twice, restored in a new daemon, steers on the third failure
    /// as the run itself would have; a blocked edit still waits for the user.
    #[test]
    fn a_restored_controller_decides_as_the_run_would_have() {
        let (mut run, mut store) = (ctl(), Store::default());
        let script = [
            start(),
            task("fix the parser"),
            pytest("p0"),
            edited("h1"),
            pytest("p1"),
            edited("h2"),
            pytest("p2"),
            weakening(),
            Event::TurnEnded,
        ];
        for (i, ev) in script.into_iter().enumerate() {
            store.keep(&run.handle(ev, i as u64 * 60_000));
        }
        store.keep(&run.pause(Some(Feature::Shape), "user", 600_000).unwrap());
        let mut back = store.restored();
        assert_eq!(back.state_digest(), run.state_digest());
        assert!(back.is_restored() && !run.is_restored());
        assert_eq!(back.card(0).said.task.as_deref(), Some("fix the parser"));

        // The guard block survives: the same edit is denied until the user allows it.
        let d = back.handle(weakening(), 700_000);
        assert!(matches!(d.response.permission, Some(Permission::Deny(_))));
        let d = run.handle(weakening(), 700_000);
        assert!(matches!(d.response.permission, Some(Permission::Deny(_))));
        let (a, b) = (run.allow(None, 701_000), back.allow(None, 701_000));
        assert_eq!(a.unwrap().entries, b.unwrap().entries);

        for c in [&mut run, &mut back] {
            c.handle(edited("h3"), 720_000);
        }
        let (a, b) = (
            run.handle(pytest("p3"), 780_000),
            back.handle(pytest("p3"), 780_000),
        );
        assert!(steered(&a), "{:?}", a.response);
        assert_eq!(a.entries, b.entries);
        assert_eq!(a.response.add_context, b.response.add_context);
        assert_eq!(back.state_digest(), run.state_digest());
    }

    /// The property: for any sequence of events, allows, and pauses, a controller that
    /// handled them and one that restored their ledger have equal digests.
    #[test]
    fn restore_digest_matches_for_random_runs() {
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for round in 0..10 {
            let (mut run, mut store) = (ctl(), Store::default());
            store.keep(&run.handle(start(), 0));
            let steps = 5 + next(15);
            for i in 1..=steps {
                let now = i * 45_000 + next(1_000_000) * u64::from(round % 3 == 0);
                let d = match next(9) {
                    0 | 1 => run.handle(pytest(&format!("p{i}")), now),
                    2 | 3 => run.handle(edited(&format!("h{}", next(4))), now),
                    4 => run.handle(weakening(), now),
                    5 => run.allow(None, now).unwrap_or_default(),
                    6 => run.handle(Event::TurnEnded, now),
                    7 => run
                        .pause(Some(Feature::Loops), "user", now)
                        .or_else(|_| run.resume(Some(Feature::Loops), "user", now))
                        .unwrap_or_default(),
                    _ => run.handle_with(pytest(&format!("late{i}")), now, false),
                };
                store.keep(&d);
                let back = store.restored();
                assert_eq!(
                    back.state_digest(),
                    run.state_digest(),
                    "round {round} step {i}"
                );
            }
        }
    }

    /// A resumed run's start gets the carry pack, not a fresh start's recall.
    #[test]
    fn a_resumed_start_gets_the_carry_pack() {
        let (mut run, mut store) = (ctl(), Store::default());
        store.keep(&run.handle(start(), 0));
        store.keep(&run.handle(task("fix the parser"), 1));
        store.keep(&run.handle(pytest("p0"), 2));
        let mut back = store.restored();
        let d = back.resumed("s-1", 9, Some("conv".into()), true, 3);
        assert!(matches!(d.entries[0].body, crate::Body::Format { .. }));
        assert!(d.entries[0].seq > store.ledger.last().unwrap().seq);
        assert!(
            matches!(&d.entries[1].body, crate::Body::Resumed { from_session, .. } if from_session == "s-1")
        );
        let d = back.handle(start(), 4);
        let pack = d.response.start_context.join("\n");
        assert!(pack.contains("fix the parser"), "{pack}");
    }
}
