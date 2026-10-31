//! `belay doctor`: check the install and the environment belay runs in (I6, U10).
//! Each line says ok, warn, or info. It changes nothing.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{config, install, paths, sessions};

/// The report, and whether everything that must work does.
pub fn run() -> (String, bool) {
    let mut out = String::new();
    let mut healthy = true;
    let mut line = |status: &str, text: String| {
        if status == "FAIL" {
            healthy = false;
        }
        let _ = writeln!(out, "{status:<5} {text}");
    };

    // The `belay` the agent will run (hooks call this binary by absolute path; the agent's
    // `belay show` goes through PATH).
    let exe = env::current_exe().ok();
    match (find_on_path("belay"), &exe) {
        (Some(p), Some(e)) if same_file(&p, e) => {
            line("ok", format!("belay {} at {}", env!("CARGO_PKG_VERSION"), e.display()))
        }
        (Some(p), Some(e)) => line(
            "warn",
            format!(
                "`belay` on PATH is {}, not this binary ({}); reinstall with `cargo install --path crates/belay-cli`",
                p.display(),
                e.display()
            ),
        ),
        (None, _) => line("warn", "`belay` is not on PATH".into()),
        _ => {}
    }

    let bin = env::var("BELAY_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    match version_of(&bin) {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .unwrap_or("?")
                .to_string();
            let checked = env!("BELAY_CONFORMANT").split(',').any(|c| c == v);
            if checked {
                line("ok", format!("Claude Code {v} (conformance checked)"));
            } else {
                line(
                    "warn",
                    format!(
                        "Claude Code {v} has no conformance record; run scripts/conformance.py"
                    ),
                );
            }
        }
        _ => line(
            "FAIL",
            format!("`{bin}` not found or failed (set BELAY_CLAUDE_BIN)"),
        ),
    }

    let home = paths::home();
    match fs::symlink_metadata(&home) {
        Err(_) => line(
            "info",
            format!("data dir {} does not exist yet", home.display()),
        ),
        Ok(m) if m.file_type().is_symlink() => line(
            "FAIL",
            format!(
                "data dir {} is a symlink; belay will not use it",
                home.display()
            ),
        ),
        Ok(m) if m.permissions().mode() & 0o077 != 0 => line(
            "warn",
            format!(
                "data dir {} is readable by others ({:o}); belay makes it 0700 on start",
                home.display(),
                m.permissions().mode() & 0o777
            ),
        ),
        Ok(_) => line("ok", format!("data dir {}", home.display())),
    }

    let cfg_path = config::path();
    let (cfg, warnings) = config::load_from(&cfg_path);
    if !cfg_path.exists() {
        line(
            "info",
            format!("no config at {} (defaults)", cfg_path.display()),
        );
    } else if warnings.is_empty() {
        let over = cfg.overrides();
        line(
            "ok",
            format!("config {} ({} values set)", cfg_path.display(), over.len()),
        );
    } else {
        for w in warnings {
            line("warn", format!("config {}: {w}", cfg_path.display()));
        }
    }

    let claude_dir = install::claude_dir();
    if install::plugin_installed(&claude_dir) {
        line(
            "ok",
            format!("Claude Code plugin {} installed", install::PLUGIN_ID),
        );
    } else {
        line(
            "info",
            "Claude Code plugin not installed (`belay install` adds it)".into(),
        );
    }
    let settings = fs::read_to_string(install::settings_path()).unwrap_or_default();
    match install::statusline_of(&settings) {
        Some((_, true)) => line("ok", "status line shows `belay status`".into()),
        Some((cmd, false)) => line(
            "info",
            format!("status line runs `{cmd}`, not belay's (`belay install --statusline --force` replaces it)"),
        ),
        None => line(
            "info",
            "no status line (`belay install --statusline` sets it to `belay status`)".into(),
        ),
    }

    let root = paths::sessions_dir();
    let all: Vec<PathBuf> = fs::read_dir(&root)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    let live = all.iter().filter(|d| sessions::is_live(d)).count();
    let bytes: u64 = all.iter().map(|d| dir_size(d)).sum();
    line(
        "info",
        format!(
            "{} sessions ({live} live), {:.1} MB; {}",
            all.len(),
            bytes as f64 / 1e6,
            retention(cfg.keep_days)
        ),
    );
    (out, healthy)
}

/// What `belay claude` does with old sessions. `keep_days = 0` keeps them all.
fn retention(keep_days: u64) -> String {
    if keep_days == 0 {
        "`belay claude` keeps old sessions (keep_days = 0)".into()
    } else {
        format!("`belay claude` deletes those older than keep_days = {keep_days}")
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn dir_size(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// `<bin> --version`, given at most 3 s: a stuck binary must not hang `belay doctor`.
fn version_of(bin: &str) -> std::io::Result<std::process::Output> {
    let mut child = Command::new(bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let start = std::time::Instant::now();
    while child.try_wait()?.is_none() {
        if start.elapsed() > std::time::Duration::from_secs(3) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "no answer in 3 s",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.wait_with_output()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_days_zero_is_not_described_as_deleting() {
        assert!(!retention(0).contains("deletes"));
        assert!(retention(30).contains("deletes those older than keep_days = 30"));
    }
}
