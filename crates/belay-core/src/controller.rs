//! Controller: the closed loop. Sense → judge → decide → act → record.
//!
//! The controller is the only writer of ledger entries. It returns a `Decision`:
//! entries and blobs to persist, plus a `Response` for the connector to apply.
//! belay acts within the agent's capabilities. A feature that is off or paused takes no
//! action; an action is recorded with `applied: false` only when the agent lacks the
//! capability or the reply reaches no one (D9).

use std::collections::{BTreeMap, HashMap};

/// An edit, by path and content hashes. A blocked edit is allowed by this key.
type EditKey = (String, String, String);

use crate::broker::{Broker, Question};
use crate::capability::Capabilities;
use crate::event::{Event, ToolKind};
use crate::guard::GuardDecision;
use crate::loops::{LoopDetector, LoopSignal};
use crate::perception::{
    classify, command_key, extract_failures, shell, writes_files, Failure, OutputKind,
};
use crate::record::{ActionKind, Body, Entry, Notice, SteerTrigger, LEDGER_FORMAT};
use crate::session::{Change, SessionModel};
use crate::shape::{shape, Shaped};
use crate::text::{estimate_tokens, sha256_hex};

/// Largest edit text (old or new) kept as a blob.
const MAX_EDIT_BLOB_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub hash: String,
    pub content: String,
}

#[derive(Debug, Default)]
pub struct Decision {
    pub entries: Vec<Entry>,
    pub blobs: Vec<Blob>,
    pub response: Response,
}

pub struct Controller {
    cfg: Config,
    caps: Capabilities,
    /// Features the user paused (`belay pause`).
    paused: std::collections::BTreeSet<Feature>,
    /// The reply reaches the agent. False while handling an event whose reply comes
    /// too late (D9): actions are then recorded as not applied.
    deliver: bool,
    session: SessionModel,
    loops: LoopDetector,
    broker: Broker,
    next_seq: u64,
    next_output: u64,
    next_action: u64,
    outputs: HashMap<String, OutputKind>,
    /// Blocked guard actions, by action id, with the guard rules behind each, and the
    /// most recent one.
    blocked: BTreeMap<String, (EditKey, Vec<String>)>,
    last_blocked: Option<String>,
    /// Edits the user allowed once, with the action id and the override entry seq.
    allowed: BTreeMap<EditKey, (String, u64)>,
    /// Bash calls wrapped by `belay run`, by tool use id.
    wrapped: BTreeMap<String, Wrapped>,
    /// The repo's `.belay.toml` (E7).
    policy: crate::policy::Policy,
    /// (actual, expected) values the latest failing run of each command showed (E4).
    failure_values: BTreeMap<String, Vec<(String, String)>>,
    /// Claude Code ran a call as written although belay rewrote it: wrap nothing more.
    rewrite_ignored: bool,
    /// Command keys never to wrap again: a wrapped run of them got no tool result
    /// (Claude Code's auto-mode classifier can deny a rewrite as a bypass).
    no_wrap: std::collections::BTreeSet<String>,
    /// The guard's memory: first texts of shell-written tests, the project top, and
    /// what this turn's edits added and removed.
    guard: guard::GuardState,
    /// The last summary the agent saw, to spot an immediate re-run of its command.
    last_summary: Option<LastSummary>,
    /// Set by `observe` in deferred mode; `filter` moves it into the wrapped call.
    deferred: Option<PendingShape>,
    /// Active clock for loop time: wall time with idle gaps capped.
    last_ms: Option<u64>,
    active_ms: u64,
    /// What other belay sessions did that matters here (F1).
    fleet: crate::fleet::Fleet,
    /// This session is a slot of a fleet (F5a): its agent may not touch the fleet
    /// identity or run fleet control.
    in_fleet: bool,
    /// Its role's write scope and others' claims (F5c).
    fleet_rules: Option<crate::fleet::scope::Rules>,
    /// Shared learning (F2).
    learning: crate::learn::Learning,
    /// Memory in this session (U4b): recall, the carry pack, the handoff's failure.
    mem: memory::MemoryState,
    /// The project's notes, for the session start.
    notes: Vec<String>,
    /// The commons card (C1): who this session is, what it said, what belay saw.
    identity: crate::commons::Identity,
    said: crate::commons::AgentCard,
    /// The sandbox this session runs work in now, for its card.
    sandbox: Option<String>,
    facts: crate::commons::Facts,
    last_test: Option<crate::commons::LastTest>,
    card_counts: (u64, u64, u64),
    started: Option<(u64, String, String)>,
    /// Other live sessions (all, in this repo), from the daemon, for the start line.
    peers: (usize, usize),
    findings_here: usize,
    handoff: Option<crate::commons::Handoff>,
    /// Commons sends (C2): rate limits, and who last asked this session a question.
    limiter: crate::commons::Limiter,
    asked_by: Option<String>,
    /// Sessions whose question reached this one but was not delivered yet.
    questions_from: std::collections::BTreeSet<String>,
    /// The Bash run being observed exited non-zero; None: not known (a wrapped run).
    run_failed: Option<bool>,
    /// This session continues earlier runs (U8): its start gets the carry pack, not the
    /// recall a new session gets.
    restored: bool,
    /// Verify before done (U6.1): edits since the last passing run.
    verify: verify::VerifyState,
    /// Guard rules the user's allows made warn (U6.3).
    guard_fb: features::GuardFeedback,
}

/// Whether `observe` may put a summary in place.
#[derive(Debug, Clone, Copy)]
enum Replace {
    /// Decide now; the bool is whether the agent supports replacing this output.
    Now(bool),
    /// `belay run` prints it; record it once the tool result shows what the agent saw.
    Deferred,
}

#[derive(Debug, Clone)]
struct LastSummary {
    command: String,
    output_id: String,
    kind: OutputKind,
    /// Edit count when the summary was shown.
    edits: u64,
}

#[derive(Debug, Clone)]
struct Wrapped {
    /// The command the agent asked for.
    command: String,
    /// Id of the wrap action.
    action_id: String,
    /// Seq of the wrap action (cause of the output entry).
    action_seq: u64,
    /// `belay run` reported the output.
    filtered: bool,
    /// A summary `belay run` was told to print. It is recorded once the tool result
    /// shows what the agent really saw (R0.2).
    pending: Option<PendingShape>,
    /// Context decided while `belay run` reported the output (a steer, a recall): the
    /// filter reply carries only the text to print, so it goes with the tool result.
    context: Vec<String>,
    /// The actions that context belongs to, for the ledger if it never goes out.
    context_actions: Vec<String>,
}

#[derive(Debug, Clone)]
struct PendingShape {
    action: ActionKind,
    reason: String,
    text: String,
    kind: OutputKind,
    /// Seq of the output entry.
    out: u64,
    command: String,
    output_id: String,
}

impl Controller {
    pub fn new(cfg: Config, caps: Capabilities) -> Self {
        Self {
            session: SessionModel::new(cfg.tighten_after),
            paused: std::collections::BTreeSet::new(),
            deliver: true,
            loops: LoopDetector::new(cfg.loop_threshold),
            cfg,
            caps,
            broker: Broker::new(),
            next_seq: 0,
            next_output: 1,
            next_action: 1,
            outputs: HashMap::new(),
            blocked: BTreeMap::new(),
            last_blocked: None,
            allowed: BTreeMap::new(),
            wrapped: BTreeMap::new(),
            policy: crate::policy::Policy::default(),
            failure_values: BTreeMap::new(),
            rewrite_ignored: false,
            no_wrap: std::collections::BTreeSet::new(),
            guard: guard::GuardState::default(),
            last_summary: None,
            deferred: None,
            last_ms: None,
            active_ms: 0,
            fleet: crate::fleet::Fleet::default(),
            in_fleet: false,
            fleet_rules: None,
            learning: crate::learn::Learning::default(),
            mem: memory::MemoryState::default(),
            notes: Vec::new(),
            identity: crate::commons::Identity::default(),
            said: crate::commons::AgentCard::default(),
            sandbox: None,
            facts: crate::commons::Facts::default(),
            last_test: None,
            card_counts: (0, 0, 0),
            started: None,
            peers: (0, 0),
            findings_here: 0,
            handoff: None,
            limiter: crate::commons::Limiter::default(),
            asked_by: None,
            questions_from: std::collections::BTreeSet::new(),
            run_failed: None,
            restored: false,
            verify: verify::VerifyState::default(),
            guard_fb: features::GuardFeedback::default(),
        }
    }

    /// Shared learning (F2): the project root, other sessions' fixes, and the
    /// project's approved notes. The daemon calls it before the session starts.
    pub fn set_learning(
        &mut self,
        project: &str,
        root: &str,
        known: Vec<crate::learn::KnownFix>,
        notes: Vec<String>,
    ) {
        self.learning.set_context(project, root, known);
        self.notes = notes;
    }

    /// Continue the ids of a ledger an earlier daemon of this session wrote (a plugin
    /// daemon that exited idle, or crashed): entries, actions, and outputs go on from
    /// there, so `a-N` and `o-N` keep naming one thing each.
    pub fn resume_ids(&mut self, next_seq: u64, next_action: u64, next_output: u64) {
        self.next_seq = self.next_seq.max(next_seq);
        self.next_action = self.next_action.max(next_action);
        self.next_output = self.next_output.max(next_output);
    }

    /// Whether this session's workspace is known (the daemon sets it at start).
    pub fn has_workspace(&self) -> bool {
        self.fleet.workspace().is_some()
    }

    /// Who this session is in the commons (C1), from the daemon.
    pub fn set_identity(&mut self, identity: crate::commons::Identity) {
        self.identity = identity;
    }

    /// Other live sessions on the machine, and in this repo, before the session starts.
    pub fn set_peers(&mut self, all: usize, here: usize) {
        self.peers = (all, here);
    }

    /// What the last session in this repo left (C4), before this one starts.
    pub fn set_handoff(&mut self, h: Option<crate::commons::Handoff>) {
        self.handoff = h;
    }

    /// Findings published for this repo (C3), for the start line.
    pub fn set_findings_here(&mut self, n: usize) {
        self.findings_here = n;
    }

    /// Fixes other sessions learned after this one started (the daemon reads the
    /// store's new lines).
    pub fn add_known_fixes(&mut self, fixes: Vec<crate::learn::KnownFix>) {
        self.learning.add_known(fixes);
    }

    /// Whether context added now reaches the agent.
    fn says(&self) -> bool {
        self.deliver && self.caps.inject_context
    }

    /// What the card shows of the kill switch: empty unless something is paused.
    fn paused_label(&self) -> String {
        if self.paused.is_empty() {
            String::new()
        } else if self.paused.len() == Feature::ALL.len() {
            "paused".into()
        } else {
            let names: Vec<&str> = self.paused.iter().map(|f| f.name()).collect();
            format!("paused: {}", names.join(", "))
        }
    }

    /// The fleet role's write scope (F5c), from the hub.
    pub fn set_fleet_rules(&mut self, rules: crate::fleet::scope::Rules) {
        self.fleet_rules = Some(rules);
    }

    /// Other sessions' claims, as the daemon last read them.
    pub fn set_fleet_claims(&mut self, claims: &[crate::fleet::scope::ClaimFile]) {
        if let Some(r) = &mut self.fleet_rules {
            r.set_claims(claims);
        }
    }

    /// The session runs as a slot of a fleet (`BELAY_FLEET_SLOT` was set).
    pub fn set_in_fleet(&mut self, in_fleet: bool) {
        self.in_fleet = in_fleet;
    }

    /// Advance the active clock to `now_ms`. A gap longer than `idle_gap_minutes`
    /// counts as that long, so a session left open overnight does not inflate loop time.
    fn tick(&mut self, now_ms: u64) -> u64 {
        let cap = self.cfg.idle_gap_minutes.saturating_mul(60_000);
        if let Some(last) = self.last_ms {
            self.active_ms += now_ms.saturating_sub(last).min(cap);
        }
        self.last_ms = Some(self.last_ms.map_or(now_ms, |l| l.max(now_ms)));
        self.active_ms
    }

    /// Handle an event. With `deliver` false the reply would reach the agent too late
    /// (D9): the ledger records no action as applied, and there is no response.
    pub fn handle_with(&mut self, event: Event, now_ms: u64, deliver: bool) -> Decision {
        if deliver {
            return self.handle(event, now_ms);
        }
        self.deliver = false;
        let mut d = self.handle(event, now_ms);
        self.deliver = true;
        d.response = Response::default();
        d
    }

    /// The reply with these decisions' applied actions reached no one (D9). Record that,
    /// and drop state that assumed the agent got them.
    pub fn undelivered(&mut self, decisions: &[Decision], now_ms: u64) -> Decision {
        let mut out = Decision::default();
        let (mut ids, mut causes) = (Vec::new(), Vec::new());
        for e in decisions.iter().flat_map(|d| &d.entries) {
            if let Body::Action {
                id,
                action,
                applied: true,
                ..
            } = &e.body
            {
                ids.push(id.clone());
                causes.push(e.seq);
                if matches!(action, ActionKind::Shape { .. }) {
                    self.last_summary = None;
                }
            }
        }
        if ids.is_empty() {
            return out;
        }
        self.forget_undelivered(&ids, &causes);
        let reason =
            "The hook reply came after the shim's timeout, so the agent did not get these actions.";
        self.push(
            &mut out,
            now_ms,
            causes,
            Body::Undelivered {
                actions: ids,
                reason: reason.into(),
            },
        );
        out
    }

    /// Drop state that assumed the agent got these actions (`seqs`: their entries).
    fn forget_undelivered(&mut self, ids: &[String], seqs: &[u64]) {
        for id in ids {
            self.blocked.remove(id);
            if self.last_blocked.as_ref() == Some(id) {
                self.last_blocked = None;
            }
        }
        // The agent runs the command as it wrote it.
        self.wrapped.retain(|_, w| !seqs.contains(&w.action_seq));
    }

    /// The user allows one blocked edit (the latest one if `action` is None).
    /// Err is a message for the user.
    pub fn allow(&mut self, action: Option<&str>, now_ms: u64) -> Result<Decision, String> {
        let id = match action {
            Some(a) => a.to_string(),
            None => self
                .last_blocked
                .clone()
                .ok_or("No edit was blocked in this session.")?,
        };
        let (key, rules) = self
            .blocked
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("{id} is not a blocked edit."))?;
        let mut d = Decision::default();
        let seq = self.push(
            &mut d,
            now_ms,
            vec![],
            Body::Override { action: id.clone() },
        );
        self.memory_on_guard(format!("The user allowed the edit {id} blocked, once."));
        self.allowed.insert(key, (id, seq));
        self.guard_allowed(&mut d, now_ms, &rules, seq);
        Ok(d)
    }

    pub fn session(&self) -> &SessionModel {
        &self.session
    }

    pub fn broker_mut(&mut self) -> &mut Broker {
        &mut self.broker
    }

    /// What this session learned that the next one in the same project can start from:
    /// the shape level of each kind (E9).
    pub fn learned_levels(&self) -> BTreeMap<String, u8> {
        self.session
            .levels()
            .iter()
            .map(|(k, v)| (k.label().to_string(), *v))
            .collect()
    }

    /// Start from what earlier sessions in this project learned. Only kinds shaped by
    /// default can be carried over (C1: code reads and diffs are never shaped).
    pub fn restore_levels(&mut self, levels: &BTreeMap<String, u8>, now_ms: u64) -> Decision {
        let mut d = Decision::default();
        for (label, level) in levels {
            let Some(kind) = OutputKind::from_label(label).filter(|k| k.shaped_by_default()) else {
                continue;
            };
            let from = self.session.shape_level(kind);
            let to = (*level).min(crate::session::MAX_LEVEL);
            if to == from {
                continue;
            }
            self.session.set_level(kind, to);
            self.push(
                &mut d,
                now_ms,
                vec![],
                Body::Adaptation {
                    knob: format!("shape_level.{label}"),
                    from: from.to_string(),
                    to: to.to_string(),
                    reason: "Carried over from earlier sessions in this project.".into(),
                },
            );
        }
        d
    }

    /// The repo's `.belay.toml`: extra test paths and protected paths for the guard.
    pub fn set_policy(&mut self, policy: crate::policy::Policy) {
        self.policy = policy;
    }

    pub fn handle(&mut self, event: Event, now_ms: u64) -> Decision {
        // Other sessions' activity is not this session's: it must not make idle time
        // here count as active (loop time).
        if !matches!(
            event,
            Event::Peer { .. }
                | Event::Workspace { .. }
                | Event::Message { .. }
                | Event::CardSet { .. }
        ) {
            self.tick(now_ms);
        }
        let for_card = matches!(
            event,
            Event::PromptSubmitted { .. }
                | Event::TurnEnded
                | Event::ToolCompleted { .. }
                | Event::FileEdited { .. }
                | Event::Peer { .. }
                | Event::Workspace { .. }
                | Event::Message { .. }
                | Event::CardSet { .. }
        )
        .then(|| event.clone())
        .unwrap_or(Event::Compacted {
            trigger: String::new(),
        });
        let mut d = Decision::default();
        let is_bash = matches!(
            &event,
            Event::ToolCompleted {
                kind: ToolKind::Shell,
                ..
            }
        );
        let is_read = matches!(
            &event,
            Event::ToolCompleted {
                kind: ToolKind::Read | ToolKind::Search,
                ..
            }
        );
        // Reads before a compaction or in another conversation are out of context (U5.1).
        if matches!(
            event,
            Event::Compacted { .. }
                | Event::ContextRestarted { .. }
                | Event::ConversationChanged { .. }
        ) {
            self.session.reads_mut().on_compacted();
        }
        // Fleet (F1): hook events can carry notices; this session's edits and failures
        // are what peers' activity is compared with.
        let delivers = matches!(
            &event,
            Event::ToolRequested { .. }
                | Event::EditProposed { .. }
                | Event::ToolCompleted { .. }
                | Event::PromptSubmitted { .. }
        );
        let my_edit = match &event {
            Event::FileEdited { path, .. } | Event::FileChanged { path, .. } => Some(path.clone()),
            _ => None,
        };
        // Learning (F2) keeps what an edit changed: proposed text joins a fix once the
        // edit lands; a shell write's snapshot gives it directly.
        match &event {
            Event::EditProposed {
                tool_use_id,
                path,
                old_text,
                new_text,
            } => self
                .learning
                .on_proposed(tool_use_id, path, old_text, new_text),
            Event::FileEdited {
                tool_use_id, path, ..
            } => self.learning.on_edited(tool_use_id, path),
            Event::FileChanged {
                path,
                old_text,
                new_text,
                ..
            } => self.learning.on_changed(path, old_text, new_text),
            _ => {}
        }
        if let Event::ToolCompleted {
            output,
            failed: true,
            ..
        } = &event
        {
            self.fleet.on_my_failure(output, now_ms);
        }
        self.guard_before(&mut d, &event, now_ms);
        match event {
            _ if is_bash => self.on_bash(&mut d, event, now_ms),
            _ if is_read => self.on_read(&mut d, event, now_ms),
            Event::EditProposed { .. } => self.on_edit_proposed(&mut d, event, now_ms),
            Event::FileEdited { .. } => self.on_file_edited(&mut d, event, now_ms),
            Event::FileChanged { .. } => self.on_file_changed(&mut d, event, now_ms),
            Event::ToolDenied { .. } => self.on_tool_denied(&mut d, event, now_ms),
            Event::SessionStarted {
                agent,
                cwd,
                transcript_path,
                mode,
            } => {
                if self.started.is_none() {
                    self.started = Some((now_ms, agent.clone(), cwd.clone()));
                }
                let event = Event::SessionStarted {
                    agent,
                    cwd,
                    transcript_path,
                    mode,
                };
                let ev = self.push(&mut d, now_ms, vec![], Body::Event { event });
                let overrides = self.cfg.overrides();
                if !overrides.is_empty() {
                    self.push(&mut d, now_ms, vec![ev], Body::Config { overrides });
                }
                // The project's notes, kept by the user (F2, L5).
                // A resumed conversation has had its start: it gets the carry pack (U8).
                let fresh = !self.restored;
                if fresh && !self.notes.is_empty() && self.on(Feature::Memory) {
                    let lines: Vec<String> = self.notes.iter().map(|n| format!("- {n}")).collect();
                    let text = format!(
                        "belay: notes for this project, kept by the user:\n{}",
                        lines.join("\n")
                    );
                    let applied = self.says();
                    let action = ActionKind::Notes {
                        count: self.notes.len(),
                        text: text.clone(),
                    };
                    let reason = format!("The project has {} approved note(s).", self.notes.len());
                    self.record(
                        &mut d,
                        now_ms,
                        vec![ev],
                        action,
                        reason,
                        applied,
                        !self.caps.inject_context,
                    );
                    if applied {
                        d.response.add_context.push(text);
                    }
                }
                // Memory (U4b): the last handoff here and the best memories.
                let recalled_handoff = if fresh {
                    self.recall_at_start(&mut d, ev, now_ms)
                } else {
                    let reason = "The conversation was resumed: this session's task, failure, and memories go with it.";
                    self.carry_pack(&mut d, ev, now_ms, reason.into());
                    false
                };
                // The commons (C1): who else is here, and how to say what this does.
                let (others, here) = self.peers;
                let findings = self.findings_here;
                if fresh && (others > 0 || findings > 0) && self.on(Feature::Commons) {
                    let mut parts = Vec::new();
                    if others > 0 {
                        parts.push(format!(
                            "{others} other belay session{} on this machine, {here} in this repo (`belay sessions` lists them, with what each is doing). Say what you are working on: `belay card --task \"…\"`.",
                            if others == 1 { "" } else { "s" }
                        ));
                    }
                    if findings > 0 {
                        parts.push(format!(
                            "{findings} finding{} for this repo from other sessions: `belay ask \"…\"` before you dig in. Publish what you learn with `belay share \"…\"`.",
                            if findings == 1 { "" } else { "s" }
                        ));
                    }
                    let text = format!("belay: {}", parts.join(" "));
                    let applied = self.says();
                    let reason = format!(
                        "{others} other live belay session(s) on this machine; {findings} finding(s) for this repo."
                    );
                    let action = ActionKind::Commons {
                        others,
                        here,
                        findings,
                        text: text.clone(),
                    };
                    self.record(
                        &mut d,
                        now_ms,
                        vec![ev],
                        action,
                        reason,
                        applied,
                        !self.caps.inject_context,
                    );
                    if applied {
                        d.response.add_context.push(text);
                    }
                }
                // C4: what the last session in this repo left.
                if let Some(h) = self.handoff.take() {
                    if fresh && self.on(Feature::Commons) && !h.is_empty() && !recalled_handoff {
                        let text = crate::commons::handoff_text(&h, now_ms);
                        let applied = self.says();
                        let reason = format!("{} ended last in this repo and left a card.", h.from);
                        let action = ActionKind::Handoff {
                            from: h.from.clone(),
                            text: text.clone(),
                        };
                        self.record(
                            &mut d,
                            now_ms,
                            vec![ev],
                            action,
                            reason,
                            applied,
                            !self.caps.inject_context,
                        );
                        if applied {
                            d.response.add_context.push(text);
                        }
                    }
                }
                // The routine: one line, once, so the agent knows belay is there to use.
                if fresh && self.cfg.routine {
                    let text = ROUTINE_LINE.to_string();
                    let applied = self.says();
                    let reason =
                        "A fresh session: the agent is pointed at belay's routine.".to_string();
                    self.record(
                        &mut d,
                        now_ms,
                        vec![ev],
                        ActionKind::Routine { text: text.clone() },
                        reason,
                        applied,
                        !self.caps.inject_context,
                    );
                    if applied {
                        d.response.add_context.push(text);
                    }
                }
            }
            Event::CardSet {
                name,
                task,
                note,
                offers,
                topics,
                handoff,
            } => {
                use crate::commons::{clean, MAX_NOTE, MAX_OFFER, MAX_OFFERS, MAX_TASK};
                // The agent's own words: cleaned, then recorded as it will show.
                let name = name.filter(|n| crate::commons::valid_name(n));
                let task = task.map(|t| clean(&t, MAX_TASK));
                let note = note.map(|t| clean(&t, MAX_NOTE));
                let offers = offers.map(|o| {
                    o.iter()
                        .map(|x| clean(x, MAX_OFFER))
                        .filter(|x| !x.is_empty())
                        .take(MAX_OFFERS)
                        .collect::<Vec<_>>()
                });
                if let Some(n) = &name {
                    self.said.name = Some(n.clone());
                }
                if let Some(t) = &task {
                    self.said.task = (!t.is_empty()).then(|| t.clone());
                }
                if let Some(t) = &note {
                    self.said.note = (!t.is_empty()).then(|| t.clone());
                }
                if let Some(o) = &offers {
                    self.said.offers = o.clone();
                }
                let topics = topics.map(|t| {
                    let mut t: Vec<String> = t
                        .into_iter()
                        .filter(|x| crate::commons::valid_name(x))
                        .collect();
                    t.sort();
                    t.dedup();
                    t.truncate(crate::commons::MAX_TOPICS);
                    t
                });
                if let Some(t) = &topics {
                    self.said.topics = t.clone();
                }
                let handoff = handoff.map(|t| clean(&t, crate::commons::MAX_HANDOFF));
                if let Some(t) = &handoff {
                    self.said.handoff = (!t.is_empty()).then(|| t.clone());
                }
                let event = Event::CardSet {
                    name,
                    task,
                    note,
                    offers,
                    topics,
                    handoff,
                };
                self.push(&mut d, now_ms, vec![], Body::Event { event });
            }
            Event::ToolRequested { .. } => self.on_tool_requested(&mut d, event, now_ms),
            Event::ContextRestarted { .. } => self.on_context_restarted(&mut d, event, now_ms),
            Event::ConversationChanged { .. } => {
                self.push(&mut d, now_ms, vec![], Body::Event { event });
                // Failures, edits, and blocks of the old conversation are not this one's.
                self.loops = LoopDetector::new(self.cfg.loop_threshold);
                self.last_summary = None;
                self.wrapped.clear();
                self.blocked.clear();
                self.last_blocked = None;
                self.allowed.clear();
                self.failure_values.clear();
                self.fleet.reset_conversation();
            }
            Event::Message {
                from,
                kind,
                text,
                channel,
                from_session,
                repo,
            } => {
                let commons = channel.as_deref() == Some("commons");
                // Only the user's own terminal writes `user` with no session: a card
                // named so (older builds allowed it) is that session, not the user.
                let from = match (&from_session, commons && from == "user") {
                    (Some(id), true) => format!("session {id}"),
                    _ => from,
                };
                let event = Event::Message {
                    from: from.clone(),
                    kind: kind.clone(),
                    text: text.clone(),
                    channel,
                    from_session,
                    repo: repo.clone(),
                };
                let ev = self.push(&mut d, now_ms, vec![], Body::Event { event });
                if commons {
                    if kind == "question" {
                        self.questions_from.insert(from.clone());
                    }
                    self.fleet
                        .on_commons_message(&from, repo.as_deref(), &kind, &text, ev, now_ms);
                } else {
                    self.fleet.on_message(&from, &kind, &text, ev, now_ms);
                }
            }
            Event::SandboxStarted { ref id, .. } => {
                self.sandbox = Some(id.clone());
                self.push(&mut d, now_ms, vec![], Body::Event { event });
            }
            Event::SandboxStopped { ref id, .. } => {
                if self.sandbox.as_deref() == Some(id.as_str()) {
                    self.sandbox = None;
                }
                self.push(&mut d, now_ms, vec![], Body::Event { event });
            }
            Event::Workspace { top, common } => {
                self.fleet.set_workspace(&top, &common, now_ms);
                let event = Event::Workspace { top, common };
                self.push(&mut d, now_ms, vec![], Body::Event { event });
            }
            Event::Peer {
                session,
                seq,
                at_ms,
                agent,
                activity,
            } => {
                let event = Event::Peer {
                    session: session.clone(),
                    seq,
                    at_ms,
                    agent: agent.clone(),
                    activity: activity.clone(),
                };
                let ev = self.push(&mut d, now_ms, vec![], Body::Event { event });
                self.fleet.on_peer(&session, &agent, at_ms, &activity, ev);
            }
            Event::TurnEnded => {
                let ev = self.push(&mut d, now_ms, vec![], Body::Event { event });
                self.end_turn(&mut d, now_ms, ev);
                self.verify_at_end(&mut d, now_ms, ev);
            }
            other => {
                self.push(&mut d, now_ms, vec![], Body::Event { event: other });
            }
        }
        if let Some(path) = my_edit {
            self.verify.on_edit(&path);
            self.fleet.on_my_edit(&path, now_ms);
        }
        if delivers {
            self.deliver_fleet(&mut d, now_ms);
        }
        self.note_for_card(&for_card, &d, now_ms);
        self.add_notice(&mut d, now_ms, true);
        d
    }

    /// Tell the agent pending fleet notices its features let through, and record each as
    /// an action caused by the peer's event and by this hook's event.
    fn deliver_fleet(&mut self, d: &mut Decision, now: u64) {
        let groups = self.fleet.take();
        if groups.is_empty() {
            return;
        }
        let trigger = d
            .entries
            .iter()
            .find(|e| matches!(e.body, Body::Event { .. }))
            .map(|e| e.seq);
        let supported = self.caps.inject_context;
        let applied = self.says();
        // Each notice is its own action; a group reaches the agent as one message.
        for group in groups {
            // A fleet message is the fleet's own traffic: `awareness` covers only the
            // notices about peers' activity (F1), not what sessions send each other (F4).
            // C2: the user's own commons messages always go; other sessions' only with
            // `commons = on` (owner, 2026-10-22).
            let wanted = match group[0].kind {
                crate::fleet::NoticeKind::Message => true,
                crate::fleet::NoticeKind::Commons => {
                    group[0].peer == "user" || self.on(Feature::Commons)
                }
                _ => self.on(Feature::Awareness),
            };
            if !wanted {
                continue;
            }
            let text = crate::fleet::message(&group);
            // A question the agent sees makes its next plain reply an answer (C2).
            if applied
                && group[0].kind == crate::fleet::NoticeKind::Commons
                && self.questions_from.remove(&group[0].peer)
            {
                self.asked_by = Some(group[0].peer.clone());
            }
            for n in group {
                let mut causes = vec![n.cause];
                causes.extend(trigger);
                let action = ActionKind::Fleet {
                    notice: n.kind,
                    peer: n.peer,
                    path: n.path,
                    text: n.text,
                };
                self.record(d, now, causes, action, n.reason, applied, !supported);
            }
            if applied {
                d.response.add_context.push(text);
            }
        }
    }

    fn on_bash(&mut self, d: &mut Decision, event: Event, now: u64) {
        let Event::ToolCompleted {
            tool_use_id,
            tool,
            kind,
            input,
            output,
            failed,
        } = event
        else {
            return;
        };
        let command = input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // A wrapped call reports the original command; the hook sees the wrapper.
        let command = self
            .wrapped
            .get(&tool_use_id)
            .map(|w| w.command.clone())
            .unwrap_or(command);
        let info = classify(&command);
        let hash = sha256_hex(&output);
        let wrapped = self.wrapped.remove(&tool_use_id);
        let filtered = wrapped.as_ref().is_some_and(|w| w.filtered);
        // belay relies on `updatedInput` without a permission decision, which Claude Code
        // does not document (R3.1). If the call ran the agent's command, not the wrapper,
        // the wrap did not happen: say so, and stop wrapping (I6, F8).
        let ran_wrapper = input
            .get("command")
            .and_then(|v| v.as_str())
            .is_some_and(|c| c.contains(" run --id "));
        let ignored = wrapped.as_ref().filter(|_| !filtered && !ran_wrapper);
        let stored = Event::ToolCompleted {
            tool_use_id,
            tool,
            kind,
            input,
            output: format!("<blob:{hash}>"),
            failed,
        };
        let ev = self.push(d, now, vec![], Body::Event { event: stored });
        if let Some(w) = ignored {
            self.rewrite_ignored = true;
            self.push(
                d,
                now,
                vec![w.action_seq, ev],
                Body::Undelivered {
                    actions: vec![w.action_id.clone()],
                    reason: "Claude Code ran the command as written: it ignored belay's rewrite."
                        .into(),
                },
            );
        }
        if filtered {
            // `belay run` already recorded the full output; this is what the agent saw.
            if let Some(w) = wrapped {
                d.response.add_context.extend(w.context);
                if let Some(p) = w.pending {
                    self.confirm_shape(d, now, ev, p, &output);
                }
            }
            d.blobs.push(Blob {
                hash,
                content: output,
            });
            return;
        }

        // Re-open of a summary: the agent needed the full output.
        if let Some(target) = info.show_target.as_deref() {
            if let Some(kind) = self.outputs.get(target).copied() {
                if let Some(change) = self.session.on_reopen(kind, &format!("re-opened {target}")) {
                    self.adapt(d, now, ev, change);
                }
            }
            d.blobs.push(Blob {
                hash,
                content: output,
            });
            return;
        }

        let supported = if failed {
            self.caps.replace_failed_output
        } else {
            self.caps.replace_output
        };
        let mode = Replace::Now(supported);
        let runs = matches!(info.kind, OutputKind::Test | OutputKind::Build);
        let level = self.session.shape_level(info.kind);
        let p = prepare_for(&output, Some(hash), level, &self.cfg, runs);
        self.run_failed = Some(failed);
        let shown = self.observe(d, now, ev, &command, info.kind, output, p, mode);
        self.run_failed = None;
        if let Some(text) = shown {
            d.response.replace_output = Some(text);
        }
    }

    fn next_action_id(&mut self) -> String {
        let id = format!("a-{}", self.next_action);
        self.next_action += 1;
        id
    }

    fn supports(&self, action: &ActionKind) -> bool {
        match action {
            ActionKind::BlockCommand { .. } => self.caps.deny,
            ActionKind::Wrap { .. } => self.caps.rewrite_input,
            ActionKind::Retry { .. } => self.caps.retry_denied,
            ActionKind::Verify { .. } => self.caps.continue_turn,
            ActionKind::Shape { .. } => self.caps.replace_output,
            ActionKind::Steer { .. }
            | ActionKind::Fleet { .. }
            | ActionKind::Recall { .. }
            | ActionKind::Notes { .. }
            | ActionKind::Commons { .. }
            | ActionKind::Routine { .. }
            | ActionKind::Handoff { .. } => self.caps.inject_context,
            ActionKind::MemoryRecalled { .. } => self.caps.start_inject,
            ActionKind::Guard { decision, .. } => match decision {
                GuardDecision::Ask => self.caps.ask,
                GuardDecision::Deny => self.caps.deny,
                GuardDecision::Warn => self.caps.inject_context,
            },
        }
    }

    /// Record an action. Returns true if the connector must apply it.
    fn act(
        &mut self,
        d: &mut Decision,
        now: u64,
        cause: u64,
        action: ActionKind,
        reason: String,
    ) -> bool {
        let supported = self.supports(&action);
        self.act_if(d, now, cause, action, reason, supported)
    }

    /// `act` with support decided by the caller (it knows more than the action kind).
    fn act_if(
        &mut self,
        d: &mut Decision,
        now: u64,
        cause: u64,
        action: ActionKind,
        reason: String,
        supported: bool,
    ) -> bool {
        let applied = self.deliver && supported;
        self.record(d, now, vec![cause], action, reason, applied, !supported);
        applied
    }

    /// Push an action entry. Returns its seq.
    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        d: &mut Decision,
        now: u64,
        causes: Vec<u64>,
        action: ActionKind,
        reason: String,
        applied: bool,
        unsupported: bool,
    ) -> u64 {
        let id = self.next_action_id();
        self.push(
            d,
            now,
            causes,
            Body::Action {
                id,
                action,
                reason,
                applied,
                unsupported,
            },
        )
    }

    fn steer(
        &mut self,
        d: &mut Decision,
        now: u64,
        cause: u64,
        trigger: SteerTrigger,
        text: String,
        reason: String,
    ) {
        if !self.on(Feature::Loops) {
            return;
        }
        let action = ActionKind::Steer {
            trigger,
            text: text.clone(),
        };
        if self.act(d, now, cause, action, reason) {
            d.response.add_context.push(text);
        }
    }

    fn adapt(&mut self, d: &mut Decision, now: u64, cause: u64, c: Change) {
        self.push(
            d,
            now,
            vec![cause],
            Body::Adaptation {
                knob: c.knob,
                from: c.from,
                to: c.to,
                reason: c.reason,
            },
        );
    }

    fn push(&mut self, d: &mut Decision, now: u64, causes: Vec<u64>, body: Body) -> u64 {
        if self.next_seq == 0 {
            self.next_seq = 1;
            d.entries.push(Entry {
                seq: 0,
                ts_ms: now,
                causes: vec![],
                body: Body::Format {
                    version: LEDGER_FORMAT,
                },
            });
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        d.entries.push(Entry {
            seq,
            ts_ms: now,
            causes,
            body,
        });
        seq
    }
}

mod belay_cmd;
mod commons;
mod config;
mod features;
mod guard;
mod memory;
mod notice;
mod reads;
mod restore;
mod shaping;
mod snapshot;
mod tools;
pub(crate) mod verify;

use config::ROUTINE_LINE;
pub use config::{Config, Permission, Response};
pub use features::{Feature, GuardSetting, ReportLevel, ALL_FEATURES};
pub use notice::notice_for;
use shaping::prepare_for;
pub use shaping::{prepare, Prepared};

#[cfg(test)]
mod tests;
