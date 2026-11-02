//! Claude Code connector.
//!
//! Payload and output shapes were captured in Phase 0 (docs/dev/TASKS.md S1–S3) with
//! Claude Code 2.1.303. Re-capture from <session>/hooks.jsonl when Claude Code changes.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use belay_core::text::sha256_hex;
use belay_core::{Capabilities, Event, Permission, Response, ToolKind};
use serde_json::{json, Value};

use super::{Connector, ResumeTarget, TranscriptSource};
use crate::paths::now_ms;

mod transcript;

const EDIT_TOOLS: [&str; 3] = ["Edit", "MultiEdit", "Write"];

/// Claude Code's tool names in the core's words.
pub fn tool_kind(tool: &str) -> ToolKind {
    match tool {
        "Bash" => ToolKind::Shell,
        "Edit" | "MultiEdit" => ToolKind::Edit,
        "Write" => ToolKind::Write,
        "Read" => ToolKind::Read,
        "Grep" | "Glob" => ToolKind::Search,
        "WebFetch" | "WebSearch" => ToolKind::Fetch,
        "Task" | "Agent" => ToolKind::Subagent,
        t if t.starts_with("mcp__") => ToolKind::Mcp,
        _ => ToolKind::Other,
    }
}

/// Claude Code's directory name for a project: the working directory with every
/// character that is not a letter or digit replaced by `-`.
/// Checked live by conformance (`project_dir_name`, Claude Code 2.1.316).
pub fn project_dir_name(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Tools belay hooks before and after they run (see `settings`). Retro looks at the
/// same tools, so its numbers match a live session's.
pub const HOOKED_TOOLS: [&str; 4] = ["Bash", "Edit", "MultiEdit", "Write"];

/// Tools belay sees only after they run (U5.1): reads are recorded, never changed.
pub const READ_TOOLS: [&str; 3] = ["Read", "Grep", "Glob"];

#[derive(Default)]
pub struct ClaudeCode {
    started: bool,
    /// The repo's `.belay.toml` (E7).
    policy: belay_core::policy::Policy,
    /// Claude Code's id of the current conversation.
    session_id: String,
    /// Test files a Bash call may write, as they were before it ran (R1.4), and the
    /// content hashes of the other files it names. Keyed by tool use id, with the time
    /// taken (ms).
    snapshots: BTreeMap<String, (u64, Snapshot, Hashes)>,
    /// The daemon's socket, for a wrapped run whose shell does not know it (plugin).
    socket: Option<PathBuf>,
}

/// Files as a Bash call found them before it ran: path and text (None: no such file).
type Snapshot = Vec<(PathBuf, Option<String>)>;

/// Other files a Bash call names, by content hash (None: no such file). One that
/// changed is an edit like an Edit tool's, for the card, peers, and edit cycles.
type Hashes = Vec<(PathBuf, Option<String>)>;

fn hash_of(path: &Path) -> Result<Option<String>, ()> {
    read_text_up_to(path, MAX_HASH_BYTES).map(|t| t.map(|t| sha256_hex(&t)))
}

/// Largest file the Bash-write guard snapshots. Test files are far smaller.
const MAX_SNAPSHOT_BYTES: u64 = 1 << 20;
/// At the end of a turn, snapshots older than this are dropped.
const SNAPSHOT_KEEP_MS: u64 = 10 * 60 * 1000;
/// Most files one Bash call snapshots.
const MAX_SNAPSHOT_FILES: usize = 32;
/// Bash calls whose snapshots wait for a result. A call that never reports (interrupted)
/// is dropped at the end of the turn; this bounds the rest.
const MAX_PENDING_SNAPSHOTS: usize = 64;

/// Largest file whose content hash PostToolUse records for edit cycles.
const MAX_HASH_BYTES: u64 = 16 << 20;

/// A file's text: None if it does not exist. Err if it cannot be compared (a directory,
/// too large, not UTF-8), so the guard skips it.
fn read_text(path: &Path) -> Result<Option<String>, ()> {
    read_text_up_to(path, MAX_SNAPSHOT_BYTES)
}

fn read_text_up_to(path: &Path, max: u64) -> Result<Option<String>, ()> {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Ok(m) if m.is_file() && m.len() <= max => {
            std::fs::read_to_string(path).map(Some).map_err(|_| ())
        }
        _ => Err(()),
    }
}

/// The files a target names: itself, or the matches of a `*` in its last component.
fn expand(base: &Path, target: &str) -> Vec<PathBuf> {
    let path = base.join(target);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let Some((prefix, suffix)) = name.split_once('*') else {
        return vec![path];
    };
    if suffix.contains('*') {
        return vec![];
    }
    let dir = path.parent().unwrap_or(base);
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            n.len() >= prefix.len() + suffix.len() && n.starts_with(prefix) && n.ends_with(suffix)
        })
        .collect();
    found.sort();
    found
}

/// Snapshot the test files a Bash command may write (R1.4). Relative paths start where
/// the command runs: the hook's cwd, then any leading `cd DIR &&`.
fn snapshot(command: &str, cwd: &str, policy: &belay_core::policy::Policy) -> (Snapshot, Hashes) {
    let protected = |p: &str| {
        policy
            .protected_by(&resolve(cwd, p).to_string_lossy())
            .is_some()
    };
    let mut targets = belay_core::perception::code_write_targets(command);
    targets.extend(belay_core::perception::write_targets_where(
        command, protected,
    ));
    // Every word of a writing command that may be a file: those that are not files
    // (a sed script, a flag) do not exist before or after, so they never show up.
    let named = belay_core::perception::write_targets_where(command, |_| true);
    if targets.is_empty() && named.is_empty() {
        return (vec![], vec![]);
    }
    let (_, dirs) = belay_core::perception::shell::leading_cd(command);
    let base = dirs.iter().fold(resolve(cwd, ""), |b, d| b.join(d));
    let mut files: Vec<(PathBuf, Option<String>)> = Vec::new();
    for path in targets.iter().flat_map(|t| expand(&base, t)) {
        if files.len() >= MAX_SNAPSHOT_FILES || files.iter().any(|(p, _)| *p == path) {
            continue;
        }
        let Ok(text) = read_text(&path) else {
            continue;
        };
        // A source file counts only when it holds tests (Rust unit tests in `src/`).
        let shown = path.to_string_lossy();
        let is_test = belay_core::guard::is_test_path(&shown)
            || text
                .as_deref()
                .is_some_and(belay_core::guard::has_test_code)
            || policy.is_test(&shown)
            || policy.protected_by(&shown).is_some();
        if is_test {
            files.push((path, text));
        }
    }
    let mut hashes: Hashes = Vec::new();
    for path in named.iter().flat_map(|t| expand(&base, t)) {
        if hashes.len() >= MAX_SNAPSHOT_FILES
            || files.iter().any(|(p, _)| *p == path)
            || hashes.iter().any(|(p, _)| *p == path)
        {
            continue;
        }
        if let Ok(h) = hash_of(&path) {
            hashes.push((path, h));
        }
    }
    (files, hashes)
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn resolve(cwd: &str, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() || cwd.is_empty() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    }
}

/// Bytes of stdout the agent sees when Claude Code persists a large output to a file.
const PERSISTED_PREVIEW_BYTES: usize = 2000;

/// The `<persisted-output>` message the agent sees in place of a large Bash output.
/// Captured in S1: the preview is the first 2000 bytes of stdout, cut back to the last
/// newline (no newline: exactly 2000 bytes). The size uses base-1024 KB, or MB from 1 MiB:
/// 1988895 bytes shows as "1.9MB", 1048576 bytes as "1MB".
fn persisted_output(stdout: &str, path: &str, size: u64) -> String {
    let mut end = stdout.len().min(PERSISTED_PREVIEW_BYTES);
    while !stdout.is_char_boundary(end) {
        end -= 1;
    }
    let cut = &stdout[..end];
    let preview = if end < stdout.len() {
        cut.rfind('\n').map_or(cut, |i| &cut[..i])
    } else {
        cut
    };
    // One decimal, without a trailing ".0": 106.3KB, 1.9MB, 1MB (N14).
    let one = |v: f64| {
        let t = format!("{v:.1}");
        t.strip_suffix(".0").map(str::to_string).unwrap_or(t)
    };
    let kb = size as f64 / 1024.0;
    let size = if kb >= 1024.0 {
        format!("{}MB", one(kb / 1024.0))
    } else {
        format!("{}KB", one(kb))
    };
    format!(
        "<persisted-output>\nOutput too large ({size}). Full output saved to: {path}\n\nPreview (first 2KB):\n{preview}\n...\n</persisted-output>"
    )
}

/// Text the agent sees for a tool result.
///
/// Captured in S1 (Claude Code 2.1.303). Bash PostToolUse `tool_response` is
/// `{stdout, stderr, interrupted, isImage, noOutputExpected}`. stderr is merged into
/// stdout, and `stderr` was always empty. There is no exit-code field. Outputs over
/// 30000 chars are cut and persisted, with `persistedOutputPath` and `persistedOutputSize`.
/// PostToolUseFailure `error` is a string: `"Exit code N\n"` plus the output.
pub fn extract_output(v: &Value) -> String {
    match v {
        Value::String(t) => t.clone(),
        Value::Object(o) if o.get("persistedOutputPath").is_some_and(Value::is_string) => {
            persisted_output(
                o.get("stdout").and_then(Value::as_str).unwrap_or(""),
                o.get("persistedOutputPath")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                o.get("persistedOutputSize")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            )
        }
        Value::Object(o) if o.contains_key("stdout") || o.contains_key("stderr") => {
            let out = o.get("stdout").and_then(Value::as_str).unwrap_or("");
            let err = o.get("stderr").and_then(Value::as_str).unwrap_or("");
            if err.is_empty() {
                out.to_string()
            } else if out.is_empty() {
                err.to_string()
            } else {
                format!("{out}\n{err}")
            }
        }
        Value::Object(o) if o.contains_key("content") => extract_output(&o["content"]),
        // Read: `{type, file: {filePath, content, numLines, startLine, totalLines}}`, seen
        // in transcripts' `toolUseResult` (2.1.316). An image or notebook has no `content`.
        // PostToolUse Read's `tool_response` has the same shape (conformance, 2.1.316).
        Value::Object(o) if o.get("file").is_some_and(Value::is_object) => o["file"]
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        // Glob, and Grep's `files_with_matches` mode: `{filenames: [...], numFiles, …}`.
        // [Verify] from the hook docs; no capture yet (Grep's other modes carry `content`).
        Value::Object(o) if o.get("filenames").is_some_and(Value::is_array) => o["filenames"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| i.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Every program in a test command is a bash builtin or a real executable. `belay run`
/// uses plain bash, so a name that exists only as a function or alias in the agent's
/// shell snapshot (`conda activate`, a custom `t`) would fail there (R0.4). A
/// `source X/bin/activate` adds X/bin to the PATH searched for later programs (R1.1).
pub fn runner_is_executable(command: &str, cwd: &str) -> bool {
    use belay_core::perception::shell;
    use std::os::unix::fs::PermissionsExt;
    if belay_core::perception::runner_segment(command).is_none() {
        return true;
    }
    const BUILTINS: [&str; 20] = [
        "source", ".", "export", "set", "unset", "echo", "printf", "true", "false", "test", "[",
        ":", "exit", "exec", "command", "type", "shopt", "ulimit", "umask", "wait",
    ];
    let executable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let mut path: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    // Relative names resolve against the directory the leading `cd`s end in.
    let (prefix, dirs) = shell::leading_cd(command);
    let cwd = dirs
        .iter()
        .fold(PathBuf::from(cwd), |at, d| at.join(d))
        .display()
        .to_string();
    let cwd = cwd.as_str();
    for seg in shell::parse(&command[prefix..]).segments {
        let Some(program) = seg.iter().find(|w| !shell::is_assignment(w)) else {
            continue;
        };
        if matches!(program.as_str(), "source" | ".") {
            if let Some(script) = seg.iter().skip_while(|w| *w != program).nth(1) {
                if let Some(bin) = resolve(cwd, script).parent() {
                    path.insert(0, bin.to_path_buf());
                }
            }
            continue;
        }
        if BUILTINS.contains(&program.as_str()) {
            continue;
        }
        let found = if program.contains('/') {
            executable(&resolve(cwd, program))
        } else {
            path.iter().any(|d| executable(&d.join(program)))
        };
        if !found {
            return false;
        }
    }
    true
}

/// (path, old text, new text) for an edit tool call.
fn edit_texts(tool: &str, input: &Value, cwd: &str) -> Option<(String, String, String)> {
    let path = s(input, "file_path");
    if path.is_empty() {
        return None;
    }
    let (old, new) = match tool {
        "Edit" => (
            s(input, "old_string").to_string(),
            s(input, "new_string").to_string(),
        ),
        "MultiEdit" => {
            let edits = input.get("edits").and_then(Value::as_array)?;
            let join = |k: &str| edits.iter().map(|e| s(e, k)).collect::<Vec<_>>().join("\n");
            (join("old_string"), join("new_string"))
        }
        // The old text matters only to the guard: read it for files that can hold tests,
        // up to the snapshot size, never a 200 MB data file under the daemon lock (I4, F4).
        "Write" => {
            let old = if belay_core::guard::may_hold_tests(path) {
                read_text(&resolve(cwd, path))
                    .ok()
                    .flatten()
                    .unwrap_or_default()
            } else {
                String::new()
            };
            (old, s(input, "content").to_string())
        }
        _ => return None,
    };
    Some((path.to_string(), old, new))
}

/// Mirror the original `tool_response` shape when replacing output.
/// S2: for Bash, the object form reaches the agent; a plain string is silently ignored.
fn encode_replacement(original: Option<&Value>, text: &str) -> Value {
    match original {
        Some(Value::Object(o)) if o.contains_key("stdout") => {
            let mut o = o.clone();
            o.insert("stdout".into(), Value::String(text.to_string()));
            o.insert("stderr".into(), Value::String(String::new()));
            Value::Object(o)
        }
        _ => Value::String(text.to_string()),
    }
}

impl Connector for ClaudeCode {
    fn agent(&self) -> &'static str {
        "claude-code"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::CLAUDE_CODE
    }

    fn set_socket(&mut self, socket: &std::path::Path) {
        self.socket = Some(socket.to_path_buf());
    }

    /// `~/.claude/skills` (under `CLAUDE_CONFIG_DIR` if set) and `<repo>/.claude/skills`.
    /// [Verify] whether a skill added during a running session is picked up (U9).
    fn skill_dir(&self, scope: belay_core::skills::Scope, repo: Option<&Path>) -> Option<PathBuf> {
        match scope {
            belay_core::skills::Scope::User => Some(crate::install::claude_dir().join("skills")),
            belay_core::skills::Scope::Project => repo.map(|r| r.join(".claude/skills")),
        }
    }

    fn sandbox_files(&self) -> super::SandboxFiles {
        super::SandboxFiles {
            copy: [
                ".claude.json",
                ".claude/.credentials.json",
                ".claude/settings.json",
            ]
            .into_iter()
            .map(std::path::PathBuf::from)
            .collect(),
            ro_dirs: vec![".claude/plugins".into()],
            env: vec![
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "ANTHROPIC_BASE_URL",
                "ANTHROPIC_MODEL",
                "CLAUDE_CODE_OAUTH_TOKEN",
                "CLAUDE_CODE_USE_BEDROCK",
                "CLAUDE_CODE_USE_VERTEX",
            ],
        }
    }

    fn transcript(&self) -> Option<Box<dyn TranscriptSource>> {
        Some(Box::new(transcript::ClaudeTranscripts))
    }

    /// `claude -p` with stream-json output, so belay counts tokens as they go. The
    /// permission mode is `bypassPermissions`: the sandbox is the boundary, and
    /// belay's hooks still run. [Verify] conformance: a PreToolUse deny holds under
    /// `bypassPermissions`. `--resume <id> --fork-session`: a new conversation that
    /// starts from the old one's context. [Verify] `claude --help` (2.1.316) says
    /// `--fork-session` makes a new session id instead of reusing the original; belay
    /// also gives the sandbox only a copy of the transcript (`conversation_files`), so
    /// the original is never written either way.
    fn headless_argv(
        &self,
        prompt: &str,
        resume: Option<&str>,
        model: Option<&str>,
    ) -> anyhow::Result<Vec<String>> {
        let mut a: Vec<String> = [
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--permission-mode",
            "bypassPermissions",
        ]
        .map(String::from)
        .to_vec();
        if let Some(m) = model {
            a.extend(["--model".into(), m.into()]);
        }
        if let Some(c) = resume {
            a.extend(["--resume".into(), c.into(), "--fork-session".into()]);
        }
        // After `--`, a prompt that starts with `-` is not read as a flag.
        a.extend(["--".into(), prompt.into()]);
        Ok(a)
    }

    /// stream-json: each `assistant` line carries its API message's usage (repeated on
    /// every line of that message: the id dedupes it); the `result` line ends the run.
    fn headless_line(&self, line: &str) -> Option<super::HeadlessLine> {
        let v: Value = serde_json::from_str(line).ok()?;
        match v.get("type")?.as_str()? {
            "assistant" => {
                let m = v.get("message")?;
                let u = m.get("usage")?;
                let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                Some(super::HeadlessLine {
                    call: m.get("id").and_then(Value::as_str).map(String::from),
                    tokens: n("input_tokens")
                        + n("output_tokens")
                        + n("cache_creation_input_tokens"),
                    result: None,
                })
            }
            "result" => Some(super::HeadlessLine {
                result: Some(s(&v, "result").to_string()),
                ..Default::default()
            }),
            _ => None,
        }
    }

    /// The transcript `<id>.jsonl` from whichever project dir holds it, copied to the
    /// project dir of `cwd` in the sandbox's home: `--resume` looks there.
    fn conversation_files(&self, conversation: &str, cwd: &Path) -> Vec<(PathBuf, PathBuf)> {
        let name = format!("{conversation}.jsonl");
        if conversation.contains(['/', '.']) {
            return Vec::new();
        }
        let Ok(rd) = fs::read_dir(crate::paths::claude_projects_dir()) else {
            return Vec::new();
        };
        let mut found: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path().join(&name))
            .filter(|p| p.is_file())
            .collect();
        found.sort();
        found
            .into_iter()
            .next()
            .map(|src| {
                let dst = Path::new(".claude/projects")
                    .join(project_dir_name(cwd))
                    .join(&name);
                vec![(src, dst)]
            })
            .unwrap_or_default()
    }

    /// `--resume <id>`, `-r <id>`, `--resume=<id>`: that conversation. `--continue`, `-c`:
    /// the latest. A bare `--resume` or `-r` opens Claude Code's picker. Arguments after
    /// `--` are the prompt's, not flags.
    fn resume_target(&self, args: &[String], _cwd: &Path) -> Option<ResumeTarget> {
        let mut it = args.iter().take_while(|a| *a != "--").peekable();
        while let Some(a) = it.next() {
            if let Some(id) = a.strip_prefix("--resume=") {
                return Some(match id {
                    "" => ResumeTarget::Picker,
                    id => ResumeTarget::Conversation(id.to_string()),
                });
            }
            match a.as_str() {
                "--resume" | "-r" => {
                    return Some(match it.peek() {
                        Some(id) if !id.starts_with('-') => {
                            ResumeTarget::Conversation(id.to_string())
                        }
                        _ => ResumeTarget::Picker,
                    })
                }
                "--continue" | "-c" => return Some(ResumeTarget::Latest),
                _ => {}
            }
        }
        None
    }

    /// The newest main transcript in Claude Code's project dir for `cwd`; its file name
    /// is the conversation id.
    fn latest_conversation(&self, cwd: &Path) -> Option<String> {
        let project = crate::paths::claude_projects_dir().join(project_dir_name(cwd));
        transcript::ClaudeTranscripts
            .list(&crate::paths::claude_projects_dir())
            .into_iter()
            .filter(|t| !t.subagent && t.path.parent() == Some(project.as_path()))
            .filter_map(|t| Some((fs::metadata(&t.path).ok()?.modified().ok()?, t.path)))
            .max()
            .and_then(|(_, p)| Some(p.file_stem()?.to_string_lossy().to_string()))
    }

    fn conversation(&self) -> Option<String> {
        (!self.session_id.is_empty()).then(|| self.session_id.clone())
    }

    fn set_policy(&mut self, policy: &belay_core::policy::Policy) {
        self.policy = policy.clone();
    }

    fn map(&mut self, hook_event: &str, p: &Value) -> Vec<Event> {
        let mut evs = Vec::new();
        let cwd = s(p, "cwd").to_string();
        let transcript_path = p
            .get("transcript_path")
            .and_then(Value::as_str)
            .map(String::from);
        let session_id = s(p, "session_id");
        if !self.started {
            self.started = true;
            evs.push(Event::SessionStarted {
                agent: self.agent().into(),
                cwd: cwd.clone(),
                transcript_path,
                mode: None,
            });
        } else if hook_event == "SessionStart"
            && session_id == self.session_id
            && s(p, "source") == "compact"
        {
            // The same conversation on a fresh context after a compaction.
            // [Verify] live: SessionStart `source: "compact"` keeps the session id (U4b;
            // fixtures/claude_code/session_start_compact.json is from the hook docs).
            evs.push(Event::ContextRestarted {
                source: s(p, "source").to_string(),
            });
        } else if !session_id.is_empty()
            && !self.session_id.is_empty()
            && session_id != self.session_id
        {
            // `/clear` or `/resume` in the same process: a new conversation (I6, R3).
            let reason = match (hook_event, s(p, "source")) {
                ("SessionStart", source) if !source.is_empty() => source.to_string(),
                _ => "new session id".to_string(),
            };
            self.snapshots.clear();
            evs.push(Event::ConversationChanged {
                transcript_path,
                reason,
            });
        }
        if !session_id.is_empty() {
            self.session_id = session_id.to_string();
        }
        let tool = s(p, "tool_name").to_string();
        let tool_use_id = s(p, "tool_use_id").to_string();
        let input = p.get("tool_input").cloned().unwrap_or(Value::Null);

        match hook_event {
            "PreToolUse" => {
                if EDIT_TOOLS.contains(&tool.as_str()) {
                    if let Some((path, old_text, new_text)) = edit_texts(&tool, &input, &cwd) {
                        evs.push(Event::EditProposed {
                            tool_use_id,
                            path,
                            old_text,
                            new_text,
                        });
                    }
                } else {
                    // S2/D4: Claude Code checks permissions on the rewritten command, so a
                    // rewrite is safe only where no prompt can appear. Background calls keep
                    // their own output handling.
                    let mode = s(p, "permission_mode");
                    let background = input
                        .get("run_in_background")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let command = s(&input, "command");
                    if tool == "Bash" && self.snapshots.len() < MAX_PENDING_SNAPSHOTS {
                        let (files, hashes) = snapshot(command, &cwd, &self.policy);
                        if !files.is_empty() || !hashes.is_empty() {
                            self.snapshots
                                .insert(tool_use_id.clone(), (now_ms(), files, hashes));
                        }
                    }
                    let rewrite_ok = matches!(mode, "auto" | "bypassPermissions")
                        && !background
                        && runner_is_executable(command, &cwd);
                    evs.push(Event::ToolRequested {
                        tool_use_id,
                        kind: tool_kind(&tool),
                        tool,
                        input,
                        rewrite_ok,
                    });
                }
            }
            "PostToolUse" | "PostToolUseFailure" => {
                let failed = hook_event == "PostToolUseFailure";
                let output = match p.get("error") {
                    Some(e) if failed => extract_output(e),
                    _ => extract_output(p.get("tool_response").unwrap_or(&Value::Null)),
                };
                if !failed && EDIT_TOOLS.contains(&tool.as_str()) {
                    let path = s(&input, "file_path").to_string();
                    if !path.is_empty() {
                        // Edit cycles compare content hashes; a huge file is skipped.
                        let after_hash = read_text_up_to(&resolve(&cwd, &path), MAX_HASH_BYTES)
                            .ok()
                            .flatten()
                            .map(|c| sha256_hex(&c));
                        evs.push(Event::FileEdited {
                            tool_use_id: tool_use_id.clone(),
                            path,
                            after_hash,
                        });
                    }
                }
                let (files, hashes) = self
                    .snapshots
                    .remove(&tool_use_id)
                    .map(|(_, f, h)| (f, h))
                    .unwrap_or_default();
                for (path, before) in hashes {
                    let Ok(after) = hash_of(&path) else {
                        continue;
                    };
                    if after != before {
                        evs.push(Event::FileEdited {
                            tool_use_id: tool_use_id.clone(),
                            path: path.display().to_string(),
                            after_hash: after,
                        });
                    }
                }
                for (path, before) in files {
                    let Ok(after) = read_text(&path) else {
                        continue;
                    };
                    if after != before {
                        evs.push(Event::FileChanged {
                            tool_use_id: tool_use_id.clone(),
                            path: path.display().to_string(),
                            old_text: before.unwrap_or_default(),
                            new_text: after.unwrap_or_default(),
                        });
                    }
                }
                evs.push(Event::ToolCompleted {
                    tool_use_id,
                    kind: tool_kind(&tool),
                    tool,
                    input,
                    output,
                    failed,
                });
            }
            "PermissionDenied" => evs.push(Event::ToolDenied {
                tool_use_id,
                tool,
                reason: s(p, "reason").to_string(),
            }),
            "UserPromptSubmit" => evs.push(Event::PromptSubmitted {
                chars: s(p, "prompt").chars().count(),
            }),
            "Stop" => {
                // A call with no result by now was interrupted or denied, unless it still
                // runs in the background (a subagent, `run_in_background`): keep recent
                // ones, drop the rest (I6, R4).
                let cutoff = now_ms().saturating_sub(SNAPSHOT_KEEP_MS);
                self.snapshots.retain(|_, (t, ..)| *t >= cutoff);
                evs.push(Event::TurnEnded);
            }
            "PreCompact" => evs.push(Event::Compacted {
                trigger: s(p, "trigger").to_string(),
            }),
            // The payload's shape (`message`, `notification_type`) is read from Claude
            // Code 2.1.316's hook input schema; no live capture yet. [Verify] conformance.
            "Notification" => evs.push(Event::AgentWaiting {
                reason: s(p, "notification_type").to_string(),
                text: s(p, "message").to_string(),
            }),
            // [Verify] hook docs: SubagentStop carries `agent_type` (no capture yet).
            "SubagentStop" => evs.push(Event::SubagentEnded {
                agent_type: p
                    .get("agent_type")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                    .map(String::from),
            }),
            _ => {}
        }
        evs
    }

    fn encode(&self, hook_event: &str, payload: &Value, r: &Response) -> Option<Value> {
        let notice = user_notice(&r.notices).filter(|_| self.capabilities().notify_user);
        let mut out = self.encode_for_agent(hook_event, payload, r);
        if let Some(text) = notice {
            // Rule 5: top-level `systemMessage` is shown to the user, not sent to the model.
            // [Verify] it shows on every hook event (scripts/conformance.py: notice).
            let o = out.get_or_insert_with(|| json!({}));
            o["systemMessage"] = json!(text);
        }
        out
    }
}

/// The one line the user sees for a hook's notices: the most important one, with the
/// others counted (the core makes one per hook; a hook can carry two decisions).
fn user_notice(notices: &[belay_core::Notice]) -> Option<String> {
    let warn = notices
        .iter()
        .position(|n| n.level == belay_core::NoticeLevel::Warn);
    let first = notices.get(warn.unwrap_or(0))?;
    let mut text = format!("belay: {}", first.text);
    if notices.len() > 1 {
        text.push_str(&format!(" (+{} more)", notices.len() - 1));
    }
    Some(text)
}

impl ClaudeCode {
    /// The part of the reply the agent acts on. Notices never go here (rule 5).
    fn encode_for_agent(&self, hook_event: &str, payload: &Value, r: &Response) -> Option<Value> {
        // Verify before done (U6.1): a Stop block keeps the agent working, with the
        // reason as its next instruction. Checked live (conformance, 2.1.316): the agent
        // went on and ran the command, and the next Stop came with `stop_hook_active`.
        if hook_event == "Stop" {
            return r
                .continue_turn
                .as_ref()
                .map(|reason| json!({ "decision": "block", "reason": reason }));
        }
        let mut context = Vec::new();
        if hook_event == "SessionStart" {
            context.extend(r.start_context.iter().cloned());
        }
        context.extend(r.add_context.iter().cloned());
        if r.replace_output.is_none()
            && !r.wrap
            && context.is_empty()
            && r.permission.is_none()
            && !r.retry
        {
            return None;
        }
        let context = (!context.is_empty()).then(|| context.join("\n"));
        let mut h = json!({ "hookEventName": hook_event });
        match hook_event {
            "PreToolUse" => {
                if r.wrap {
                    let mut input = payload
                        .get("tool_input")
                        .filter(|i| i.is_object())
                        .cloned()
                        .unwrap_or(json!({}));
                    let command = s(&input, "command").to_string();
                    // The daemon is the belay binary; its absolute path does not depend on
                    // the PATH in the agent's shell snapshot (R0.4).
                    let exe = std::env::current_exe().unwrap_or_else(|_| "belay".into());
                    // A leading `cd DIR &&` runs in the agent's shell, so it persists.
                    let (prefix, _) = belay_core::perception::shell::leading_cd(&command);
                    input["command"] = json!(format!(
                        "{}{}",
                        &command[..prefix],
                        crate::run::wrapped_command(
                            &exe,
                            s(payload, "tool_use_id"),
                            self.socket.as_deref(),
                            &command[prefix..]
                        )
                    ));
                    // No permissionDecision: Claude Code still checks the new command.
                    h["updatedInput"] = input;
                }
                if let Some(p) = &r.permission {
                    let (decision, reason) = match p {
                        Permission::Ask(m) => ("ask", m),
                        Permission::Deny(m) => ("deny", m),
                    };
                    h["permissionDecision"] = json!(decision);
                    h["permissionDecisionReason"] = json!(reason);
                }
            }
            "PostToolUse" => {
                if let Some(t) = &r.replace_output {
                    h["updatedToolOutput"] = encode_replacement(payload.get("tool_response"), t);
                }
            }
            // R3.7: the only output this event takes (2.1.303 hook docs).
            "PermissionDenied" => {
                if !r.retry {
                    return None;
                }
                return Some(json!({ "hookSpecificOutput": {
                    "hookEventName": hook_event,
                    "retry": true,
                }}));
            }
            "PostToolUseFailure" | "UserPromptSubmit" | "SessionStart" => {}
            _ => return None,
        }
        if let Some(c) = context {
            h["additionalContext"] = json!(c);
        }
        Some(json!({ "hookSpecificOutput": h }))
    }
}

/// Settings file the launcher passes with `claude --settings <file>`.
/// S3: these hooks merge with user, project, and local hooks.
pub fn settings(exe: &Path) -> Value {
    // Quoted for the shell Claude Code runs hooks with: a path may hold `"` or `$`.
    let exe = crate::run::sh_quote(&exe.to_string_lossy());
    let cmd = |event: &str| format!("{exe} hook {event}");
    let entry = |event: &str, matcher: Option<&str>| {
        let mut m =
            json!({ "hooks": [{ "type": "command", "command": cmd(event), "timeout": 5 }] });
        if let Some(matcher) = matcher {
            m["matcher"] = json!(matcher);
        }
        json!([m])
    };
    let tools = HOOKED_TOOLS.join("|");
    let tools = tools.as_str();
    let seen = [&HOOKED_TOOLS[..], &READ_TOOLS[..]].concat().join("|");
    json!({
        "hooks": {
            "SessionStart": entry("SessionStart", None),
            // Bash too: the agent may not run `belay allow` (see Controller::on_tool_requested).
            "PreToolUse": entry("PreToolUse", Some(tools)),
            // Reads too (U5.1), after they run only: belay records them, never changes them.
            "PostToolUse": entry("PostToolUse", Some(seen.as_str())),
            "PostToolUseFailure": entry("PostToolUseFailure", Some(tools)),
            // R3.7: only belay's own rewrites (Bash) can make auto mode deny a call.
            "PermissionDenied": entry("PermissionDenied", Some("Bash")),
            "UserPromptSubmit": entry("UserPromptSubmit", None),
            "Stop": entry("Stop", None),
            "PreCompact": entry("PreCompact", None),
            "SubagentStop": entry("SubagentStop", None),
            // V3: a permission prompt or a question waits for the user.
            "Notification": entry("Notification", None),
        }
    })
}

#[cfg(test)]
connector_contract_tests!(
    contract,
    connector: crate::connector::claude_code::ClaudeCode::default(),
    fixtures: "fixtures/claude_code",
    hook_event: |v| v["hook_event_name"].as_str().map(String::from),
    pre_tool: "PreToolUse",
    post_tool: "PostToolUse",
    denied: |v| v["hookSpecificOutput"]["permissionDecision"] == "deny",
    replaced_output: |v| {
        let o = &v["hookSpecificOutput"]["updatedToolOutput"];
        o.as_str().or(o["stdout"].as_str()).map(String::from)
    },
    context: |v| v["hookSpecificOutput"]["additionalContext"].as_str().map(String::from),
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headless_stream_counts_each_call_once_and_finds_the_result() {
        let c = ClaudeCode::default();
        let text = include_str!("../../../../fixtures/claude_code/headless_stream.jsonl");
        let lines: Vec<_> = text.lines().filter_map(|l| c.headless_line(l)).collect();
        let mut seen = std::collections::BTreeSet::new();
        let total: u64 = lines
            .iter()
            .filter(|l| l.call.as_ref().is_none_or(|id| seen.insert(id.clone())))
            .map(|l| l.tokens)
            .sum();
        // msg_01A twice (8606), msg_01B once (152); cache reads are left out.
        assert_eq!(total, 8606 + 152);
        assert_eq!(
            lines.last().and_then(|l| l.result.as_deref()),
            Some("Done: a.txt says b.")
        );
        let a = c
            .headless_argv("-x fix it", Some("abc"), Some("haiku"))
            .unwrap();
        let tail = &a[a.len() - 7..];
        assert_eq!(
            tail,
            [
                "--model",
                "haiku",
                "--resume",
                "abc",
                "--fork-session",
                "--",
                "-x fix it"
            ]
        );
        assert_eq!(a[0], "-p");
    }
    use std::os::unix::fs::PermissionsExt;

    fn fixture(name: &str) -> Value {
        let path = format!(
            "{}/../../fixtures/claude_code/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn completed(evs: &[Event]) -> (&str, bool) {
        match evs.last() {
            Some(Event::ToolCompleted { output, failed, .. }) => (output, *failed),
            e => panic!("unexpected {e:?}"),
        }
    }

    #[test]
    fn maps_reads_and_searches_to_their_text() {
        let cases = [
            (
                "post_tool_use_read.json",
                ToolKind::Read,
                "from shop import total\n\n\ndef test_total():\n    assert total([1, 2, 3]) == 6\n",
            ),
            (
                "post_tool_use_grep.json",
                ToolKind::Search,
                "shop/__init__.py\ntests/test_api.py",
            ),
            (
                "post_tool_use_grep_content.json",
                ToolKind::Search,
                "shop/__init__.py:1:def total(xs):",
            ),
            (
                "post_tool_use_glob.json",
                ToolKind::Search,
                "/home/dev/shop/tests/test_api.py\n/home/dev/shop/tests/test_cart.py",
            ),
        ];
        for (name, want_kind, want) in cases {
            let mut c = ClaudeCode {
                started: true,
                ..Default::default()
            };
            let evs = c.map("PostToolUse", &fixture(name));
            assert_eq!(evs.len(), 1, "{name}: {evs:?}");
            match &evs[0] {
                Event::ToolCompleted {
                    kind,
                    output,
                    failed,
                    ..
                } => {
                    assert_eq!((*kind, output.as_str(), *failed), (want_kind, want, false));
                }
                e => panic!("{name}: {e:?}"),
            }
        }
    }

    #[test]
    fn maps_a_notification_to_agent_waiting_and_answers_nothing() {
        let mut c = ClaudeCode {
            started: true,
            session_id: "9b1d6f2e-0000-4000-8000-000000000001".into(),
            ..Default::default()
        };
        let p = fixture("notification_permission.json");
        assert_eq!(
            c.map("Notification", &p),
            vec![Event::AgentWaiting {
                reason: "permission_prompt".into(),
                text: "Claude needs your permission to use Bash".into(),
            }]
        );
        assert_eq!(c.encode("Notification", &p, &Response::default()), None);
        let hooks = settings(Path::new("/bin/belay"));
        assert!(hooks["hooks"]["Notification"].is_array());
    }

    #[test]
    fn maps_subagent_stop() {
        let mut c = ClaudeCode {
            started: true,
            session_id: "9b1d6f2e-0000-4000-8000-000000000001".into(),
            ..Default::default()
        };
        let evs = c.map("SubagentStop", &fixture("subagent_stop.json"));
        assert_eq!(
            evs,
            vec![Event::SubagentEnded {
                agent_type: Some("Explore".into())
            }]
        );
    }

    #[test]
    fn settings_hook_reads_after_they_run_only() {
        let hooks = &settings(Path::new("/x/belay"))["hooks"];
        let post = hooks["PostToolUse"][0]["matcher"].as_str().unwrap();
        for t in ["Bash", "Edit", "Read", "Grep", "Glob"] {
            assert!(post.split('|').any(|m| m == t), "{post}");
        }
        assert_eq!(
            hooks["PreToolUse"][0]["matcher"],
            "Bash|Edit|MultiEdit|Write"
        );
        assert!(hooks["SubagentStop"].is_array());
    }

    #[test]
    fn maps_bash_post_tool_use() {
        let mut c = ClaudeCode::default();
        let evs = c.map("PostToolUse", &fixture("post_tool_use_bash.json"));
        assert!(matches!(evs[0], Event::SessionStarted { .. }));
        match &evs[1] {
            Event::ToolCompleted {
                tool,
                output,
                failed,
                ..
            } => {
                assert_eq!(tool, "Bash");
                assert!(output.starts_with("===="));
                assert!(output.contains("300 passed"));
                assert!(!failed);
            }
            e => panic!("unexpected {e:?}"),
        }
        assert_eq!(
            c.map("PostToolUse", &fixture("post_tool_use_bash.json"))
                .len(),
            1
        );
    }

    #[test]
    fn bash_stderr_is_merged_into_stdout() {
        let mut c = ClaudeCode {
            started: true,
            ..Default::default()
        };
        let evs = c.map("PostToolUse", &fixture("post_tool_use_bash_stderr.json"));
        assert_eq!(completed(&evs), ("out-line\nerr-line", false));
    }

    #[test]
    fn persisted_bash_output_is_the_preview() {
        let mut c = ClaudeCode {
            started: true,
            ..Default::default()
        };
        let p = fixture("post_tool_use_bash_persisted.json");
        let evs = c.map("PostToolUse", &p);
        let (output, _) = completed(&evs);
        let stdout = p["tool_response"]["stdout"].as_str().unwrap();
        assert_eq!(stdout.len(), 30000);
        assert!(output.starts_with(
            "<persisted-output>\nOutput too large (106.3KB). Full output saved to: /home/dev/"
        ));
        assert!(output.contains("Preview (first 2KB):\n1\n2\n3\n"));
        assert!(output.ends_with("\n526\n527\n...\n</persisted-output>"));
        assert!(output.len() < 2300);
    }

    #[test]
    fn persisted_size_switches_to_mb() {
        let out = persisted_output("1\n2\n", "/p", 1_988_895);
        assert!(out.contains("Output too large (1.9MB)"), "{out}");
    }

    /// Captured N14: 40000 `x` and exactly 1 MiB of `a`, both with no newline.
    #[test]
    fn persisted_edge_cases() {
        for (name, size, preview_len) in [
            (
                "post_tool_use_bash_persisted_no_newline.json",
                "39.1KB",
                2000,
            ),
            ("post_tool_use_bash_persisted_1mib.json", "1MB", 2000),
        ] {
            let mut c = ClaudeCode {
                started: true,
                ..Default::default()
            };
            let evs = c.map("PostToolUse", &fixture(name));
            let (out, _) = completed(&evs);
            assert!(
                out.contains(&format!("Output too large ({size})")),
                "{name}: {out}"
            );
            let preview = out
                .split("Preview (first 2KB):\n")
                .nth(1)
                .and_then(|p| p.strip_suffix("\n...\n</persisted-output>"))
                .unwrap();
            assert_eq!(preview.len(), preview_len, "{name}");
        }
    }

    /// U8: the arguments that resume a conversation, and those that do not.
    #[test]
    fn resume_flags_name_a_conversation_the_latest_or_the_picker() {
        let c = ClaudeCode::default();
        let t = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            c.resume_target(&args, Path::new("/x"))
        };
        let conv = |id: &str| Some(ResumeTarget::Conversation(id.into()));
        assert_eq!(t(&["--resume", "abc"]), conv("abc"));
        assert_eq!(t(&["-r", "abc", "--model", "x"]), conv("abc"));
        assert_eq!(t(&["--resume=abc"]), conv("abc"));
        assert_eq!(t(&["--continue"]), Some(ResumeTarget::Latest));
        assert_eq!(t(&["-p", "hi", "-c"]), Some(ResumeTarget::Latest));
        assert_eq!(t(&["--resume"]), Some(ResumeTarget::Picker));
        assert_eq!(t(&["-r", "--model", "x"]), Some(ResumeTarget::Picker));
        assert_eq!(t(&["--model", "x"]), None);
        assert_eq!(t(&["--", "--resume", "abc"]), None, "a prompt, not a flag");
    }

    /// U8: a resumed launch's first hook is SessionStart with `source: "resume"`: the
    /// session starts, and the connector knows the conversation's id.
    #[test]
    fn a_resumed_start_starts_the_session_and_names_the_conversation() {
        let mut c = ClaudeCode::default();
        assert_eq!(c.conversation(), None);
        let evs = c.map("SessionStart", &fixture("session_start_resume.json"));
        assert!(
            matches!(&evs[..], [Event::SessionStarted { .. }]),
            "{evs:?}"
        );
        assert_eq!(
            c.conversation().as_deref(),
            Some("9b1d6f2e-0000-4000-8000-000000000001")
        );
    }

    #[test]
    fn project_dirs_are_named_as_claude_code_names_them() {
        assert_eq!(
            project_dir_name(Path::new("/home/dev/my.shop_1")),
            "-home-dev-my-shop-1"
        );
    }

    /// U4b: a SessionStart after a compaction keeps the session id; it restarts the
    /// context, not the conversation.
    #[test]
    fn a_compact_session_start_restarts_the_context() {
        let mut c = ClaudeCode::default();
        let evs = c.map("SessionStart", &fixture("session_start.json"));
        assert!(matches!(&evs[..], [Event::SessionStarted { .. }]));
        let evs = c.map("SessionStart", &fixture("session_start_compact.json"));
        assert!(
            matches!(&evs[..], [Event::ContextRestarted { source }] if source == "compact"),
            "{evs:?}"
        );
        let r = Response {
            start_context: vec!["carried".into()],
            ..Response::default()
        };
        let out = c
            .encode("SessionStart", &fixture("session_start_compact.json"), &r)
            .unwrap();
        assert_eq!(out["hookSpecificOutput"]["additionalContext"], "carried");
    }

    /// R3: a new session id in the same process (`/clear`, `/resume`) is a new
    /// conversation with its own transcript.
    #[test]
    fn new_session_id_is_a_new_conversation() {
        let mut c = ClaudeCode::default();
        let mut p = fixture("pre_tool_use_bash_auto.json");
        p["hook_event_name"] = json!("SessionStart");
        p["source"] = json!("startup");
        assert!(matches!(
            c.map("SessionStart", &p)[0],
            Event::SessionStarted { .. }
        ));
        assert!(c.map("SessionStart", &p).is_empty(), "same conversation");
        p["session_id"] = json!("9b1d6f2e-0000-4000-8000-000000000002");
        p["transcript_path"] = json!("/home/dev/.claude/projects/x/second.jsonl");
        p["source"] = json!("clear");
        let evs = c.map("SessionStart", &p);
        assert!(
            matches!(&evs[..], [Event::ConversationChanged { transcript_path: Some(t), reason }]
                if t.ends_with("second.jsonl") && reason == "clear"),
            "{evs:?}"
        );
    }

    /// T8: real captures from Claude Code 2.1.304 (conformance run): a PreToolUse Bash
    /// payload in auto mode, and the PostToolUseFailure of a wrapped test run, whose
    /// error text is belay's summary.
    #[test]
    fn real_auto_mode_and_wrapped_failure_payloads() {
        let mut c = ClaudeCode {
            started: true,
            ..Default::default()
        };
        let pre = fixture("pre_tool_use_bash_auto.json");
        assert_eq!(pre["permission_mode"], "auto");
        let evs = c.map("PreToolUse", &pre);
        assert!(
            matches!(&evs[..], [Event::ToolRequested { tool, input, .. }]
                if tool == "Bash" && input["command"] == "python3 -m pytest -v tests"),
            "{evs:?}"
        );
        let post = fixture("post_tool_use_failure_wrapped.json");
        let evs = c.map("PostToolUseFailure", &post);
        let Some(Event::ToolCompleted {
            failed,
            output,
            input,
            ..
        }) = evs.last()
        else {
            panic!("{evs:?}")
        };
        assert!(*failed);
        assert!(input["command"].as_str().unwrap().contains(" run --id "));
        assert!(output.contains("full output: belay show o-1"), "{output}");
        assert!(output.contains("FAILED tests/test_api.py::test_total_empty"));
    }

    /// R3.7: payload shape from the 2.1.303 hook schema (not yet captured live).
    #[test]
    fn permission_denied_maps_and_encodes_retry() {
        let mut c = ClaudeCode {
            started: true,
            ..Default::default()
        };
        let p = fixture("permission_denied_bash.json");
        let evs = c.map("PermissionDenied", &p);
        assert!(matches!(
            &evs[..],
            [Event::ToolDenied { tool_use_id, reason, .. }]
                if tool_use_id == "toolu_07" && reason.starts_with("[Auto-Mode Bypass]")
        ));
        assert_eq!(c.encode("PermissionDenied", &p, &Response::default()), None);
        let r = Response {
            retry: true,
            ..Default::default()
        };
        assert_eq!(
            c.encode("PermissionDenied", &p, &r),
            Some(json!({ "hookSpecificOutput": {
                "hookEventName": "PermissionDenied", "retry": true
            }}))
        );
        let hooks = &settings(Path::new("/x/belay"))["hooks"];
        assert_eq!(hooks["PermissionDenied"][0]["matcher"], "Bash");
    }

    #[test]
    fn maps_bash_failures() {
        let mut c = ClaudeCode {
            started: true,
            ..Default::default()
        };
        let evs = c.map(
            "PostToolUseFailure",
            &fixture("post_tool_use_failure_bash.json"),
        );
        let (output, failed) = completed(&evs);
        assert!(failed);
        assert!(output.starts_with("Exit code 1\n"));
        assert!(output.contains("[16523 characters truncated]"));
        assert!(output.contains("FAILED tests/test_api.py::test_create_order - KeyError: 'total'"));

        let evs = c.map(
            "PostToolUseFailure",
            &fixture("post_tool_use_failure_bash_exit.json"),
        );
        assert_eq!(completed(&evs), ("Exit code 3\nout-line\nerr-line", true));
    }

    /// R1.4: test files a Bash command names are compared before and after it runs.
    /// I2 G2: a Bash write to a source file that holds unit tests is guarded too; a
    /// source file without tests is not snapshotted.
    #[test]
    fn bash_writes_to_inline_tests_become_file_changes() {
        let dir = std::env::temp_dir().join(format!("belay-bashw-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let lib = dir.join("src/lib.rs");
        let plain = dir.join("src/main.rs");
        let tests = "fn t() -> u32 { 6 }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t6() { assert_eq!(super::t(), 6); }\n}\n";
        std::fs::write(&lib, tests).unwrap();
        std::fs::write(&plain, "fn main() { assert!(true); }\n").unwrap();

        let mut c = ClaudeCode::default();
        let mut pre = fixture("post_tool_use_bash.json");
        pre["hook_event_name"] = json!("PreToolUse");
        pre.as_object_mut().unwrap().remove("tool_response");
        pre["cwd"] = json!(dir.display().to_string());
        pre["tool_input"]["command"] = json!("sed -i '/assert/d' src/lib.rs src/main.rs");
        c.map("PreToolUse", &pre);
        std::fs::write(&lib, tests.replace("assert_eq!(super::t(), 6);", "")).unwrap();
        std::fs::write(&plain, "fn main() { }\n").unwrap();

        let mut post = fixture("post_tool_use_bash.json");
        post["cwd"] = pre["cwd"].clone();
        post["tool_input"] = pre["tool_input"].clone();
        let paths: Vec<String> = c
            .map("PostToolUse", &post)
            .iter()
            .filter_map(|e| match e {
                Event::FileChanged { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(paths, [lib.display().to_string()]);
    }

    #[test]
    fn bash_writes_to_test_files_become_file_changes() {
        let dir = std::env::temp_dir().join(format!("belay-bashw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("shop/tests")).unwrap();
        let test = dir.join("shop/tests/test_a.py");
        std::fs::write(&test, "assert a\nassert b\n").unwrap();
        std::fs::write(dir.join("shop/tests/test_b.py"), "assert c\n").unwrap();

        let mut c = ClaudeCode::default();
        let mut pre = fixture("post_tool_use_bash.json");
        pre["hook_event_name"] = json!("PreToolUse");
        pre.as_object_mut().unwrap().remove("tool_response");
        pre["cwd"] = json!(dir.display().to_string());
        pre["tool_input"]["command"] = json!("cd shop && sed -i '/assert b/d' tests/*.py");
        c.map("PreToolUse", &pre);
        std::fs::write(&test, "assert a\n").unwrap();
        // R4: the turn ends while the call still runs (a background subagent): the
        // snapshot is recent, so it stays.
        c.map("Stop", &json!({}));

        let mut post = fixture("post_tool_use_bash.json");
        post["cwd"] = pre["cwd"].clone();
        post["tool_input"] = pre["tool_input"].clone();
        let evs = c.map("PostToolUse", &post);
        let changed: Vec<_> = evs
            .iter()
            .filter_map(|e| match e {
                Event::FileChanged {
                    path,
                    old_text,
                    new_text,
                    ..
                } => Some((path.clone(), old_text.as_str(), new_text.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            changed,
            [(
                test.display().to_string(),
                "assert a\nassert b\n",
                "assert a\n"
            )],
            "test_b.py did not change"
        );
        assert!(matches!(evs.last(), Some(Event::ToolCompleted { .. })));
        // The snapshot is used once.
        let evs = c.map("PostToolUse", &post);
        assert!(!evs.iter().any(|e| matches!(e, Event::FileChanged { .. })));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cards and peers missed what the shell wrote outside tests (live handoff check):
    /// a file a Bash write names that changed, or appeared, is an edit.
    #[test]
    fn bash_writes_to_other_files_become_edits() {
        let dir = std::env::temp_dir().join(format!("belay-bashw-other-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("shop")).unwrap();
        let app = dir.join("shop/app.py");
        std::fs::write(&app, "x = 1\n").unwrap();
        std::fs::write(dir.join("shop/same.py"), "y = 1\n").unwrap();

        let mut c = ClaudeCode::default();
        let mut pre = fixture("post_tool_use_bash.json");
        pre["hook_event_name"] = json!("PreToolUse");
        pre.as_object_mut().unwrap().remove("tool_response");
        pre["cwd"] = json!(dir.display().to_string());
        pre["tool_input"]["command"] =
            json!("cd shop && sed -i 's/x = 1/x = 2/' app.py same.py && echo n > notes.md");
        c.map("PreToolUse", &pre);
        std::fs::write(&app, "x = 2\n").unwrap();
        std::fs::write(dir.join("shop/notes.md"), "n\n").unwrap();

        let mut post = fixture("post_tool_use_bash.json");
        post["cwd"] = pre["cwd"].clone();
        post["tool_input"] = pre["tool_input"].clone();
        let edited: Vec<String> = c
            .map("PostToolUse", &post)
            .iter()
            .filter_map(|e| match e {
                Event::FileEdited {
                    path, after_hash, ..
                } => {
                    assert!(after_hash.is_some(), "{path}");
                    Some(path.clone())
                }
                _ => None,
            })
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            edited,
            [
                app.display().to_string(),
                dir.join("shop/notes.md").display().to_string()
            ],
            "same.py did not change; notes.md is new"
        );
    }

    #[test]
    fn maps_edit_and_write() {
        let mut c = ClaudeCode {
            started: true,
            ..Default::default()
        };
        let evs = c.map("PreToolUse", &fixture("pre_tool_use_edit_test.json"));
        match &evs[0] {
            Event::EditProposed {
                path,
                old_text,
                new_text,
                ..
            } => {
                assert_eq!(path, "/home/dev/shop/tests/test_api.py");
                assert_eq!(old_text.matches("assert").count(), 2);
                assert_eq!(new_text.matches("assert").count(), 1);
            }
            e => panic!("unexpected {e:?}"),
        }
        let evs = c.map("PreToolUse", &fixture("pre_tool_use_write.json"));
        assert!(
            matches!(&evs[0], Event::EditProposed { path, new_text, .. } if path == "/home/dev/shop/notes.txt" && new_text == "first\n")
        );
        for name in ["post_tool_use_edit.json", "post_tool_use_write.json"] {
            let evs = c.map("PostToolUse", &fixture(name));
            assert!(matches!(&evs[0], Event::FileEdited { .. }), "{name}");
            assert!(
                matches!(&evs[1], Event::ToolCompleted { failed: false, .. }),
                "{name}"
            );
        }
    }

    #[test]
    fn maps_lifecycle_events() {
        let mut c = ClaudeCode::default();
        let evs = c.map("SessionStart", &fixture("session_start.json"));
        assert!(
            matches!(&evs[..], [Event::SessionStarted { cwd, transcript_path: Some(_), .. }] if cwd == "/home/dev/shop")
        );
        let evs = c.map("UserPromptSubmit", &fixture("user_prompt_submit.json"));
        assert!(matches!(&evs[..], [Event::PromptSubmitted { chars }] if *chars > 0));
        let evs = c.map("Stop", &fixture("stop.json"));
        assert!(matches!(&evs[..], [Event::TurnEnded]));
    }

    #[test]
    fn encodes_replacement_in_original_shape() {
        let c = ClaudeCode::default();
        let p = fixture("post_tool_use_bash.json");
        let r = Response {
            replace_output: Some("short".into()),
            ..Default::default()
        };
        let out = c.encode("PostToolUse", &p, &r).unwrap();
        assert_eq!(
            out["hookSpecificOutput"]["updatedToolOutput"]["stdout"],
            "short"
        );
        assert_eq!(c.encode("PostToolUse", &p, &Response::default()), None);
    }

    #[test]
    fn wraps_test_commands_only_where_no_prompt_can_appear() {
        let mut c = ClaudeCode {
            started: true,
            ..Default::default()
        };
        let mut p = fixture("post_tool_use_bash.json");
        p["hook_event_name"] = json!("PreToolUse");
        p.as_object_mut().unwrap().remove("tool_response");
        for (mode, bg, want) in [
            ("auto", false, true),
            ("bypassPermissions", false, true),
            ("default", false, false),
            ("acceptEdits", false, false),
            ("auto", true, false),
        ] {
            p["permission_mode"] = json!(mode);
            p["tool_input"]["run_in_background"] = json!(bg);
            let evs = c.map("PreToolUse", &p);
            assert!(
                matches!(&evs[0], Event::ToolRequested { rewrite_ok, .. } if *rewrite_ok == want),
                "{mode} bg={bg}"
            );
        }
        // R0.4: a runner that is not an executable (a shell function, a missing path).
        let mut q = p.clone();
        q["permission_mode"] = json!("auto");
        q["tool_input"]["run_in_background"] = json!(false);
        for cmd in [
            "/nonexistent/bin/pytest -q",
            "./no-such-venv/bin/python -m pytest",
        ] {
            q["tool_input"]["command"] = json!(cmd);
            let evs = c.map("PreToolUse", &q);
            assert!(
                matches!(
                    &evs[0],
                    Event::ToolRequested {
                        rewrite_ok: false,
                        ..
                    }
                ),
                "{cmd}"
            );
        }
        // R1.1: a runner inside an activated venv is found through the activate script.
        let venv = std::env::temp_dir().join(format!("belay-venv-{}", std::process::id()));
        std::fs::create_dir_all(venv.join(".venv/bin")).unwrap();
        std::fs::write(venv.join(".venv/bin/activate"), "").unwrap();
        let runner = venv.join(".venv/bin/py.test");
        std::fs::write(&runner, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o755)).unwrap();
        q["cwd"] = json!(venv.display().to_string());
        q["tool_input"]["command"] =
            json!("source .venv/bin/activate && py.test -q 2>&1 | tail -5");
        let evs = c.map("PreToolUse", &q);
        assert!(matches!(
            &evs[0],
            Event::ToolRequested {
                rewrite_ok: true,
                ..
            }
        ));
        // The same from the parent dir, after a leading cd.
        q["cwd"] = json!(venv.parent().unwrap().display().to_string());
        let name = venv.file_name().unwrap().to_string_lossy().to_string();
        let cmd = format!("cd {name} && source .venv/bin/activate && py.test -q");
        q["tool_input"]["command"] = json!(cmd);
        let evs = c.map("PreToolUse", &q);
        assert!(matches!(
            &evs[0],
            Event::ToolRequested {
                rewrite_ok: true,
                ..
            }
        ));
        let r = Response {
            wrap: true,
            ..Default::default()
        };
        let out = c.encode("PreToolUse", &q, &r).unwrap();
        let new = out["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap();
        assert!(
            new.starts_with(&format!("cd {name} && '")),
            "the cd stays outside: {new}"
        );
        assert!(
            new.ends_with("--sh 'source .venv/bin/activate && py.test -q'"),
            "{new}"
        );
        let _ = std::fs::remove_dir_all(&venv);

        let r = Response {
            wrap: true,
            ..Default::default()
        };
        let out = c.encode("PreToolUse", &p, &r).unwrap();
        let h = &out["hookSpecificOutput"];
        let original = p["tool_input"]["command"].as_str().unwrap();
        assert_eq!(
            h["updatedInput"]["command"],
            crate::run::wrapped_command(
                &std::env::current_exe().unwrap(),
                "toolu_01",
                None,
                original
            )
        );
        assert_eq!(
            h["updatedInput"]["run_in_background"], true,
            "other fields kept"
        );
        assert!(h.get("permissionDecision").is_none(), "never auto-allow");
    }

    #[test]
    fn maps_tool_names_to_kinds() {
        for (tool, kind) in [
            ("Bash", ToolKind::Shell),
            ("Edit", ToolKind::Edit),
            ("MultiEdit", ToolKind::Edit),
            ("Write", ToolKind::Write),
            ("Read", ToolKind::Read),
            ("Grep", ToolKind::Search),
            ("Glob", ToolKind::Search),
            ("WebFetch", ToolKind::Fetch),
            ("WebSearch", ToolKind::Fetch),
            ("mcp__github__create_issue", ToolKind::Mcp),
            ("Task", ToolKind::Subagent),
            ("Agent", ToolKind::Subagent),
            ("TodoWrite", ToolKind::Other),
            ("", ToolKind::Other),
        ] {
            assert_eq!(tool_kind(tool), kind, "{tool}");
        }
        let mut c = ClaudeCode::default();
        let evs = c.map("PostToolUse", &fixture("post_tool_use_bash.json"));
        assert!(matches!(
            evs.last(),
            Some(Event::ToolCompleted {
                kind: ToolKind::Shell,
                ..
            })
        ));
    }

    #[test]
    fn encodes_start_context_only_at_session_start_and_notices_for_the_user() {
        let c = ClaudeCode::default();
        let r = Response {
            start_context: vec!["peers".into()],
            add_context: vec!["more".into()],
            ..Default::default()
        };
        let out = c
            .encode("SessionStart", &fixture("session_start.json"), &r)
            .unwrap();
        assert_eq!(
            out["hookSpecificOutput"]["additionalContext"],
            "peers\nmore"
        );
        let out = c
            .encode("UserPromptSubmit", &fixture("user_prompt_submit.json"), &r)
            .unwrap();
        assert_eq!(out["hookSpecificOutput"]["additionalContext"], "more");

        let user_only = Response {
            notices: vec![belay_core::Notice {
                level: belay_core::NoticeLevel::Info,
                text: "for the user".into(),
                action: None,
            }],
            start_context: vec!["only at start".into()],
            ..Default::default()
        };
        for (event, file) in [
            ("PostToolUse", "post_tool_use_bash.json"),
            ("Stop", "stop.json"),
            ("UserPromptSubmit", "user_prompt_submit.json"),
        ] {
            // Rule 5: the notice is a user-only `systemMessage`, never context.
            assert_eq!(
                c.encode(event, &fixture(file), &user_only),
                Some(json!({ "systemMessage": "belay: for the user" })),
                "{event}"
            );
        }
        // With context, the notice rides along and stays out of `additionalContext`.
        let both = Response {
            add_context: vec!["for the agent".into()],
            ..user_only.clone()
        };
        let out = c
            .encode("PostToolUse", &fixture("post_tool_use_bash.json"), &both)
            .unwrap();
        assert_eq!(out["systemMessage"], "belay: for the user");
        assert_eq!(
            out["hookSpecificOutput"]["additionalContext"],
            "for the agent"
        );
        assert_eq!(
            c.encode("Stop", &fixture("stop.json"), &Response::default()),
            None
        );
    }

    #[test]
    fn continue_turn_is_a_stop_block_and_only_at_stop() {
        let c = ClaudeCode::default();
        let r = Response {
            continue_turn: Some("run it".into()),
            ..Default::default()
        };
        assert_eq!(
            c.encode("Stop", &fixture("stop.json"), &r),
            Some(json!({ "decision": "block", "reason": "run it" }))
        );
        assert_eq!(
            c.encode("UserPromptSubmit", &fixture("user_prompt_submit.json"), &r),
            None
        );
        // A notice rides along for the user; the reason is the agent's.
        let both = Response {
            notices: vec![belay_core::Notice {
                level: belay_core::NoticeLevel::Act,
                text: "asked for tests".into(),
                action: None,
            }],
            ..r
        };
        assert_eq!(
            c.encode("Stop", &fixture("stop.json"), &both),
            Some(
                json!({ "decision": "block", "reason": "run it", "systemMessage": "belay: asked for tests" })
            )
        );
    }

    #[test]
    fn several_notices_make_one_line_led_by_a_warning() {
        let n = |level, text: &str| belay_core::Notice {
            level,
            text: text.into(),
            action: None,
        };
        let notices = [
            n(belay_core::NoticeLevel::Act, "summarized"),
            n(belay_core::NoticeLevel::Warn, "blocked"),
        ];
        assert_eq!(user_notice(&notices).unwrap(), "belay: blocked (+1 more)");
        assert_eq!(user_notice(&[]), None);
    }

    #[test]
    fn encodes_permission() {
        let c = ClaudeCode::default();
        let r = Response {
            permission: Some(Permission::Ask("why".into())),
            ..Default::default()
        };
        let out = c
            .encode("PreToolUse", &fixture("pre_tool_use_edit_test.json"), &r)
            .unwrap();
        assert_eq!(out["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(out["hookSpecificOutput"]["permissionDecision"], "ask");
        assert_eq!(out["hookSpecificOutput"]["permissionDecisionReason"], "why");
    }

    #[test]
    fn wrap_with_non_object_tool_input_does_not_panic() {
        let c = ClaudeCode::default();
        let r = Response {
            wrap: true,
            ..Default::default()
        };
        for input in [json!("ls"), json!([1]), json!(7), Value::Null] {
            let p = json!({ "tool_use_id": "t1", "tool_input": input });
            let out = c.encode("PreToolUse", &p, &r).unwrap();
            assert!(out["hookSpecificOutput"]["updatedInput"].is_object());
        }
    }

    #[test]
    fn a_plugin_daemon_names_its_socket_in_the_wrapped_command() {
        let mut c = ClaudeCode::default();
        c.set_socket(std::path::Path::new("/tmp/belay s/belayd.sock"));
        let p: Value = serde_json::from_str(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/claude_code/pre_tool_use_bash_auto.json"
            ))
            .unwrap(),
        )
        .unwrap();
        let r = Response {
            wrap: true,
            ..Default::default()
        };
        let out = c.encode("PreToolUse", &p, &r).unwrap();
        let cmd = out["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap();
        assert!(
            cmd.contains(" --socket '/tmp/belay s/belayd.sock' --sh "),
            "{cmd}"
        );
    }
}
