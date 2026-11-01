//! End-to-end: `belay sandbox` with bubblewrap. Each test is skipped, with the reason
//! printed, where bwrap or user namespaces are not available.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    hermetic();
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
        "outside the worktree",
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
