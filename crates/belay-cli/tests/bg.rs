//! `belay bg` end to end: the detached supervisor runs `belay claude` with a fake
//! headless agent, and the tests look at its state, notifications, and attach.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_belay")
}

struct Env {
    dir: PathBuf,
    home: PathBuf,
    agent: PathBuf,
    config: PathBuf,
}

/// A fake Claude Code. Headless (`-p`), it prints stream-json as `FAKE_MODE` says:
/// `done` waits for a `go` file, then ends; `limit` ends on a usage limit; `budget`
/// calls a tool again and again. Otherwise (an attach) it records its arguments.
fn setup(name: &str) -> Env {
    let dir = std::env::temp_dir().join(format!("belay-bg-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("work")).unwrap();
    let agent = dir.join("fake-claude.sh");
    let body = format!(
        r#"#!/bin/sh
[ "$1" = --version ] && echo '2.1.316 (Claude Code)' && exit 0
case " $* " in *" -p "*) ;; *) echo "$@" > '{d}/attach-args.txt'; exit 0 ;; esac
echo "$@" >> '{d}/args.txt'
belay hook SessionStart < '{start}' > /dev/null
echo '{{"type":"system","subtype":"init","session_id":"conv-1"}}'
case "$FAKE_MODE" in
  done)
    i=0; while [ ! -f '{d}/go' ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done
    echo '{{"type":"assistant","message":{{"id":"m1","content":[{{"type":"text","text":"done"}}],"usage":{{"input_tokens":10,"output_tokens":5}}}},"session_id":"conv-1"}}'
    echo '{{"type":"result","subtype":"success","is_error":false,"result":"All done.","session_id":"conv-1","total_cost_usd":0.01}}'
    ;;
  limit)
    echo '{{"type":"result","subtype":"success","is_error":true,"result":"You'"'"'ve hit your session limit · resets 11pm (UTC)","session_id":"conv-1"}}'
    exit 1
    ;;
  budget)
    i=0; while [ $i -lt 100 ]; do i=$((i+1))
      echo '{{"type":"assistant","message":{{"id":"m'$i'","content":[{{"type":"tool_use","id":"t'$i'","name":"Bash","input":{{}}}}],"usage":{{"input_tokens":500,"output_tokens":100}}}},"session_id":"conv-1"}}'
      sleep 0.1
      echo '{{"type":"user","message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"t'$i'","content":"ok"}}]}},"session_id":"conv-1"}}'
    done
    ;;
esac
"#,
        d = dir.display(),
        start = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/claude_code/session_start.json")
            .display(),
    );
    fs::write(&agent, body).unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
    let config = dir.join("config.toml");
    fs::write(
        &config,
        format!("notify_command = cat >> '{}/notes.jsonl'\n", dir.display()),
    )
    .unwrap();
    Env {
        home: dir.join("home"),
        dir,
        agent,
        config,
    }
}

impl Env {
    fn cmd(&self, mode: &str) -> Command {
        self.cmd_of(bin(), mode)
    }

    fn cmd_of(&self, program: &str, mode: &str) -> Command {
        let mut c = Command::new(program);
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("BELAY_") || k == "CLAUDECODE" {
                c.env_remove(k);
            }
        }
        c.current_dir(self.dir.join("work"))
            .env("BELAY_HOME", &self.home)
            .env("BELAY_CONFIG", &self.config)
            .env("BELAY_CLAUDE_BIN", &self.agent)
            .env("BELAY_TEST_TERMINAL", "1")
            .env("BELAY_TEST_PARENT", std::process::id().to_string())
            .env("FAKE_MODE", mode);
        c
    }

    fn belay(&self, args: &[&str]) -> String {
        let out = self.cmd("").args(args).output().unwrap();
        assert!(
            ok(&out),
            "belay {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn start(&self, mode: &str, flags: &[&str]) -> String {
        let out = self
            .cmd(mode)
            .args(["bg", "start", "--here", "--no-keep-awake"])
            .args(flags)
            .arg("fix the thing")
            .output()
            .unwrap();
        assert!(ok(&out), "{}", String::from_utf8_lossy(&out.stderr));
        self.only_id()
    }

    fn only_id(&self) -> String {
        let mut ids: Vec<String> = fs::read_dir(self.home.join("bg"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(ids.len(), 1, "{ids:?}");
        ids.pop().unwrap()
    }

    fn state(&self, id: &str) -> serde_json::Value {
        fs::read_to_string(self.home.join("bg").join(id).join("state.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Wait for the state to satisfy `f`.
    fn wait(
        &self,
        id: &str,
        what: &str,
        f: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let s = self.state(id);
            if f(&s) {
                return s;
            }
            assert!(
                Instant::now() < deadline,
                "never {what}: {s}\nrunner.log: {}",
                fs::read_to_string(self.home.join("bg").join(id).join("runner.log"))
                    .unwrap_or_default()
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn notes(&self) -> Vec<serde_json::Value> {
        fs::read_to_string(self.dir.join("notes.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Wait for a notification of `kind`.
    fn note(&self, kind: &str) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(n) = self.notes().into_iter().find(|n| n["kind"] == kind) {
                return n;
            }
            assert!(
                Instant::now() < deadline,
                "no {kind} notification: {:?}",
                self.notes()
            );
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn ok(out: &Output) -> bool {
    out.status.success()
}

fn runner_gone(e: &Env, id: &str) -> bool {
    e.state(id)["runner"].is_null()
}

/// The user starts it and leaves: the shell that started it is killed, the run goes
/// on, ends, and says so with the brief. Then attach opens the conversation.
#[test]
fn a_background_session_outlives_its_shell_and_attach_resumes_it() {
    let e = setup("done");
    // A shell that starts it and stays, as the user's terminal would.
    let mut shell = e
        .cmd_of("sh", "done")
        .arg("-c")
        .arg(format!(
            "'{}' bg start --here --no-keep-awake 'fix the thing'; sleep 60",
            bin()
        ))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while fs::read_dir(e.home.join("bg")).map_or(true, |mut d| d.next().is_none()) {
        assert!(Instant::now() < deadline, "bg start made nothing");
        thread::sleep(Duration::from_millis(50));
    }
    let id = e.only_id();
    let s = e.wait(&id, "working with a conversation", |s| {
        s["phase"] == "working" && s["conversation"] == "conv-1"
    });
    // The starting process and its group are gone; the supervisor is not.
    let runner = s["runner"].as_i64().unwrap() as i32;
    unsafe { libc::kill(-(shell.id() as i32), libc::SIGKILL) };
    shell.wait().unwrap();
    assert_eq!(
        unsafe { libc::kill(runner, 0) },
        0,
        "the supervisor died with its shell"
    );
    assert_ne!(
        unsafe { libc::getpgid(runner) },
        shell.id() as i32,
        "the supervisor is in its starter's group"
    );
    let status = e.belay(&["bg", "status", &id]);
    assert!(status.contains(&format!("{id} · working")), "{status}");
    assert!(status.contains("conversation: conv-1"), "{status}");

    fs::write(e.dir.join("go"), "").unwrap();
    let s = e.wait(&id, "done", |s| {
        s["phase"] == "done" && s["runner"].is_null()
    });
    assert_eq!(s["tokens"], 15);
    assert_eq!(s["result"], "All done.");
    let done = e.note("done");
    let text = done["text"].as_str().unwrap();
    assert!(text.starts_with("Done.\nChanged: "), "{text}");
    assert!(text.contains("\nChecked: "), "{text}");
    assert_eq!(done["bg"], id.as_str());
    let args = fs::read_to_string(e.dir.join("args.txt")).unwrap();
    assert!(args.contains("--permission-mode auto"), "{args}");
    assert!(args.trim_end().ends_with("-- fix the thing"), "{args}");
    let status = e.belay(&["bg", "status", &id]);
    assert!(status.contains(&format!("{id} · done")), "{status}");
    assert!(e.belay(&["bg", "list"]).contains(&format!("{id}  done")));

    // Attach on a finished run: the launcher starts the agent on its conversation.
    let out = e.cmd("").args(["bg", "attach", &id]).output().unwrap();
    assert!(ok(&out), "{}", String::from_utf8_lossy(&out.stderr));
    let args = fs::read_to_string(e.dir.join("attach-args.txt")).unwrap();
    assert!(args.contains("--resume conv-1"), "{args}");
    let _ = fs::remove_dir_all(&e.dir);
}

/// A usage limit: the run waits for the reset, says until when, and tells the user.
/// A stop during the wait ends it.
#[test]
fn a_usage_limit_waits_for_the_reset_and_says_until_when() {
    let e = setup("limit");
    let id = e.start("limit", &[]);
    e.wait(&id, "waiting", |s| s["phase"] == "waiting");
    let status = e.belay(&["bg", "status", &id]);
    assert!(
        status.contains("waiting for the usage limit to reset, until "),
        "{status}"
    );
    let n = e.note("usage_limit");
    assert!(
        n["text"].as_str().unwrap().contains("goes on by itself at"),
        "{n}"
    );

    // Only the user's terminal stops it.
    let out = e
        .cmd("")
        .env_remove("BELAY_TEST_TERMINAL")
        .args(["bg", "stop", &id])
        .output()
        .unwrap();
    assert!(!ok(&out));
    assert!(String::from_utf8_lossy(&out.stderr).contains("is the user's"));
    let said = e.belay(&["bg", "stop", &id]);
    assert!(said.contains("stops at the next step"), "{said}");
    let s = e.wait(&id, "stopped", |s| {
        s["phase"] == "stopped" && runner_gone(&e, &id)
    });
    assert_eq!(s["reason"], "the user stopped it");
    let _ = fs::remove_dir_all(&e.dir);
}

/// Past its token budget, the run stops at the next tool result, and says so.
#[test]
fn a_budget_stops_the_run_at_a_step_and_says_so() {
    let e = setup("budget");
    let id = e.start("budget", &["--budget", "1500"]);
    let s = e.wait(&id, "stopped", |s| {
        s["phase"] == "stopped" && runner_gone(&e, &id)
    });
    let reason = s["reason"].as_str().unwrap();
    assert!(reason.contains("past the budget of 1500"), "{reason}");
    // Three calls (1800 tokens) cross it; it stopped at that step, not many after.
    let used = s["tokens"].as_u64().unwrap();
    assert!((1800..=2400).contains(&used), "{used}");
    let n = e.note("budget");
    assert!(
        n["text"].as_str().unwrap().contains("belay bg attach"),
        "{n}"
    );
    let status = e.belay(&["bg", "status", &id]);
    assert!(status.contains("stopped: it used"), "{status}");
    let _ = fs::remove_dir_all(&e.dir);
}
