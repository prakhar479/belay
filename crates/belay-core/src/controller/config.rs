//! Config and Response: the controller's settings and what it asks the connector to apply.

use super::*;

/// Initial values. Tune them in Phase 2 with real sessions.
#[derive(Debug, Clone)]
pub struct Config {
    /// Shape Bash output only above this size.
    pub shape_threshold_tokens: usize,
    /// Use a summary only if it is at most this fraction of the original.
    pub max_summary_ratio: f32,
    /// Distinct edits before an unchanged failure counts as a loop.
    pub loop_threshold: u64,
    /// Summaries without a re-open before the controller tightens a kind.
    pub tighten_after: u32,
    /// Loop time counts at most this much of any gap between events (idle time).
    pub idle_gap_minutes: u64,
    /// The test guard: deny (block a weakening edit), warn (say so), or off.
    pub guard: GuardSetting,
    /// Tell the agent when a failure loops or an edit cycles.
    pub loops: bool,
    /// Summarize large test and build output.
    pub shape: bool,
    /// Wrap test runs in `belay run`, so a failed run can be summarized too. A wrapped
    /// call can cost the agent some permission checking, so this is off by default.
    pub wrap_tests: bool,
    /// Verify before done (U6.1): at the end of a turn with files changed since the
    /// last passing test run, ask the agent to run it before it finishes.
    pub verify: bool,
    /// Memory (F2, U4b): handoffs, recall at the start, fixes other sessions learned,
    /// and the project's notes.
    pub memory: bool,
    /// Tokens the recall at a session's start, and the carry pack after a compaction,
    /// may take, at most.
    pub recall_tokens: usize,
    /// Awareness (F1): tell the agent what other belay sessions here did that matters.
    pub awareness: bool,
    /// The commons (C1–C4): who else is here, handoffs, and other sessions' messages.
    /// The user's own messages reach the agent either way.
    pub commons: bool,
    /// At a fresh start, one line points the agent at belay's routine (handoff, task,
    /// brief). The CLI's `agent_skill` sets it, which also gives the agent belay's skill
    /// and tools.
    pub routine: bool,
    /// How much the user hears about what belay did.
    pub report: ReportLevel,
    /// Settings the core does not read (the launcher's), as `name → value`. Only their
    /// overrides, so the session-start record names every setting the user changed.
    pub other_overrides: BTreeMap<String, String>,
}

/// The line a fresh session gets (see `Config::routine`).
pub(super) const ROUTINE_LINE: &str = "belay: this session runs under belay, which keeps memory and a record outside this conversation. Use it as the belay skill says: at the start, read the last handoff (handoff_get or `belay handoff`) and say what you work on (`belay card --task \"…\"`); before you say you are done, run `belay brief`; before you stop, leave `belay handoff \"done: …; open: …; next: …\"`.";

impl Default for Config {
    fn default() -> Self {
        Self {
            shape_threshold_tokens: 2000,
            max_summary_ratio: 0.7,
            loop_threshold: 3,
            tighten_after: 5,
            idle_gap_minutes: 10,
            guard: GuardSetting::Deny,
            loops: true,
            shape: true,
            wrap_tests: false,
            verify: true,
            memory: true,
            recall_tokens: 600,
            awareness: true,
            commons: true,
            routine: true,
            report: ReportLevel::Quiet,
            other_overrides: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Every core knob as `name → value`, in a stable order.
    pub fn fields(&self) -> BTreeMap<String, String> {
        let on_off = |b: bool| if b { "on" } else { "off" }.to_string();
        [
            ("idle_gap_minutes", self.idle_gap_minutes.to_string()),
            ("guard", self.guard.name().to_string()),
            ("loops", on_off(self.loops)),
            ("shape", on_off(self.shape)),
            ("wrap_tests", self.wrap_tests.to_string()),
            ("verify", on_off(self.verify)),
            ("memory", on_off(self.memory)),
            ("awareness", on_off(self.awareness)),
            ("commons", on_off(self.commons)),
            ("agent_skill", on_off(self.routine)),
            ("report", self.report.name().to_string()),
            ("loop_threshold", self.loop_threshold.to_string()),
            ("max_summary_ratio", self.max_summary_ratio.to_string()),
            (
                "shape_threshold_tokens",
                self.shape_threshold_tokens.to_string(),
            ),
            ("tighten_after", self.tighten_after.to_string()),
            ("recall_tokens", self.recall_tokens.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    /// Knobs that differ from the defaults, with `other_overrides`.
    pub fn overrides(&self) -> BTreeMap<String, String> {
        let defaults = Config::default().fields();
        let mut out: BTreeMap<String, String> = self
            .fields()
            .into_iter()
            .filter(|(k, v)| defaults.get(k) != Some(v))
            .collect();
        out.extend(self.other_overrides.clone());
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permission {
    Ask(String),
    Deny(String),
}

/// What the connector must do now. Empty = do nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Response {
    pub replace_output: Option<String>,
    /// Run this Bash call through `belay run` (PreToolUse input rewrite).
    pub wrap: bool,
    pub add_context: Vec<String>,
    pub permission: Option<Permission>,
    /// The agent may retry a denied call (R3.7).
    pub retry: bool,
    /// For the user only (rule 5): the connector shows these through user channels.
    pub notices: Vec<Notice>,
    /// Keep the agent working at the end of its turn, with this reason.
    pub continue_turn: Option<String>,
    /// Context for the start of a session.
    pub start_context: Vec<String>,
}

impl Response {
    pub fn is_empty(&self) -> bool {
        self.replace_output.is_none()
            && !self.wrap
            && self.add_context.is_empty()
            && self.permission.is_none()
            && !self.retry
            && self.notices.is_empty()
            && self.continue_turn.is_none()
            && self.start_context.is_empty()
    }

    pub fn merge(&mut self, other: Response) {
        if other.replace_output.is_some() {
            self.replace_output = other.replace_output;
        }
        self.wrap |= other.wrap;
        self.retry |= other.retry;
        self.add_context.extend(other.add_context);
        if self.permission.is_none() {
            self.permission = other.permission;
        }
        self.notices.extend(other.notices);
        if self.continue_turn.is_none() {
            self.continue_turn = other.continue_turn;
        }
        self.start_context.extend(other.start_context);
    }
}
