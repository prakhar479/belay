//! Common event vocabulary. Connectors translate agent traffic into these events.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Observe and record. Never change the session.
    Look,
    /// Apply actions the agent supports.
    Optimize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// First event of a session. The controller fills `mode` when it records the event.
    SessionStarted {
        agent: String,
        cwd: String,
        transcript_path: Option<String>,
        #[serde(default)]
        mode: Option<Mode>,
    },
    /// The agent switched conversations in the same process (`/clear`, `/resume`): a new
    /// transcript, and earlier failures and edits belong to another conversation (I6, R3).
    ConversationChanged {
        transcript_path: Option<String>,
        reason: String,
    },
    /// A tool call before it runs (non-edit tools).
    ToolRequested {
        tool_use_id: String,
        tool: String,
        input: Value,
        /// The connector says rewriting this input adds no permission prompt.
        #[serde(default)]
        rewrite_ok: bool,
    },
    /// An edit the agent wants to make. Sent before the edit runs.
    EditProposed {
        tool_use_id: String,
        path: String,
        old_text: String,
        new_text: String,
    },
    /// A tool call after it ran. `output` is what the agent would see.
    ToolCompleted {
        tool_use_id: String,
        tool: String,
        input: Value,
        output: String,
        failed: bool,
    },
    /// An edit that happened. `after_hash` is the file content hash after the edit.
    FileEdited {
        tool_use_id: String,
        path: String,
        after_hash: Option<String>,
    },
    /// A file a shell command changed, seen after the command ran (R1.4). The connector
    /// compares the file with a snapshot from before the call. A missing file is "".
    FileChanged {
        tool_use_id: String,
        path: String,
        old_text: String,
        new_text: String,
    },
    /// A tool call that auto mode denied before it ran (R3.7).
    ToolDenied {
        tool_use_id: String,
        tool: String,
        reason: String,
    },
    PromptSubmitted {
        chars: usize,
    },
    TurnEnded,
    Compacted {
        trigger: String,
    },
    /// Where this session works, found once at start (F1): the worktree's top directory
    /// and the repo's common git dir. Outside git both are the working directory.
    Workspace {
        top: String,
        common: String,
    },
    /// A message to this session from another one in its fleet (F4), or from the user.
    /// It is the sender's text: information, never an instruction that belay obeys.
    Message {
        from: String,
        kind: String,
        text: String,
        /// `commons`: from any belay session, or the user's terminal (C2). None: the fleet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
        /// The sender's session id (commons), for `belay why` and replies.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_session: Option<String>,
        /// The sender's repo (commons).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
    /// This session published a finding (C3).
    FindingShared {
        id: String,
        kind: String,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tags: Vec<String>,
    },
    /// This session asked the commons (C3), and these findings came back.
    FindingsRead {
        query: String,
        ids: Vec<String>,
    },
    /// This session sent a commons message (C2), or was refused, with the reason.
    MessageSent {
        to: String,
        recipients: Vec<String>,
        kind: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refused: Option<String>,
    },
    /// The agent's own words for its commons card (C1, `belay card`): each field that is
    /// set replaces the card's. Its own data, not an instruction.
    CardSet {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offers: Option<Vec<String>>,
        /// The topics it is in, after a join or leave (C2).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        topics: Option<Vec<String>>,
        /// What it leaves for the next session in this repo (C4).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        handoff: Option<String>,
    },
    /// Something another live belay session on this machine did (F1). `session` and `seq`
    /// name the entry in that session's ledger; `at_ms` is when it happened there.
    Peer {
        session: String,
        seq: u64,
        at_ms: u64,
        agent: String,
        activity: PeerActivity,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PeerActivity {
    /// The peer's session started (or belay first saw it), with its workspace.
    Joined {
        cwd: String,
        top: String,
        common: String,
    },
    /// The peer changed a file: an edit tool or a shell command. `path` is absolute.
    Edited { path: String },
    /// The peer ran a git command that changes its checkout (see `fleet::git_effect`).
    Git { command: String },
    /// The peer's daemon is gone.
    Left,
}
