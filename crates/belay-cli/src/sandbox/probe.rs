//! `belay sandbox doctor`: escape probes in a fresh sandbox of a scratch repo. Each must
//! be contained; one that is not makes the doctor exit 1.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use super::{backend, create, exec, layout, remove_tree, remove_work, Backend, Io, Limits, Meta};
use crate::{audit, config, paths};

/// What one probe found.
enum Found {
    Contained(String),
    Escaped(String),
    /// Not on this machine (the WSL probes elsewhere).
    Skipped(String),
}

struct Ctx<'a> {
    meta: &'a Meta,
    backend: &'a dyn Backend,
    cfg: config::Config,
    files: crate::connector::SandboxFiles,
    limits: Limits,
}

impl Ctx<'_> {
    fn run(&self, script: &str, args: &[&str], lim: Limits) -> Result<super::Outcome> {
        let mut argv = vec!["sh".to_string(), "-c".into(), script.into(), "sh".into()];
        argv.extend(args.iter().map(|s| s.to_string()));
        let spec = layout(self.meta, &self.cfg, &self.files, Path::new(""), None, argv)?;
        exec(self.meta, self.backend, &spec, lim, Io::Capture)
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim().to_string()
}

pub fn doctor() -> Result<i32> {
    let backend = backend()?;
    let pid = std::process::id();
    let scratch = super::root().join(format!(".doctor-{pid}"));
    let result = (|| -> Result<Vec<(&'static str, Found)>> {
        let repo = scratch.join("repo");
        fs::create_dir_all(&repo)?;
        paths::ensure_private_dir(&scratch)?;
        fs::write(repo.join("README"), "belay sandbox doctor\n")?;
        for args in [
            &["init", "--quiet"][..],
            &["add", "README"],
            &[
                "-c",
                "user.name=belay",
                "-c",
                "user.email=belay@localhost",
                "commit",
                "--quiet",
                "-m",
                "doctor",
            ],
        ] {
            audit::git(&repo, args, None).context("make a scratch repo")?;
        }
        let meta = create(&repo, &format!("doctor-{pid}"))?;
        let cfg = config::load();
        let ctx = Ctx {
            meta: &meta,
            backend: &*backend,
            limits: Limits {
                memory: 256 << 20,
                procs: 64,
                disk: cfg.sandbox_disk,
                wall: Duration::from_secs(60),
            },
            files: crate::connector::for_agent("claude-code")?.sandbox_files(),
            cfg,
        };
        let found = probes(&ctx, &scratch);
        let _ = remove_work(&meta);
        let _ = remove_tree(&meta.dir());
        let _ = audit::git(&repo, &["branch", "-D", &meta.branch], None);
        found
    })();
    let _ = remove_tree(&scratch);
    let found = result?;
    println!(
        "belay sandbox doctor: {} backend, {} probes",
        backend.name(),
        found.len()
    );
    let mut escaped = false;
    for (name, f) in &found {
        let (word, why) = match f {
            Found::Contained(w) => ("contained", w),
            Found::Escaped(w) => {
                escaped = true;
                ("ESCAPED", w)
            }
            Found::Skipped(w) => ("skipped", w),
        };
        println!("  {word:<9}  {name}: {why}");
    }
    if escaped {
        println!("A probe escaped: do not trust sandboxes on this machine. See docs/sandbox.md.");
    }
    Ok(i32::from(escaped))
}

pub fn wsl() -> bool {
    Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists()
        || fs::read_to_string("/proc/version").is_ok_and(|v| v.to_lowercase().contains("microsoft"))
}

fn probes(ctx: &Ctx, scratch: &Path) -> Result<Vec<(&'static str, Found)>> {
    let mut out = Vec::new();
    let home = paths::env_path("HOME");
    let lim = ctx.limits;

    // A file outside the worktree: next to it on the host, in the real home, in /tmp.
    let outside = scratch.join("outside");
    fs::create_dir_all(&outside)?;
    let mut targets: Vec<PathBuf> = vec![
        outside.join("escape"),
        std::env::temp_dir().join(format!("belay-sandbox-escape-{}", std::process::id())),
    ];
    if let Some(h) = &home {
        targets.push(h.join(".belay-sandbox-escape"));
    }
    let args: Vec<String> = targets
        .iter()
        .map(|t| t.to_string_lossy().into_owned())
        .collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    ctx.run(
        r#"for f; do echo x > "$f"; done 2>/dev/null; true"#,
        &args,
        lim,
    )?;
    let written: Vec<String> = targets
        .iter()
        .filter(|t| t.exists())
        .map(|t| t.display().to_string())
        .collect();
    for t in &targets {
        let _ = fs::remove_file(t);
    }
    out.push((
        "write a file outside the sandbox",
        if written.is_empty() {
            Found::Contained("no file appeared on the host".into())
        } else {
            Found::Escaped(format!("wrote {}", written.join(", ")))
        },
    ));

    // Home secrets.
    let secrets: Vec<String> = home
        .iter()
        .flat_map(|h| [".ssh", ".aws", ".config/gcloud", ".gnupg", ".netrc"].map(|s| h.join(s)))
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let args: Vec<&str> = secrets.iter().map(String::as_str).collect();
    let o = ctx.run(
        r#"for p; do [ -e "$p" ] && echo "$p"; done; true"#,
        &args,
        lim,
    )?;
    let seen = text(&o.stdout);
    out.push((
        "read ~/.ssh and cloud credentials",
        if seen.is_empty() {
            Found::Contained("none of them is visible".into())
        } else {
            Found::Escaped(format!("visible: {}", seen.replace('\n', ", ")))
        },
    ));

    // A host process: the shell that ran belay.
    // SAFETY: getppid has no preconditions.
    let parent = unsafe { libc::getppid() }.to_string();
    let o = ctx.run(r#"kill -0 "$1" 2>/dev/null"#, &[&parent], lim)?;
    out.push((
        "signal a host process (kill -0 the parent shell)",
        if o.code != 0 {
            Found::Contained(format!("pid {parent} is not reachable"))
        } else {
            Found::Escaped(format!("pid {parent} answered"))
        },
    ));

    // Windows, through WSL's interop.
    if wsl() {
        let o = ctx.run(
            "cmd.exe /c ver 2>/dev/null; /mnt/c/Windows/System32/cmd.exe /c ver 2>/dev/null; true",
            &[],
            lim,
        )?;
        out.push(("run cmd.exe (WSL)", windows(&o)));
        let pe = ctx.meta.work().join("probe.exe");
        let copied = fs::copy("/mnt/c/Windows/System32/cmd.exe", &pe)
            .and_then(|_| fs::set_permissions(&pe, fs::Permissions::from_mode(0o755)));
        out.push((
            "run a copied Windows program (WSL)",
            match copied {
                Ok(()) => windows(&ctx.run("./probe.exe /c ver 2>/dev/null; true", &[], lim)?),
                Err(e) => Found::Skipped(format!("could not copy cmd.exe: {e}")),
            },
        ));
        let _ = fs::remove_file(&pe);
    } else {
        out.push(("run Windows programs", Found::Skipped("not WSL".into())));
    }

    // The Docker socket: root on the host for whoever can connect.
    let o = ctx.run(
        r#"for p in /var/run/docker.sock /run/docker.sock "$XDG_RUNTIME_DIR/docker.sock"; do [ -S "$p" ] && echo "$p"; done; true"#,
        &[],
        lim,
    )?;
    let seen = text(&o.stdout);
    out.push((
        "connect to docker.sock",
        if seen.is_empty() {
            Found::Contained("no Docker socket inside".into())
        } else {
            Found::Escaped(format!("visible: {seen}"))
        },
    ));

    out.push((
        "run code on the host through the sandbox's .git",
        planted_git(ctx, &outside)?,
    ));

    // A fork bomb, bounded so a failed limit cannot take the machine down.
    let o = ctx.run(
        // stdout carries the count so a refused fork can be told from a bomb that ran.
        r#"i=0; while [ $i -lt 2000 ]; do sleep 30 & i=$((i+1)); echo $i; done; wait; echo done"#,
        &[],
        lim,
    )?;
    out.push(("fork bomb", judge_bomb(&o, lim.procs)));

    // A memory hog: `tail` keeps the "line" from /dev/zero, which never ends.
    let o = ctx.run("head -c 8000M /dev/zero | tail", &[], lim)?;
    out.push((
        "memory hog",
        stopped_for(
            &o,
            "sandbox_memory",
            &format!("stopped at {}", super::limits::fmt_size(lim.memory)),
        ),
    ));
    Ok(out)
}

/// Hooks, and config that names programs, planted in the clone's `.git` from inside.
/// Each writes a marker on the host if it runs there. belay then brings the work out
/// (commit inside, fetch from outside): no marker may appear, and the work must arrive.
fn planted_git(ctx: &Ctx, outside: &Path) -> Result<Found> {
    let marker = outside.join("git-ran");
    let script = r#"M=$1
printf '#!/bin/sh\necho "$0" >> %s\ncat\n' "$M" > .git/evil; chmod +x .git/evil
mkdir -p .git/hooks
for h in pre-commit post-commit commit-msg prepare-commit-msg reference-transaction \
         post-checkout post-index-change pre-auto-gc fsmonitor-watchman post-update \
         pre-receive update push-to-checkout; do cp .git/evil .git/hooks/$h; done
E=$PWD/.git/evil
for kv in core.fsmonitor core.hooksPath core.pager core.sshCommand core.alternateRefsCommand \
          uploadpack.packObjectsHook gpg.program filter.p.clean filter.p.smudge; do
  git config "$kv" "$E"
done
git config core.hooksPath "$PWD/.git/hooks"
git config commit.gpgSign true
echo '* filter=p' > .gitattributes
echo planted > planted.txt"#;
    let o = ctx.run(script, &[&marker.to_string_lossy()], ctx.limits)?;
    if o.code != 0 {
        return Ok(Found::Skipped(format!(
            "could not plant them (exit {})",
            o.code
        )));
    }
    let landed = super::git::land(ctx.meta, ctx.backend, "doctor");
    if marker.exists() {
        let ran = fs::read_to_string(&marker).unwrap_or_default();
        return Ok(Found::Escaped(format!(
            "ran on the host: {}",
            ran.trim().replace('\n', ", ")
        )));
    }
    Ok(match landed {
        Ok(files) if files.iter().any(|f| f == "planted.txt") => {
            Found::Contained("none ran on the host, and the work still reached the branch".into())
        }
        Ok(files) => Found::Contained(format!(
            "none ran on the host; but the work did not arrive (got {files:?})"
        )),
        Err(e) => Found::Contained(format!(
            "none ran on the host; but bringing the work out failed: {e:#}"
        )),
    })
}

fn windows(o: &super::Outcome) -> Found {
    let said = text(&o.stdout);
    if said.contains("Microsoft Windows") {
        Found::Escaped(format!("Windows answered: {said}"))
    } else {
        Found::Contained("Windows did not run it".into())
    }
}

/// A fork bomb is contained when the watchdog stopped it, or when the forks were
/// refused by the task limit belay put on the systemd scope. Without a belay limit,
/// a refusal or a finished bomb is an escape (the host's own limit does not count).
fn judge_bomb(o: &super::Outcome, procs: u64) -> Found {
    let ran_all = text(&o.stdout).lines().any(|l| l.trim() == "done");
    if o.stopped.is_some() {
        return stopped_for(o, "sandbox_procs", &format!("stopped at {procs} processes"));
    }
    if o.scoped && !ran_all && o.code != 0 {
        return Found::Contained(format!(
            "forks refused at the {procs}-process limit (systemd scope)"
        ));
    }
    if ran_all {
        Found::Escaped("all 2000 processes started".into())
    } else {
        Found::Escaped(format!("not stopped (exit {})", o.code))
    }
}

fn stopped_for(o: &super::Outcome, key: &str, how: &str) -> Found {
    match &o.stopped {
        Some(why) if why.contains(key) => Found::Contained(format!("the watchdog {how}")),
        Some(why) => Found::Escaped(format!("stopped, but for another limit: {why}")),
        None => Found::Escaped(format!("not stopped (exit {})", o.code)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Outcome;

    fn out(code: i32, stopped: Option<&str>, stdout: &str, scoped: bool) -> Outcome {
        Outcome {
            code,
            stopped: stopped.map(Into::into),
            stdout: stdout.as_bytes().to_vec(),
            scoped,
        }
    }

    #[test]
    fn bomb_refused_by_the_scope_is_contained() {
        let o = out(2, None, "1\n2\n3\n", true);
        assert!(matches!(judge_bomb(&o, 64), Found::Contained(m) if m.contains("refused")));
    }

    #[test]
    fn bomb_stopped_by_the_watchdog_is_contained() {
        let o = out(
            137,
            Some("it ran more than 64 processes (sandbox_procs)"),
            "1\n",
            false,
        );
        assert!(matches!(judge_bomb(&o, 64), Found::Contained(m) if m.contains("watchdog")));
    }

    #[test]
    fn bomb_that_ran_free_is_escaped() {
        let o = out(0, None, "1999\n2000\ndone\n", true);
        assert!(matches!(judge_bomb(&o, 64), Found::Escaped(_)));
    }

    #[test]
    fn refusal_without_a_belay_limit_is_escaped() {
        let o = out(2, None, "1\n2\n", false);
        assert!(matches!(judge_bomb(&o, 64), Found::Escaped(_)));
    }
}
