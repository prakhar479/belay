//! End-to-end: `belay sandbox` with bubblewrap. Each test is skipped, with the reason
//! printed, where bwrap or user namespaces are not available.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    hermetic();
    common::no_desktop();
    env!("CARGO_BIN_EXE_belay")
}

/// No `BELAY_*` from the shell that runs the tests (a belay session, a fleet worker).
fn hermetic() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("BELAY_") || k == "CLAUDECODE" {
                std::env::remove_var(k);
            }
        }
        std::env::set_var("BELAY_CONFIG", "/nonexistent/belay-e2e/config.toml");
    });
}

/// Why sandboxes cannot run here, or None.
fn unavailable() -> Option<String> {
    let out = Command::new("bwrap")
        .args([
            "--unshare-user",
            "--unshare-pid",
            "--ro-bind",
            "/",
            "/",
            "true",
        ])
        .output();
    match out {
        Err(e) => Some(format!("no bwrap: {e}")),
        Ok(o) if !o.status.success() => Some(format!(
            "bwrap cannot make user namespaces here: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Ok(_) => None,
    }
}

macro_rules! need_sandbox {
    () => {
        if let Some(why) = unavailable() {
            eprintln!("skipped: {why}");
            return;
        }
    };
}

fn tmp(name: &str) -> PathBuf {
    hermetic();
    let d = std::env::temp_dir().join(format!("belay-sb-{name}-{}", std::process::id()));
    let _ = Command::new("chmod")
        .arg("-R")
        .arg("u+rwX")
        .arg(&d)
        .output();
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A repo with one commit, a fake real home, belay's home, and a config file.
struct World {
    dir: PathBuf,
    repo: PathBuf,
    home: PathBuf,
    belay_home: PathBuf,
    config: PathBuf,
}

fn world(name: &str, config_text: &str) -> World {
    let dir = tmp(name);
    let repo = dir.join("repo");
    let home = dir.join("realhome");
    fs::create_dir_all(repo.join("sub")).unwrap();
    fs::create_dir_all(home.join(".ssh")).unwrap();
    fs::write(home.join(".ssh/id_ed25519"), "secret\n").unwrap();
    fs::write(repo.join("sub/a.txt"), "a\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    let config = dir.join("config.toml");
    fs::write(&config, config_text).unwrap();
    World {
        belay_home: dir.join("belay"),
        dir,
        repo,
        home,
        config,
    }
}

fn sandbox(w: &World, cwd: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .arg("sandbox")
        .args(args)
        .current_dir(cwd)
        .env("HOME", &w.home)
        .env("BELAY_HOME", &w.belay_home)
        .env("BELAY_CONFIG", &w.config)
        .env("REAL_HOME_PATH", &w.home)
        .output()
        .unwrap()
}

fn ids(w: &World) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(w.belay_home.join("sandboxes"))
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

#[test]
fn sandbox_run_keeps_changes_on_its_branch_and_nothing_escapes() {
    need_sandbox!();
    let w = world("run", "");
    let out_file = format!("/tmp/belay-sb-out-{}", std::process::id());
    let script = format!(
        "echo x > in.txt; echo y > {out_file}; echo z > $REAL_HOME_PATH/escape; \
         cat $REAL_HOME_PATH/.ssh/id_ed25519; test \"$BELAY_SANDBOX\" != '' && pwd"
    );
    let o = sandbox(&w, &w.repo.join("sub"), &["run", "--", "sh", "-c", &script]);
    let stdout = String::from_utf8_lossy(&o.stdout);
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{stdout}\n{stderr}");
    assert!(!stdout.contains("secret"), "{stdout}");
    // It ran in the worktree, in the same subdirectory.
    assert!(stdout.trim().ends_with("/work/sub"), "{stdout}");
    assert!(!w.home.join("escape").exists());
    assert!(!Path::new(&out_file).exists());
    assert!(!w.repo.join("sub/in.txt").exists());
    let id = ids(&w).pop().expect("a sandbox record");
    let branch = format!("belay/sandbox/{id}");
    assert_eq!(
        git(&w.repo, &["show", &format!("{branch}:sub/in.txt")]),
        "x\n"
    );
    let log = git(&w.repo, &["log", "-1", "--format=%s", &branch]);
    assert!(log.starts_with(&format!("sandbox {id}: sh -c")), "{log}");
    // Without --keep the worktree is gone; the branch and the record stay.
    let list = sandbox(&w, &w.repo, &["list"]);
    let list = String::from_utf8_lossy(&list.stdout);
    assert!(list.contains(&id) && list.contains("removed"), "{list}");
    let show = sandbox(&w, &w.repo, &["show", &id]);
    let show = String::from_utf8_lossy(&show.stdout);
    assert!(show.contains(&format!("sandbox {id}:")), "{show}");
    // Nesting is refused.
    let nested = Command::new(bin())
        .args(["sandbox", "list"])
        .env("BELAY_SANDBOX", "x")
        .output()
        .unwrap();
    assert!(!nested.status.success());
    // `rm` is the user's.
    let rm = sandbox(&w, &w.repo, &["rm", &id]);
    assert!(!rm.status.success());
    let rm = Command::new(bin())
        .args(["sandbox", "rm", &id, "--branch"])
        .current_dir(&w.repo)
        .env("BELAY_HOME", &w.belay_home)
        .env("BELAY_TEST_TERMINAL", "1")
        .output()
        .unwrap();
    assert!(
        rm.status.success(),
        "{}",
        String::from_utf8_lossy(&rm.stderr)
    );
    assert!(ids(&w).is_empty());
    assert!(git(&w.repo, &["branch", "--list", &branch])
        .trim()
        .is_empty());
    let _ = fs::remove_dir_all(&w.dir);
}

#[test]
fn the_watchdog_stops_a_memory_hog_and_records_why() {
    need_sandbox!();
    let w = world("hog", "sandbox_memory = 64M\n");
    let o = sandbox(
        &w,
        &w.repo,
        &[
            "run",
            "--id",
            "hog",
            "--keep",
            "--",
            "sh",
            "-c",
            "head -c 4000M /dev/zero | tail",
        ],
    );
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success(), "{stderr}");
    assert!(stderr.contains("sandbox_memory"), "{stderr}");
    let events =
        fs::read_to_string(w.belay_home.join("sandboxes/hog/events.jsonl")).unwrap_or_default();
    assert!(
        events.contains("\"limit\"") && events.contains("64M"),
        "{events}"
    );
    let show = sandbox(&w, &w.repo, &["show", "hog"]);
    let show = String::from_utf8_lossy(&show.stdout);
    assert!(
        show.contains("kept") && show.contains("sandbox_memory"),
        "{show}"
    );
    // Reused by id: the kept worktree is there.
    let o = sandbox(
        &w,
        &w.repo,
        &["run", "--id", "hog", "--", "sh", "-c", "echo again > b.txt"],
    );
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(
        git(&w.repo, &["show", "belay/sandbox/hog:b.txt"]),
        "again\n"
    );
}

#[test]
fn a_sandbox_outside_git_or_with_agents_off_is_refused() {
    need_sandbox!();
    let w = world("refuse", "sandbox_agent = off\n");
    let plain = w.dir.join("plain");
    fs::create_dir_all(&plain).unwrap();
    let o = sandbox(&w, &w.repo, &["run", "--", "touch", "x"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("sandbox_agent = off"));
    fs::write(&w.config, "").unwrap();
    let o = sandbox(&w, &plain, &["run", "--", "touch", "x"]);
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stderr).contains("git repo"));
    assert!(!plain.join("x").exists());
}

#[test]
fn doctor_finds_every_probe_contained() {
    need_sandbox!();
    let w = world("doctor", "");
    let o = sandbox(&w, &w.dir, &["doctor"]);
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(
        o.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(!stdout.contains("ESCAPED"), "{stdout}");
    for probe in [
        "outside the sandbox",
        "through the sandbox's .git",
        "~/.ssh",
        "host process",
        "docker.sock",
        "fork bomb",
        "memory hog",
    ] {
        assert!(stdout.contains(probe), "{probe}: {stdout}");
    }
    // It cleans up after itself.
    assert!(ids(&w).is_empty(), "{:?}", ids(&w));
}

#[test]
fn a_sandbox_run_from_a_session_is_recorded_there() {
    need_sandbox!();
    let w = world("session", "");
    let agent = w.dir.join("fake-claude.sh");
    fs::write(
        &agent,
        "#!/bin/sh\n[ \"$1\" = --version ] && echo '2.1.304 (Claude Code)' && exit 0\n\
         belay sandbox run --id fromsession -- sh -c 'echo s > s.txt'\n",
    )
    .unwrap();
    fs::set_permissions(&agent, fs::Permissions::from_mode(0o755)).unwrap();
    let o = Command::new(bin())
        .args(["claude", "--resume", "abc"])
        .current_dir(&w.repo)
        .env("HOME", &w.home)
        .env("BELAY_HOME", &w.belay_home)
        .env("BELAY_CONFIG", &w.config)
        .env("BELAY_CLAUDE_BIN", &agent)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let session = fs::read_dir(w.belay_home.join("sessions"))
        .unwrap()
        .flatten()
        .next()
        .unwrap()
        .path();
    let ledger = fs::read_to_string(session.join("ledger.jsonl")).unwrap();
    assert!(
        ledger.contains("\"sandbox_started\"") && ledger.contains("belay/sandbox/fromsession"),
        "{ledger}"
    );
    assert!(
        ledger.contains("\"sandbox_stopped\"") && ledger.contains("exited with 0"),
        "{ledger}"
    );
}

fn script(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// The headless fake agent: edits a file, runs git inside, plants hooks and config in
/// the clone's `.git` that would write `marker` if they ran on the host, then prints
/// stream-json with its usage and final message.
fn fake_headless(w: &World, marker: &Path) -> PathBuf {
    let agent = w.dir.join("fake-headless.sh");
    script(
        &agent,
        &format!(
            r#"#!/bin/sh
[ "$1" = --version ] && echo '2.1.304 (Claude Code)' && exit 0
case "$*" in *--fork-session*) ls "$HOME"/.claude/projects/*/ > resumed.txt 2>&1;; esac
echo changed > sub/a.txt
git status --short > status.txt 2>&1 && echo ok > gitok.txt
printf '#!/bin/sh\necho "$0" >> {marker}\ncat\n' > .git/evil; chmod +x .git/evil
for h in pre-commit post-commit commit-msg reference-transaction post-index-change; do
  cp .git/evil .git/hooks/$h
done
E=$PWD/.git/evil
for k in core.fsmonitor filter.p.clean uploadpack.packObjectsHook core.alternateRefsCommand gpg.program; do
  git config $k "$E"
done
git config commit.gpgSign true
git config core.hooksPath "$PWD/.git/hooks"
echo '* filter=p' > .gitattributes
echo '{{"type":"assistant","message":{{"id":"m1","usage":{{"input_tokens":3000,"output_tokens":2000}}}}}}'
echo '{{"type":"assistant","message":{{"id":"m1","usage":{{"input_tokens":3000,"output_tokens":2000}}}}}}'
[ -n "$BELAY_TEST_SLOW" ] && sleep 30
echo '{{"type":"result","result":"edited sub/a.txt"}}'
"#,
            marker = marker.display()
        ),
    );
    agent
}

fn task(w: &World, agent: &Path, args: &[&str], extra: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(bin());
    cmd.args(["sandbox", "task"])
        .args(args)
        .current_dir(&w.repo)
        .env("HOME", &w.home)
        .env("BELAY_HOME", &w.belay_home)
        .env("BELAY_CONFIG", &w.config)
        .env("BELAY_CLAUDE_BIN", agent)
        .stdin(std::process::Stdio::null());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

#[test]
fn a_sandbox_task_runs_git_inside_and_lands_its_branch_without_running_the_clones_hooks() {
    need_sandbox!();
    let w = world("task", "sandbox_tasks_per_hour = 2\n");
    let marker = w.dir.join("marker");
    let agent = fake_headless(&w, &marker);
    // A conversation of the user's to follow up on.
    let conv = "0b1d6f2e-0000-4000-8000-00000000c0de";
    let project = w.home.join(".claude/projects/-elsewhere");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join(format!("{conv}.jsonl")), "{}\n").unwrap();
    let o = task(
        &w,
        &agent,
        &[
            "--id",
            "t1",
            "--resume",
            conv,
            "--",
            "-make a.txt say changed",
        ],
        &[],
    );
    let stdout = String::from_utf8_lossy(&o.stdout);
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("result: edited sub/a.txt"), "{stdout}");
    assert!(stdout.contains("branch: belay/sandbox/t1 ("), "{stdout}");
    assert!(stdout.contains("sub/a.txt"), "{stdout}");
    assert!(stdout.contains("audit:"), "{stdout}");
    assert!(stdout.contains("tokens: 5000 of 2M"), "{stdout}");
    assert!(stdout.contains("limits: none reached"), "{stdout}");
    // The work is on the branch in the main repo; the main checkout is untouched.
    let branch = "belay/sandbox/t1";
    assert_eq!(
        git(&w.repo, &["show", &format!("{branch}:sub/a.txt")]),
        "changed\n"
    );
    assert_eq!(
        git(&w.repo, &["show", &format!("{branch}:gitok.txt")]),
        "ok\n"
    );
    let status = git(&w.repo, &["show", &format!("{branch}:status.txt")]);
    assert!(
        status.contains("sub/a.txt"),
        "git status ran inside: {status}"
    );
    let resumed = git(&w.repo, &["show", &format!("{branch}:resumed.txt")]);
    assert!(
        resumed.contains(conv),
        "the conversation was copied in: {resumed}"
    );
    assert_eq!(fs::read_to_string(w.repo.join("sub/a.txt")).unwrap(), "a\n");
    // Nothing planted in the clone ran on the host: not during the commit, not during
    // the fetch.
    assert!(
        !marker.exists(),
        "ran on the host: {}",
        fs::read_to_string(&marker).unwrap_or_default()
    );
    // Without --keep the clone is gone; the branch stays.
    assert!(!w.belay_home.join("sandboxes/t1/work").exists());

    // A second task by an agent goes over its budget and is stopped.
    fs::write(
        &w.config,
        "sandbox_tasks_per_hour = 2\nsandbox_task_tokens = 1K\n",
    )
    .unwrap();
    let o = task(
        &w,
        &agent,
        &["--id", "t2", "go"],
        &[("BELAY_TEST_SLOW", "1")],
    );
    let stdout = String::from_utf8_lossy(&o.stdout);
    assert!(!o.status.success(), "{stdout}");
    assert!(stdout.contains("limit: sandbox_task_tokens"), "{stdout}");
    // A third, within the hour, is refused.
    let o = task(&w, &agent, &["go"], &[]);
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(!o.status.success());
    assert!(stderr.contains("sandbox_tasks_per_hour"), "{stderr}");
    let _ = fs::remove_dir_all(&w.dir);
}

#[test]
fn a_whole_session_in_a_sandbox_reaches_the_daemon_outside() {
    need_sandbox!();
    let w = world("claude", "");
    let work = w.belay_home.join("sandboxes/box/work");
    let pre = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/claude_code/pre_tool_use_edit_test.json"),
    )
    .unwrap();
    let mut pre: serde_json::Value = serde_json::from_str(&pre).unwrap();
    pre["cwd"] = work.display().to_string().into();
    pre["tool_input"]["file_path"] = work.join("tests/test_api.py").display().to_string().into();
    let agent = w.dir.join("fake-claude.sh");
    script(
        &agent,
        &format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && echo '2.1.304 (Claude Code)' && exit 0\n\
             cat > /tmp/pre.json <<'EOF'\n{pre}\nEOF\n\
             belay hook PreToolUse < /tmp/pre.json > deny.out\n\
             sleep 0.3; cat \"$BELAY_SESSION_DIR/card.json\" > card.txt\n\
             git status --short > status.txt 2>&1\n\
             echo \"$*\" > args.txt\n"
        ),
    );
    let o = Command::new(bin())
        .args(["sandbox", "claude", "--id", "box", "--model", "x"])
        .current_dir(&w.repo)
        .env("HOME", &w.home)
        .env("BELAY_HOME", &w.belay_home)
        .env("BELAY_CONFIG", &w.config)
        .env("BELAY_CLAUDE_BIN", &agent)
        .env("BELAY_TEST_TERMINAL", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(o.status.success(), "{stderr}");
    let branch = "belay/sandbox/box";
    let show = |f: &str| git(&w.repo, &["show", &format!("{branch}:{f}")]);
    // The guard outside denied the edit the agent inside asked about.
    let deny = show("deny.out");
    assert!(deny.contains("\"deny\""), "{deny}");
    let card: serde_json::Value = serde_json::from_str(&show("card.txt")).unwrap();
    assert_eq!(card["sandbox"], "box", "{card}");
    assert!(show("args.txt").contains("--settings") && show("args.txt").contains("--model x"));
    assert!(stderr.contains("`belay sandbox show box`"), "{stderr}");
    // The session's ledger is outside, and has the sandbox.
    let session = fs::read_dir(w.belay_home.join("sessions"))
        .unwrap()
        .flatten()
        .find(|e| e.path().join("ledger.jsonl").exists())
        .unwrap()
        .path();
    let ledger = fs::read_to_string(session.join("ledger.jsonl")).unwrap();
    assert!(ledger.contains("\"sandbox_started\""), "{ledger}");
    // The sandbox is kept for a resume, which goes on in it with the session's state.
    assert!(work.exists());
    let conv = pre["session_id"].as_str().unwrap();
    let o = Command::new(bin())
        .args(["sandbox", "claude", "--id", "box", "--resume", conv])
        .current_dir(&w.repo)
        .env("HOME", &w.home)
        .env("BELAY_HOME", &w.belay_home)
        .env("BELAY_CONFIG", &w.config)
        .env("BELAY_CLAUDE_BIN", &agent)
        .env("BELAY_TEST_TERMINAL", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(show("args.txt").contains(&format!("--resume {conv}")));
    let resumed = fs::read_dir(w.belay_home.join("sessions"))
        .unwrap()
        .flatten()
        .filter_map(|e| fs::read_to_string(e.path().join("ledger.jsonl")).ok())
        .any(|l| l.contains("\"resumed\""));
    assert!(
        resumed,
        "the second run continued the first one's belay state"
    );
    // `belay sandbox claude` is the user's.
    let o = Command::new(bin())
        .args(["sandbox", "claude", "--id", "box"])
        .current_dir(&w.repo)
        .env("BELAY_HOME", &w.belay_home)
        .env("BELAY_CLAUDE_BIN", &agent)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(!o.status.success());
    let _ = fs::remove_dir_all(&w.dir);
}

/// `belay bg start --sandbox`: the detached run works in a sandbox, its work lands on
/// the sandbox's branch, and the user's checkout is untouched.
#[test]
fn a_background_session_in_a_sandbox_lands_its_branch() {
    need_sandbox!();
    let w = world("bg", "");
    let agent = w.dir.join("fake-bg.sh");
    script(
        &agent,
        r#"#!/bin/sh
[ "$1" = --version ] && echo '2.1.316 (Claude Code)' && exit 0
echo changed > sub/a.txt
echo '{"type":"system","subtype":"init","session_id":"conv-bg"}'
echo '{"type":"assistant","message":{"id":"m1","usage":{"input_tokens":30,"output_tokens":20}}}'
echo '{"type":"result","result":"edited sub/a.txt","session_id":"conv-bg"}'
"#,
    );
    let out = Command::new(bin())
        .args([
            "bg",
            "start",
            "--sandbox",
            "--no-keep-awake",
            "change a.txt",
        ])
        .current_dir(&w.repo)
        .env("HOME", &w.home)
        .env("BELAY_HOME", &w.belay_home)
        .env("BELAY_CONFIG", &w.config)
        .env("BELAY_CLAUDE_BIN", &agent)
        .env("BELAY_TEST_TERMINAL", "1")
        .env("BELAY_TEST_PARENT", std::process::id().to_string())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = stdout
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':')
        .to_string();
    assert!(stdout.contains(&format!("in sandbox {id}")), "{stdout}");
    let state = w.belay_home.join("bg").join(&id).join("state.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let s = loop {
        let s: serde_json::Value = fs::read_to_string(&state)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        if s["runner"].is_null() && s["phase"] != "starting" && s["phase"] != "working" {
            break s;
        }
        assert!(std::time::Instant::now() < deadline, "never ended: {s}");
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    assert_eq!(s["phase"], "done", "{s}");
    assert_eq!(s["conversation"], "conv-bg");
    assert_eq!(s["tokens"], 50);
    let branch = format!("belay/sandbox/{id}");
    assert_eq!(
        git(&w.repo, &["show", &format!("{branch}:sub/a.txt")]),
        "changed\n"
    );
    assert_eq!(fs::read_to_string(w.repo.join("sub/a.txt")).unwrap(), "a\n");
}
