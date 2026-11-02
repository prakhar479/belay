//! Claude Code session transcripts (`~/.claude/projects/<project>/<session>.jsonl`),
//! read as the events the hooks would have produced. Transcript shape checked in S5.
//!
//! Limit: edit hashes are approximate (hash of path + new text), so edit-cycle
//! detection is weaker than in a live session.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use belay_core::text::sha256_hex;
use belay_core::Event;
use serde_json::Value;

use super::{extract_output, runner_is_executable, tool_kind, HOOKED_TOOLS, READ_TOOLS};
use crate::connector::{TranscriptFile, TranscriptSource, TranscriptStep};
use crate::report::{self, Usage};
use crate::usage::{MessageUsage, UsageLog};

pub struct ClaudeTranscripts;

impl TranscriptSource for ClaudeTranscripts {
    fn read(&self, path: &Path) -> Result<Vec<TranscriptStep>> {
        let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let mut steps = Vec::new();
        let mut pending: HashMap<String, (String, Value)> = HashMap::new();
        let mut seen = HashSet::new();
        let mut cwd = String::new();
        // Bytes, not `lines()`: one line that is not UTF-8 must not end the analysis.
        for line in BufReader::new(f).split(b'\n').map_while(Result::ok) {
            let Ok(v) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            let line_cwd = v["cwd"].as_str().map(String::from);
            if let Some(c) = &line_cwd {
                cwd.clone_from(c);
            }
            let mut step = TranscriptStep {
                at_ms: v["timestamp"].as_str().and_then(parse_rfc3339_ms),
                cwd: line_cwd,
                events: Vec::new(),
            };
            for item in v["message"]["content"].as_array().into_iter().flatten() {
                match item["type"].as_str() {
                    Some("tool_use") => tool_use(item, &cwd, &mut seen, &mut pending, &mut step),
                    Some("tool_result") => {
                        tool_result(item, &v["toolUseResult"], &mut pending, &mut step)
                    }
                    _ => {}
                }
            }
            steps.push(step);
        }
        Ok(steps)
    }

    fn usage(&self, path: &Path) -> Option<Usage> {
        report::usage_from_transcript(path)
    }

    fn usage_log(&self, path: &Path, from: u64) -> UsageLog {
        usage_log(path, from)
    }

    /// `<project>/<session>.jsonl` and `<project>/<session>/subagents/*.jsonl`. A
    /// subagent's tool calls appear only in its own transcript, so counting both counts
    /// nothing twice.
    fn list(&self, root: &Path) -> Vec<TranscriptFile> {
        let jsonl = |d: &Path| -> Vec<PathBuf> {
            fs::read_dir(d)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "jsonl"))
                .collect()
        };
        let mut paths = Vec::new();
        for project in fs::read_dir(root)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
        {
            let project = project.path();
            if !project.is_dir() {
                continue;
            }
            paths.extend(jsonl(&project));
            for session in fs::read_dir(&project)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
            {
                paths.extend(jsonl(&session.path().join("subagents")));
            }
        }
        paths.sort();
        paths
            .into_iter()
            .map(|path| TranscriptFile {
                subagent: is_subagent(&path),
                scratch: is_tmp_project(root, &path),
                path,
            })
            .collect()
    }
}

fn tool_use(
    item: &Value,
    cwd: &str,
    seen: &mut HashSet<String>,
    pending: &mut HashMap<String, (String, Value)>,
    step: &mut TranscriptStep,
) {
    let id = item["id"].as_str().unwrap_or("").to_string();
    // One API message can appear on several transcript lines with the same content
    // blocks: handle each tool call once.
    if !seen.insert(id.clone()) {
        return;
    }
    let name = item["name"].as_str().unwrap_or("").to_string();
    // Live hooks never see other tools (WebFetch, Task, …).
    if !HOOKED_TOOLS.contains(&name.as_str()) && !READ_TOOLS.contains(&name.as_str()) {
        return;
    }
    let input = item["input"].clone();
    if name == "Bash" {
        // What PreToolUse would have seen, assuming auto mode: lets the report say how
        // many test runs belay would wrap.
        let cmd = input["command"].as_str().unwrap_or("");
        step.events.push(Event::ToolRequested {
            tool_use_id: id.clone(),
            tool: name.clone(),
            kind: tool_kind(&name),
            input: input.clone(),
            rewrite_ok: runner_is_executable(cmd, cwd),
        });
    }
    if name == "Edit" || name == "MultiEdit" {
        step.events.push(Event::EditProposed {
            tool_use_id: id.clone(),
            path: input["file_path"].as_str().unwrap_or("").into(),
            old_text: input["old_string"].as_str().unwrap_or("").into(),
            new_text: input["new_string"].as_str().unwrap_or("").into(),
        });
    }
    pending.insert(id, (name, input));
}

/// `result`: the line's `toolUseResult`, the structured result a hook gets as
/// `tool_response`. For reads it gives the text the hook sees (a Read's file without
/// line numbers), so retro hashes reads as a live session does.
fn tool_result(
    item: &Value,
    result: &Value,
    pending: &mut HashMap<String, (String, Value)>,
    step: &mut TranscriptStep,
) {
    let id = item["tool_use_id"].as_str().unwrap_or("");
    let Some((tool, input)) = pending.remove(id) else {
        return;
    };
    let failed = item["is_error"].as_bool().unwrap_or(false);
    if !failed && matches!(tool.as_str(), "Edit" | "MultiEdit" | "Write") {
        let path = input["file_path"].as_str().unwrap_or("").to_string();
        let approx = format!(
            "{path}\n{}",
            input["new_string"]
                .as_str()
                .or(input["content"].as_str())
                .unwrap_or("")
        );
        step.events.push(Event::FileEdited {
            tool_use_id: id.to_string(),
            path,
            after_hash: Some(sha256_hex(&approx)),
        });
    }
    let output = if READ_TOOLS.contains(&tool.as_str()) && result.is_object() && !failed {
        extract_output(result)
    } else {
        extract_output(&item["content"])
    };
    step.events.push(Event::ToolCompleted {
        tool_use_id: id.to_string(),
        kind: tool_kind(&tool),
        tool,
        input,
        output,
        failed,
    });
}

/// Each API message's usage, and each limit hit, from byte `from` on. A last line with
/// no newline yet is left for the next read: `end` stops before it.
fn usage_log(path: &Path, from: u64) -> UsageLog {
    let mut log = UsageLog {
        end: from,
        ..UsageLog::default()
    };
    let Ok(mut f) = File::open(path) else {
        return log;
    };
    if f.seek(SeekFrom::Start(from)).is_err() {
        return log;
    }
    let mut reader = BufReader::new(f);
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(n) if n > 0 && line.last() == Some(&b'\n') => log.end += n as u64,
            _ => break,
        }
        if !may_hold_usage(&line) {
            continue;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if log.cwd.is_none() {
            log.cwd = v["cwd"].as_str().map(String::from);
        }
        let Some(at_ms) = v["timestamp"].as_str().and_then(parse_rfc3339_ms) else {
            continue;
        };
        if is_limit_hit(&v) {
            log.limit_hits.push(at_ms);
            continue;
        }
        let msg = &v["message"];
        let usage = &msg["usage"];
        let model = msg["model"].as_str().unwrap_or("");
        // Claude Code writes its own notices as `<synthetic>` messages: no API call.
        if !usage.is_object() || model == "<synthetic>" {
            continue;
        }
        let n = |k: &str| usage[k].as_u64().unwrap_or(0);
        let u = Usage {
            input: n("input_tokens"),
            cache_creation: n("cache_creation_input_tokens"),
            cache_read: n("cache_read_input_tokens"),
            output: n("output_tokens"),
            messages: 1,
        };
        let id = match msg["id"].as_str().or(v["uuid"].as_str()) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => format!("{}#{}", path.display(), log.end),
        };
        // One API message spans several lines; the last line's usage wins (S5).
        match index.get(&id) {
            Some(&i) => log.messages[i].usage = u,
            None => {
                index.insert(id.clone(), log.messages.len());
                log.messages.push(MessageUsage {
                    at_ms,
                    id,
                    model: model.to_string(),
                    usage: u,
                });
            }
        }
    }
    log
}

/// Most bytes are users' lines (tool results): skip them unparsed, from the line's
/// head, where Claude Code writes `type`. The first `belay status` scans every
/// transcript, so this keeps it to about a second for 1 GB.
fn may_hold_usage(line: &[u8]) -> bool {
    let head = &line[..line.len().min(400)];
    let has = |needle: &[u8]| head.windows(needle.len()).any(|w| w == needle);
    !has(b"\"type\"") || has(b"\"assistant\"") || has(b"\"result\"")
}

/// A plan limit the user hit: Claude Code's own error line, not text that quotes it
/// (a prompt or a tool result can). `result`: the headless `-p` stream's last line
/// (fixtures/fleet/result_five_hour_limit.json). In a transcript (checked in V2 on real
/// ones): an assistant line from `<synthetic>` with `isApiErrorMessage`, text "You've
/// hit your 5-hour limit · resets …" or "… session limit …" (the same window). A
/// weekly limit is not the 5-hour one.
fn is_limit_hit(v: &Value) -> bool {
    let text = match v["type"].as_str() {
        Some("result") if v["is_error"].as_bool() == Some(true) => {
            v["result"].as_str().unwrap_or("").to_string()
        }
        Some("assistant")
            if v["isApiErrorMessage"].as_bool() == Some(true)
                || v["message"]["model"].as_str() == Some("<synthetic>") =>
        {
            v["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join(" ")
        }
        _ => return false,
    };
    let text = text.trim_start();
    let hit = text.starts_with("You've hit your") || text.contains("usage limit reached");
    hit && !text.to_lowercase().contains("week")
}

/// A project dir for a working directory under `/tmp`: scratch work, such as belay's
/// own conformance runs, rather than real sessions (checkpoint C3).
fn is_tmp_project(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .ok()
        .and_then(|p| p.components().next())
        .is_some_and(|c| c.as_os_str().to_string_lossy().starts_with("-tmp-"))
}

fn is_subagent(path: &Path) -> bool {
    path.parent()
        .and_then(|p| p.file_name())
        .is_some_and(|n| n == "subagents")
}

/// Parse `2026-10-19T08:35:44.123Z` (UTC, optional fraction) to unix milliseconds.
fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    let (date, time) = s.strip_suffix('Z')?.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, "0"));
    let mut t = hms.split(':').map(|x| x.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    let ok = (0..=9999).contains(&y)
        && (1..=12).contains(&m)
        && (1..=31).contains(&day)
        && (0..24).contains(&hh)
        && (0..60).contains(&mm)
        && (0..61).contains(&ss)
        && !frac.is_empty()
        && frac.bytes().all(|b| b.is_ascii_digit());
    if !ok {
        return None;
    }
    let ms: i64 = format!("{frac:0<3}")[..3].parse().ok()?;
    // Days from civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let total = ((days * 24 + hh) * 60 + mm) * 60 + ss;
    u64::try_from(total * 1000 + ms).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bad timestamp in a transcript is skipped, never a panic.
    #[test]
    fn odd_timestamps_do_not_panic() {
        for s in [
            "2026-10-19T08:35:44.éé9Z",
            "2026-10-19T08:35:44.-1Z",
            "99999999999999999-01-01T00:00:00Z",
            "2026-13-45T99:99:99Z",
        ] {
            assert_eq!(parse_rfc3339_ms(s), None, "{s}");
        }
    }

    #[test]
    fn parses_transcript_timestamps() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2026-10-19T08:35:44.123Z"),
            Some(1_792_398_944_123)
        );
        assert_eq!(
            parse_rfc3339_ms("2024-02-29T23:59:59.5Z"),
            Some(1_709_251_199_500)
        );
        assert_eq!(parse_rfc3339_ms("not a time"), None);
    }

    /// Retro sees reads as live hooks do: the file's text from `toolUseResult`, not
    /// the line-numbered text the model got; tools no hook sees stay out.
    #[test]
    fn reads_and_searches_are_read() {
        let fixture = format!(
            "{}/../../fixtures/transcripts/reads_session.jsonl",
            env!("CARGO_MANIFEST_DIR")
        );
        let done: Vec<(String, belay_core::ToolKind, String)> = ClaudeTranscripts
            .read(Path::new(&fixture))
            .unwrap()
            .into_iter()
            .flat_map(|s| s.events)
            .filter_map(|e| match e {
                Event::ToolCompleted {
                    tool, kind, output, ..
                } => Some((tool, kind, output)),
                _ => None,
            })
            .collect();
        use belay_core::ToolKind::{Read, Search};
        let file = "def total(xs):\n    return sum(xs)\n";
        assert_eq!(
            done,
            vec![
                ("Read".into(), Read, file.into()),
                ("Read".into(), Read, file.into()),
                ("Grep".into(), Search, "shop/__init__.py".into()),
                (
                    "Glob".into(),
                    Search,
                    "/home/dev/shop/tests/test_api.py".into()
                ),
            ]
        );
    }

    /// Claude Code repeats a message's content blocks on several lines.
    #[test]
    fn repeated_blocks_map_once() {
        let fixture = format!(
            "{}/../../fixtures/transcripts/loop_session.jsonl",
            env!("CARGO_MANIFEST_DIR")
        );
        let text = fs::read_to_string(&fixture).unwrap();
        let doubled: String = text.lines().flat_map(|l| [l, "\n", l, "\n"]).collect();
        let path =
            std::env::temp_dir().join(format!("belay-dup-blocks-{}.jsonl", std::process::id()));
        fs::write(&path, doubled).unwrap();
        let count = |p: &Path| -> usize {
            ClaudeTranscripts
                .read(p)
                .unwrap()
                .iter()
                .map(|s| s.events.len())
                .sum()
        };
        let (a, b) = (count(Path::new(&fixture)), count(&path));
        let _ = fs::remove_file(&path);
        assert!(a > 0);
        assert_eq!(a, b);
    }
}
