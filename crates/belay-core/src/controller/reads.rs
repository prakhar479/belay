//! Reads and searches the agent made (U5.1): recorded as outputs, never shaped, and
//! never seen by loops or the guard. What the agent asked to read is what it needs.

use super::{Blob, Controller, Decision};
use crate::event::{Event, ToolKind};
use crate::perception::OutputKind;
use crate::record::Body;
use crate::text::{estimate_tokens, sha256_hex};

impl Controller {
    /// A Read or Search call that completed. The output goes to a blob, as a Bash
    /// output does; a read's (path, content hash) goes to the session model.
    pub(super) fn on_read(&mut self, d: &mut Decision, event: Event, now: u64) {
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
        let hash = sha256_hex(&output);
        let path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let stored = Event::ToolCompleted {
            tool_use_id,
            tool,
            kind,
            input,
            output: format!("<blob:{hash}>"),
            failed,
        };
        let ev = self.push(d, now, vec![], Body::Event { event: stored });
        let output_kind = if kind == ToolKind::Read {
            OutputKind::View
        } else {
            OutputKind::Search
        };
        let id = format!("o-{}", self.next_output);
        self.next_output += 1;
        self.outputs.insert(id.clone(), output_kind);
        self.push(
            d,
            now,
            vec![ev],
            Body::Output {
                id,
                blob: hash.clone(),
                output_kind,
                lines: output.lines().count(),
                tokens: estimate_tokens(&output),
            },
        );
        if kind == ToolKind::Read && !failed && !path.is_empty() {
            self.session.reads_mut().on_read(&path, &hash);
        }
        d.blobs.push(Blob {
            hash,
            content: output,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::Config;
    use super::*;
    use crate::capability::Capabilities;
    use crate::record::ActionKind;
    use serde_json::json;

    fn read(path: &str, text: &str) -> Event {
        Event::ToolCompleted {
            tool_use_id: "r".into(),
            tool: "Read".into(),
            kind: ToolKind::Read,
            input: json!({ "file_path": path }),
            output: text.into(),
            failed: false,
        }
    }

    fn ctl() -> Controller {
        Controller::new(Config::default(), Capabilities::CLAUDE_CODE)
    }

    #[test]
    fn a_read_is_an_output_with_a_blob_and_is_never_shaped() {
        let mut c = ctl();
        // Large enough that a Bash test run of it would be shaped.
        let big = "FAILED tests/test_x.py::test_y - assert 1 == 2\n".repeat(2_000);
        let d = c.handle(read("src/a.py", &big), 0);
        assert!(d.response.is_empty(), "{:?}", d.response);
        assert_eq!(d.blobs.len(), 1);
        assert_eq!(d.blobs[0].content, big);
        let hash = sha256_hex(&big);
        let stored = d.entries.iter().find_map(|e| match &e.body {
            Body::Event {
                event: Event::ToolCompleted { output, .. },
            } => Some(output.clone()),
            _ => None,
        });
        assert_eq!(stored, Some(format!("<blob:{hash}>")));
        assert!(d.entries.iter().any(|e| matches!(
            &e.body,
            Body::Output { blob, output_kind: OutputKind::View, tokens, .. }
                if *blob == hash && *tokens > 0
        )));
        assert!(!d.entries.iter().any(|e| matches!(
            &e.body,
            Body::Action {
                action: ActionKind::Shape { .. } | ActionKind::Steer { .. },
                ..
            }
        )));
    }

    #[test]
    fn a_search_is_a_search_output() {
        let mut c = ctl();
        let ev = Event::ToolCompleted {
            tool_use_id: "g".into(),
            tool: "Grep".into(),
            kind: ToolKind::Search,
            input: json!({ "pattern": "fn main" }),
            output: "src/main.rs".into(),
            failed: false,
        };
        let d = c.handle(ev, 0);
        assert!(d.entries.iter().any(|e| matches!(
            &e.body,
            Body::Output {
                output_kind: OutputKind::Search,
                ..
            }
        )));
        assert_eq!(c.session().reads().total, 0, "a search is not a file read");
    }

    #[test]
    fn repeated_reads_count_until_a_compaction() {
        let mut c = ctl();
        c.handle(read("a.py", "x = 1\n"), 0);
        c.handle(read("a.py", "x = 1\n"), 1);
        c.handle(read("a.py", "x = 2\n"), 2);
        assert_eq!(c.session().reads().repeated, 1);
        c.handle(
            Event::Compacted {
                trigger: "auto".into(),
            },
            3,
        );
        c.handle(read("a.py", "x = 2\n"), 4);
        let r = c.session().reads();
        assert_eq!((r.total, r.repeated), (4, 1));
    }
}
