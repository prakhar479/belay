//! `belay mcp` over stdio: a client's session from initialize to tool calls.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next: u64,
}

impl Server {
    fn start(name: &str) -> (Self, PathBuf) {
        let dir = std::env::temp_dir().join(format!("belay-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cwd = dir.join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let session = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/sessions/mcp");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_belay"));
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("BELAY_") || k == "CLAUDECODE" {
                cmd.env_remove(k);
            }
        }
        let mut child = cmd
            .arg("mcp")
            .current_dir(&cwd)
            .env("BELAY_HOME", dir.join("home"))
            .env("BELAY_CONFIG", "/nonexistent/belay-mcp/config.toml")
            .env("BELAY_SESSION_DIR", session)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        (
            Server {
                child,
                stdin,
                stdout,
                next: 0,
            },
            dir,
        )
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], id, "{reply}");
        reply
    }

    /// A tool's text, and whether it is an error.
    fn tool(&mut self, name: &str, args: Value) -> (String, bool) {
        let r = self.request("tools/call", json!({"name": name, "arguments": args}));
        let res = &r["result"];
        (
            res["content"][0]["text"].as_str().unwrap().to_string(),
            res["isError"].as_bool().unwrap(),
        )
    }
}

#[test]
fn a_client_lists_tools_adds_and_finds_a_memory_and_reads_the_ledger() {
    let (mut s, dir) = Server::start("flow");
    let init = s.request(
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    let list = s.request("tools/list", json!({}));
    let names: Vec<&str> = list["result"]["tools"]
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
            "sandbox_run",
            "sandbox_task"
        ]
    );

    let (said, err) = s.tool(
        "memory_add",
        json!({"text": "The api tests need DB_URL set", "kind": "gotcha", "tags": ["pytest"]}),
    );
    assert!(
        !err && said.starts_with("Kept m-1 as the agent's"),
        "{said}"
    );
    let (found, err) = s.tool("memory_search", json!({"query": "DB_URL", "limit": 3}));
    assert!(!err, "{found}");
    assert!(
        found.contains("m-1 · gotcha · this project · by agent"),
        "{found}"
    );
    assert!(found.contains("The api tests need DB_URL set"), "{found}");

    let (out, err) = s.tool("output_show", json!({"id": "o-1"}));
    assert!(
        !err && out.contains("test_b FAILED: assert 1 == 2"),
        "{out}"
    );
    let (missing, err) = s.tool("output_show", json!({"id": "o-9"}));
    assert!(err && missing.contains("no output o-9"), "{missing}");

    let (chain, err) = s.tool("why", json!({"id": "a-1"}));
    assert!(!err, "{chain}");
    assert!(chain.contains("action a-1 (shape, applied)"), "{chain}");
    assert!(chain.contains("caused by #2 output o-1"), "{chain}");

    let (handoff, err) = s.tool("handoff_get", json!({}));
    assert!(!err && handoff.starts_with("No handoff"), "{handoff}");

    drop(s.stdin);
    assert!(s.child.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(dir);
}
