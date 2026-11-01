//! Working-tree snapshots and `belay undo` (U6.2). The daemon takes the snapshots (git
//! is I/O); the controller records them, so it stays the only writer of the ledger.

use super::{Controller, Decision};
use crate::record::Body;

impl Controller {
    /// Record snapshot `id` (`t-0` at the start, `t-N` at the end of turn N), kept as
    /// `commit`. `files` changed since the previous snapshot; `cause` is the event that
    /// ended the turn or started the session.
    pub fn snapshot(
        &mut self,
        id: &str,
        commit: &str,
        files: Vec<String>,
        cause: Option<u64>,
        now_ms: u64,
    ) -> Decision {
        let reason = match id.strip_prefix("t-") {
            Some("0") => "Kept the working tree as the session started, for `belay undo`.".into(),
            Some(n) => format!("Kept the working tree at the end of turn {n}, for `belay undo`."),
            None => format!("Kept the working tree as snapshot {id}, for `belay undo`."),
        };
        let mut d = Decision::default();
        let body = Body::Snapshot {
            id: id.to_string(),
            commit: commit.to_string(),
            files,
            reason,
            undo: false,
        };
        self.push(&mut d, now_ms, cause.into_iter().collect(), body);
        d
    }

    /// The user rolled the files back to snapshot `id` (`belay undo`). `before` keeps the
    /// tree as it was, so the undo can be undone; `files` were restored or removed.
    pub fn undone(&mut self, id: &str, before: &str, files: Vec<String>, now_ms: u64) -> Decision {
        let mut d = Decision::default();
        let body = Body::Snapshot {
            id: id.to_string(),
            commit: before.to_string(),
            reason: format!(
                "The user rolled {} file{} back to snapshot {id} (`belay undo`).",
                files.len(),
                if files.len() == 1 { "" } else { "s" }
            ),
            files,
            undo: true,
        };
        self.push(&mut d, now_ms, vec![], body);
        d
    }
}

#[cfg(test)]
mod tests {
    use crate::{Capabilities, Config, Controller, Event, Permission, ToolKind};

    #[test]
    fn snapshots_and_undos_are_recorded_with_a_reason_and_cause() {
        let mut c = Controller::new(Config::default(), Capabilities::CLAUDE_CODE);
        let turn = c.handle(Event::TurnEnded, 1);
        let cause = turn.entries.last().unwrap().seq;
        let d = c.snapshot("t-1", "abc", vec!["a.txt".into()], Some(cause), 2);
        let e = &d.entries[0];
        assert_eq!(e.causes, vec![cause]);
        assert!(e.seq > cause);
        let json = serde_json::to_string(e).unwrap();
        assert!(json.contains(r#""kind":"snapshot""#), "{json}");
        assert!(json.contains("end of turn 1"), "{json}");
        assert!(!json.contains("undo\":"), "{json}");
        let d = c.undone("t-1", "def", vec!["a.txt".into(), "b.txt".into()], 3);
        let json = serde_json::to_string(&d.entries[0]).unwrap();
        assert!(json.contains(r#""undo":true"#), "{json}");
        assert!(
            json.contains("rolled 2 files back to snapshot t-1"),
            "{json}"
        );
    }

    #[test]
    fn an_agent_may_not_run_belay_undo() {
        for cmd in [
            "belay undo t-1 --yes",
            "cd x && /usr/bin/belay undo",
            "git log; belay undo",
        ] {
            let mut c = Controller::new(Config::default(), Capabilities::CLAUDE_CODE);
            let d = c.handle(
                Event::ToolRequested {
                    tool_use_id: "u".into(),
                    tool: "Bash".into(),
                    kind: ToolKind::Shell,
                    input: serde_json::json!({ "command": cmd }),
                    rewrite_ok: false,
                },
                0,
            );
            assert!(
                matches!(&d.response.permission, Some(Permission::Deny(m)) if m.contains("only the user rolls files back")),
                "{cmd}: {:?}",
                d.response
            );
        }
    }
}
