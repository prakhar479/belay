//! The sandbox's git: its own clone, so `git` works inside, and a way back out that runs
//! nothing of the clone's on the host.
//!
//! The workspace is `git clone --shared --no-checkout` of the repo, checked out on
//! `belay/sandbox/<id>`. Its `.git` is a whole repo the sandbox owns (read-write
//! inside). Its objects come from the repo's object store through `--shared`
//! alternates; that one dir is bound read-only at its own path.
//!
//! The clone's config, hooks, and attributes are the sandbox's to write, so the host
//! never runs a git command that reads them with effect: the commit of leftover work
//! runs inside the sandbox, and so does the `upload-pack` of the fetch that brings the
//! branch into the repo. The host's side of that fetch reads only the repo's own
//! config. docs/sandbox.md, "Getting the work out".

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::{exec, Backend, Io, Limits, Meta, Spec};
use crate::audit;

/// git on the host, with hooks and fsmonitor off and no `GIT_*` from the caller.
fn host(dir: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args([
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.fsmonitor=false",
    ]);
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(k);
        }
    }
    let out = cmd
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .context("run git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Make `work` a clone of `repo` on `branch` at `start`. The clone is new and only
/// belay has touched it, so the host may still run git in it here. Returns the repo's
/// object dir that the clone borrows from.
pub fn clone(repo: &Path, work: &Path, branch: &str, start: &str) -> Result<PathBuf> {
    let parent = work.parent().context("a sandbox dir")?;
    host(
        parent,
        &[
            "clone",
            "--quiet",
            "--shared",
            "--no-checkout",
            "--no-tags",
            &repo.to_string_lossy(),
            &work.to_string_lossy(),
        ],
    )
    .context("clone the repo for the sandbox")?;
    // The repo's branch names are not the sandbox's business; nor is a way to fetch
    // from a path it cannot see.
    host(work, &["remote", "remove", "origin"])?;
    host(work, &["checkout", "--quiet", "-b", branch, start])?;
    objects_of(work)
}

/// The object dir the clone borrows (its `--shared` alternate), read once at creation.
fn objects_of(work: &Path) -> Result<PathBuf> {
    let file = work.join(".git/objects/info/alternates");
    let text = fs::read_to_string(&file).context("the clone has no alternates")?;
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .context("the clone's alternates are empty")?;
    fs::canonicalize(line).with_context(|| format!("find {line}"))
}

/// A spec that runs git inside the sandbox: the clone, the private home, the borrowed
/// objects read-only, nothing else of belay's or the agent's.
fn spec(meta: &Meta, argv: Vec<String>) -> Result<Spec> {
    let objects = meta.objects()?;
    let host_env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| k == "PATH" || k == "LANG")
        .collect();
    Ok(Spec {
        work: meta.work(),
        home: meta.home(),
        cwd: meta.work(),
        ro: vec![(objects.clone(), objects)],
        rw: Vec::new(),
        env: super::environment(&host_env, &[], &meta.home(), &meta.id, None),
        argv,
    })
}

/// Who the commit is by: the repo's own identity (its config is the user's), else belay.
fn identity(repo: &Path) -> (String, String) {
    let get = |k: &str| {
        audit::git(repo, &["config", k], None)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    (
        get("user.name").unwrap_or_else(|| "belay".into()),
        get("user.email").unwrap_or_else(|| "belay@localhost".into()),
    )
}

/// Commit what is not committed yet, inside the sandbox. Hooks, fsmonitor, and signing
/// are off; filters the clone names run inside, where they can reach nothing.
pub fn commit_inside(meta: &Meta, backend: &dyn Backend, message: &str) -> Result<()> {
    let (name, email) = identity(&meta.repo);
    let script = r#"M=$1 N=$2 E=$3
g() { git --no-pager -c core.hooksPath=/dev/null -c core.fsmonitor=false -c commit.gpgSign=false -c "user.name=$N" -c "user.email=$E" "$@"; }
{ g add -A && { g diff --cached --quiet || g commit --quiet --no-verify -m "$M"; }; } 2>&1"#;
    let argv = ["sh", "-c", script, "sh", message, &name, &email]
        .map(String::from)
        .to_vec();
    let lim = Limits {
        wall: std::time::Duration::from_secs(600),
        ..Limits::from_config(&crate::config::load())
    };
    let out = exec(meta, backend, &spec(meta, argv)?, lim, Io::Capture)?;
    if out.code != 0 {
        bail!(
            "git commit in the sandbox failed: {}",
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }
    Ok(())
}

/// One shell word.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Bring the sandbox's branch into the repo: a fetch on the host whose `upload-pack`
/// runs inside the sandbox. Returns the files that changed on the branch.
pub fn fetch(meta: &Meta, backend: &dyn Backend) -> Result<Vec<String>> {
    let spec = spec(meta, vec!["git".into(), "upload-pack".into()])?;
    // git runs this through the shell and appends the clone's path.
    let upload_pack = backend
        .argv(&spec)
        .iter()
        .map(|a| quote(&a.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");
    let refspec = format!("+refs/heads/{0}:refs/heads/{0}", meta.branch);
    let before = host(
        &meta.repo,
        &["rev-parse", "--verify", "--quiet", &meta.branch],
    )
    .map(|s| s.trim().to_string())
    .unwrap_or_else(|_| meta.base.clone());
    host(
        &meta.repo,
        &[
            "-c",
            "protocol.file.allow=always",
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-recurse-submodules",
            "--no-write-fetch-head",
            "--upload-pack",
            &upload_pack,
            &meta.work().to_string_lossy(),
            &refspec,
        ],
    )
    .context("bring the sandbox's branch into the repo")?;
    let names = host(
        &meta.repo,
        &["diff", "--name-only", "-z", &before, &meta.branch],
    )?;
    Ok(names
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect())
}

/// Commit inside, then fetch out: the files the branch gained.
pub fn land(meta: &Meta, backend: &dyn Backend, message: &str) -> Result<Vec<String>> {
    commit_inside(meta, backend, message)?;
    fetch(meta, backend)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quoted_word_survives_the_shell() {
        let w = "it's a /path with spaces";
        let out = Command::new("sh")
            .args(["-c", &format!("printf %s {}", quote(w))])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), w);
    }
}
