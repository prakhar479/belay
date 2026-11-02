//! `belay mcp`: an MCP server over stdio for memory and the ledger.
//!
//! JSON-RPC 2.0, one message per line each way. Only what a tools-only server needs:
//! `initialize`, `ping`, `tools/list`, `tools/call`. Written by hand on serde_json: the
//! protocol surface is small, and an SDK would add an async runtime to the binary.
//! Everything is read from and written to this machine; nothing calls out. The two
//! sandbox tools run `belay sandbox run|task` as the agent, under the same rules.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use belay_core::memory::{Kind, Memory, Provenance, Scope};
use serde_json::{json, Value};

use crate::{commons, fleet, learnstore, memorystore, paths, report, sessions, skillstore, store};

/// The newest protocol revision this server speaks; a client's own is echoed back.
const PROTOCOL: &str = "2025-06-18";
/// Hits `memory_search` returns when no limit is given, and at most.
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 50;

// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Serve until stdin closes.
pub fn run() -> Result<i32> {
    let stdin = std::io::stdin().lock();
    let mut out = std::io::stdout().lock();
    for line in stdin.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = handle_line(&line) {
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
    }
    Ok(0)
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// The reply to one line, or None for a notification (or a response to us).
fn handle_line(line: &str) -> Option<Value> {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        return Some(error(Value::Null, PARSE_ERROR, "not JSON"));
    };
    let id = msg.get("id").cloned();
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        // A response to a request we never send, or junk.
        let is_response = msg.get("result").is_some() || msg.get("error").is_some();
        return (!is_response)
            .then(|| error(id.unwrap_or(Value::Null), INVALID_REQUEST, "not a request"));
    };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => call(&params),
        _ if id.is_none() => return None,
        _ => Err((METHOD_NOT_FOUND, format!("no method {method}"))),
    };
    // A notification gets no reply, even one we could not handle.
    let id = id?;
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, m)) => error(id, code, &m),
    })
}

fn initialize(params: &Value) -> Value {
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "belay", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "belay's memory for this project and the record of this session. \
    Search memory before you start a task; add what the next session should know.",
    })
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required })
}

fn tools() -> Value {
    let strings = json!({ "type": "array", "items": { "type": "string" } });
    let kinds: Vec<&str> = Kind::ALL.iter().map(|k| k.name()).collect();
    json!([
        {
            "name": "memory_search",
            "description": "Search belay's memories for this project and the user: facts, gotchas, decisions, learned fixes, findings, handoffs. Best matches first.",
            "inputSchema": schema(json!({
                "query": { "type": "string", "description": "Words, an error message, or file names." },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "description": "Most hits to return (default 10)." },
            }), &["query"]),
        },
        {
            "name": "memory_add",
            "description": "Keep a fact for later sessions in this project. It is kept as the agent's until the user approves it.",
            "inputSchema": schema(json!({
                "text": { "type": "string", "description": "One fact, in a plain sentence." },
                "kind": { "type": "string", "enum": kinds, "description": "Default: note." },
                "tags": strings,
                "files": strings,
            }), &["text"]),
        },
        {
            "name": "handoff_get",
            "description": "The newest handoff a session in this project left for the next one.",
            "inputSchema": schema(json!({}), &[]),
        },
        {
            "name": "output_show",
            "description": "The full original of an output belay summarized in this session (an id like o-3).",
            "inputSchema": schema(json!({ "id": { "type": "string", "description": "o-N" } }), &["id"]),
        },
        {
            "name": "skill_preview",
            "description": "Fetch a skill (GitHub tree or blob URL, raw SKILL.md URL, .zip or .tar.gz, or a path) and show its name, files, risk warnings, and digest. Installs nothing.",
            "inputSchema": schema(json!({ "url": { "type": "string" } }), &["url"]),
        },
        {
            "name": "skill_stage",
            "description": "Stage a skill for the user to approve. Nothing is installed until the user runs `belay skill approve <name>` in their own terminal.",
            "inputSchema": schema(json!({
                "url": { "type": "string" },
                "scope": { "type": "string", "enum": ["user", "project"], "description": "Default: user." },
                "name": { "type": "string", "description": "Install under this name instead of the one in SKILL.md." },
            }), &["url"]),
        },
        {
            "name": "why",
            "description": "Why belay did something in this session: the cause chain of an action (a-N), output (o-N), or entry number.",
            "inputSchema": schema(json!({ "id": { "type": "string", "description": "a-N, o-N, or an entry number" } }), &["id"]),
        },
        {
            "name": "brief",
            "description": "Summarize this session for review: files changed, each test or check command and whether it passed after the last edit, and what to look at (guard actions, CI and dependency changes, suppressions), each with its evidence id. From belay's ledger and git, not the agent's claims.",
            "inputSchema": schema(json!({
                "format": { "type": "string", "enum": ["text", "markdown", "json"], "description": "Default: text. markdown: a pull request section." },
            }), &[]),
        },
        {
            "name": "sandbox_run",
            "description": "Run a command in a sandbox: a clone of this repo with a private home, limits, and no access to the rest of the machine (the network is the host's). Its changes land on branch belay/sandbox/<id> for the user to merge. For risky commands and experiments.",
            "inputSchema": schema(json!({
                "argv": { "type": "array", "items": { "type": "string" }, "description": "The command and its arguments." },
                "id": { "type": "string", "description": "Reuse (or name) this sandbox." },
            }), &["argv"]),
        },
        {
            "name": "sandbox_task",
            "description": "A headless follow-up: an agent works on the prompt alone in a sandbox and stops. Returns its final message, the branch, the files changed, and the audit. Limited per hour and by a token budget.",
            "inputSchema": schema(json!({
                "prompt": { "type": "string" },
                "id": { "type": "string", "description": "Reuse (or name) this sandbox." },
                "resume": { "type": "string", "description": "Start from a copy of this conversation's context." },
                "model": { "type": "string" },
            }), &["prompt"]),
        },
    ])
}

/// `tools/call`: a tool's failure is a result with `isError`, so the model reads it; an
/// unknown tool or a malformed call is a protocol error.
fn call(params: &Value) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((INVALID_PARAMS, "tools/call needs a name".to_string()))?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let said = match name {
        "memory_search" => memory_search(&args),
        "memory_add" => memory_add(&args),
        "handoff_get" => handoff_get(),
        "output_show" => str_arg(&args, "id").and_then(|id| output_show(&session()?, id)),
        "why" => str_arg(&args, "id").and_then(|id| why(&session()?, id)),
        "skill_preview" => str_arg(&args, "url").and_then(skillstore::preview),
        // Always a stage: the server runs under the agent, whoever started it.
        "skill_stage" => str_arg(&args, "url").and_then(|url| {
            let opt = |k: &str| args.get(k).and_then(Value::as_str);
            skillstore::stage(url, opt("scope"), opt("name"))
        }),
        "brief" => brief(&args),
        "sandbox_run" => sandbox_run(&args),
        "sandbox_task" => sandbox_task(&args),
        _ => return Err((INVALID_PARAMS, format!("no tool {name}"))),
    };
    let (text, is_error) = match said {
        Ok(t) => (t, false),
        Err(e) => (format!("{e:#}"), true),
    };
    Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }))
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("`{key}` is required"))
}

fn strings_arg(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn project() -> Result<String> {
    Ok(learnstore::project_of(&std::env::current_dir()?))
}

fn memory_search(args: &Value) -> Result<String> {
    let query = str_arg(args, "query")?;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_LIMIT, |n| (n as usize).clamp(1, MAX_LIMIT));
    memorystore::search_top(query, limit)
}

/// Always the agent's: the server runs under the agent, whoever started it.
fn memory_add(args: &Value) -> Result<String> {
    let text = str_arg(args, "text")?;
    let kind = match args.get("kind").and_then(Value::as_str) {
        None => Kind::Note,
        Some(k) => Kind::parse(k).ok_or_else(|| {
            let names: Vec<&str> = Kind::ALL.iter().map(|k| k.name()).collect();
            anyhow!("unknown kind {k}: use one of {}", names.join(", "))
        })?,
    };
    let project = project()?;
    let m = memorystore::add_in(&memorystore::dir(), |id| {
        Memory::new(
            id,
            Scope::Project,
            kind,
            Provenance::Agent,
            Some(&project),
            text,
            &strings_arg(args, "tags"),
            &strings_arg(args, "files"),
            paths::now_ms(),
        )
    })?;
    Ok(format!(
        "Kept {} as the agent's. The user can `belay memory approve {}`.",
        m.id, m.id
    ))
}

fn handoff_get() -> Result<String> {
    let cards = commons::read_cards(&paths::sessions_dir());
    let me = session_env()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_default();
    let now = paths::now_ms();
    Ok(
        match commons::latest_handoff(&cards, &me, &project()?, now) {
            Some(h) => belay_core::commons::handoff_text(&h, now),
            None => "No handoff for this repo: no session here ended in the last week with one."
                .to_string(),
        },
    )
}

fn session_env() -> Option<PathBuf> {
    paths::env_path("BELAY_SESSION_DIR")
}

/// This session: `BELAY_SESSION_DIR`, else the newest session started in this
/// directory, else the newest one started anywhere in this project.
fn session() -> Result<PathBuf> {
    if let Some(d) = session_env() {
        return Ok(d);
    }
    let cwd = std::env::current_dir()?;
    let project = project()?;
    let mut all = paths::list_sessions();
    all.reverse();
    let cwds: Vec<(PathBuf, String)> = all
        .into_iter()
        .filter_map(|d| sessions::session_cwd(&d).map(|c| (d, c)))
        .collect();
    let here = cwd.to_string_lossy();
    cwds.iter()
        .find(|(_, c)| *c == here)
        .or_else(|| {
            cwds.iter()
                .find(|(_, c)| Path::new(c).starts_with(&project))
        })
        .map(|(d, _)| d.clone())
        .ok_or_else(|| anyhow!("no belay session in this project"))
}

/// Most characters of a sandbox's output given back to the model.
const MAX_SANDBOX_OUTPUT: usize = 20_000;

/// `belay sandbox …` as a child with no terminal: the agent's rules apply (agents may
/// be refused, task limits count), as when it runs the command itself.
fn belay_sandbox(args: &[String]) -> Result<String> {
    let exe = std::env::current_exe()?;
    let out = std::process::Command::new(exe)
        .arg("sandbox")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = belay_core::text::truncate_chars(text.trim(), MAX_SANDBOX_OUTPUT);
    if !out.status.success() {
        bail!("{text}\n(exit {})", out.status.code().unwrap_or(1));
    }
    Ok(text)
}

fn opt_flag(args: &Value, key: &str, out: &mut Vec<String>) {
    if let Some(v) = args
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        out.extend([format!("--{key}"), v.to_string()]);
    }
}

fn sandbox_run(args: &Value) -> Result<String> {
    let argv = strings_arg(args, "argv");
    if argv.is_empty() {
        bail!("`argv` is required");
    }
    let mut a = vec!["run".to_string()];
    opt_flag(args, "id", &mut a);
    a.push("--".into());
    a.extend(argv);
    belay_sandbox(&a)
}

fn sandbox_task(args: &Value) -> Result<String> {
    let prompt = str_arg(args, "prompt")?;
    let mut a = vec!["task".to_string()];
    for k in ["id", "resume", "model"] {
        opt_flag(args, k, &mut a);
    }
    a.extend(["--".into(), prompt.into()]);
    belay_sandbox(&a)
}

fn output_show(dir: &Path, id: &str) -> Result<String> {
    let entries = store::read_entries(dir)?;
    let Some(hash) = report::find_output_blob(&entries, id) else {
        bail!("no output {id} in this session");
    };
    store::read_blob(dir, &hash)
}

fn why(dir: &Path, id: &str) -> Result<String> {
    let entries = store::read_entries(dir)?;
    report::why_across(&entries, id, &fleet::peer_entry)
}

fn brief(args: &Value) -> Result<String> {
    use crate::brief::Format;
    let format = match args.get("format").and_then(Value::as_str) {
        None | Some("text") => Format::Text,
        Some("markdown") => Format::Markdown,
        Some("json") => Format::Json,
        Some(f) => bail!("unknown format {f}: use text, markdown, or json"),
    };
    Ok(crate::brief::render(
        &crate::brief::of_session(&session()?)?,
        format,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(line: &str) -> Value {
        handle_line(line).expect("a reply")
    }

    #[test]
    fn initialize_echoes_the_clients_version_and_offers_tools() {
        let r = reply(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
        );
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        assert_eq!(r["result"]["serverInfo"]["name"], "belay");
    }

    #[test]
    fn notifications_get_no_reply_and_bad_lines_get_errors() {
        assert!(handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        assert!(handle_line(r#"{"jsonrpc":"2.0","method":"nope"}"#).is_none());
        assert!(handle_line(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#).is_none());
        assert_eq!(reply("{oops")["error"]["code"], PARSE_ERROR);
        assert_eq!(
            reply(r#"{"jsonrpc":"2.0","id":2,"method":"nope"}"#)["error"]["code"],
            METHOD_NOT_FOUND
        );
        let r = reply(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"rm"}}"#);
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn tools_are_listed_in_a_stable_order_with_schemas() {
        let r = reply(r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#);
        let names: Vec<&str> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "memory_search",
                "memory_add",
                "handoff_get",
                "output_show",
                "skill_preview",
                "skill_stage",
                "why",
                "brief",
                "sandbox_run",
                "sandbox_task"
            ]
        );
        assert_eq!(
            r["result"]["tools"][0]["inputSchema"]["required"][0],
            "query"
        );
    }

    #[test]
    fn a_missing_argument_is_a_tool_error_the_model_can_read() {
        let r = reply(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"memory_add","arguments":{}}}"#,
        );
        assert_eq!(r["result"]["isError"], true);
        assert_eq!(r["result"]["content"][0]["text"], "`text` is required");
    }
}
