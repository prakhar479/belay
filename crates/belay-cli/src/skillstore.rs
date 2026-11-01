//! `belay skill` (U9): skills from URLs, fetched, checked, staged, and installed only
//! when the user approves (rule 12). A skill is instructions the agent loads in every
//! session, so installing one is deliberate, checked, pinned, and recorded.
//!
//! Fetching uses `git`, `curl`, `tar`, and `unzip` subprocesses, and only for an explicit
//! `belay skill` command or MCP call (rule 11).
//!
//! <home>/skills/
//!   staging/<digest>/files/…      what was fetched, as checked
//!   staging/<digest>/source.json  where it came from
//!   pending.json                  name → a skill staged for the user to approve
//!   log.jsonl                     every stage, approval, install, rejection, removal
//!   tmp/                          fetch scratch, removed after each fetch
//! <skills dir>/.belay-skills.lock name → url, commit or etag, digest, approved_by, time

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use belay_core::skills::{self, Inspected, Lock, LockEntry, Scope, SkillFile};
use serde::{Deserialize, Serialize};

use crate::{audit, connector, paths};

/// Longest a `git` or `curl` call may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest a local `tar` or `unzip` call may take.
const LOCAL_TIMEOUT: Duration = Duration::from_secs(30);
pub const LOCK_FILE: &str = ".belay-skills.lock";

pub fn dir() -> PathBuf {
    paths::home().join("skills")
}

// --- Sources ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    Zip,
    TarGz,
}

#[derive(Debug, PartialEq, Eq)]
enum Source {
    /// A directory (or file) in a GitHub repo. `git_ref` None: the default branch.
    GitHub {
        owner: String,
        repo: String,
        git_ref: Option<String>,
        /// The skill's directory in the repo; empty for the root.
        path: String,
    },
    /// One `SKILL.md` over HTTP(S).
    Raw(String),
    Archive(String, ArchiveKind),
    Local(PathBuf),
}

fn archive_kind(name: &str) -> Option<ArchiveKind> {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".zip") {
        Some(ArchiveKind::Zip)
    } else if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
        Some(ArchiveKind::TarGz)
    } else {
        None
    }
}

fn parse_source(url: &str) -> Result<Source> {
    let url = url.trim();
    if let Some(p) = url.strip_prefix("file://") {
        return Ok(Source::Local(PathBuf::from(p)));
    }
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        if url.contains("://") || url.starts_with("git@") {
            bail!("belay reads https URLs, file:// URLs, and local paths, not `{url}`");
        }
        return Ok(Source::Local(PathBuf::from(url)));
    };
    let bare = rest.split(['?', '#']).next().unwrap_or(rest);
    let (host, path) = bare.split_once('/').unwrap_or((bare, ""));
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if host == "github.com" || host == "www.github.com" {
        let repo = |r: &str| r.trim_end_matches(".git").to_string();
        match segs.as_slice() {
            [o, r] => {
                return Ok(Source::GitHub {
                    owner: o.to_string(),
                    repo: repo(r),
                    git_ref: None,
                    path: String::new(),
                })
            }
            [o, r, kind @ ("tree" | "blob"), git_ref, rest @ ..] => {
                let mut path: Vec<&str> = rest.to_vec();
                // A blob is a file: its skill is the directory it is in.
                if *kind == "blob" {
                    path.pop();
                }
                let path = path.join("/");
                if !path.is_empty() {
                    skills::check_path(&path).map_err(|e| anyhow!(e))?;
                }
                return Ok(Source::GitHub {
                    owner: o.to_string(),
                    repo: repo(r),
                    git_ref: Some(git_ref.to_string()),
                    path,
                });
            }
            _ => {}
        }
    }
    if let Some(kind) = archive_kind(bare) {
        return Ok(Source::Archive(url.to_string(), kind));
    }
    if bare.ends_with(skills::SKILL_FILE) {
        return Ok(Source::Raw(url.to_string()));
    }
    bail!(
        "belay reads GitHub tree or blob URLs, raw SKILL.md URLs, .zip and .tar.gz archives, \
         and local paths; `{url}` is none of these"
    )
}

/// What a fetch brought back, before it is checked.
struct Fetched {
    files: Vec<SkillFile>,
    commit: Option<String>,
    etag: Option<String>,
}

/// A scratch dir removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self> {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = dir()
            .join("tmp")
            .join(format!("{}-{}-{n}", std::process::id(), paths::now_ms()));
        paths::ensure_private_dir(&dir())?;
        fs::create_dir_all(&d)?;
        Ok(Self(d))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fetch(url: &str, work: &Path) -> Result<Fetched> {
    let plain = |files| Fetched {
        files,
        commit: None,
        etag: None,
    };
    match parse_source(url)? {
        Source::Local(p) => {
            let meta = fs::metadata(&p).with_context(|| format!("read {}", p.display()))?;
            if meta.is_dir() {
                return Ok(plain(read_tree(&p, false)?));
            }
            let name = p.file_name().map(|n| n.to_string_lossy().to_string());
            if let Some(kind) = name.as_deref().and_then(archive_kind) {
                return Ok(plain(from_archive(&p, kind, work)?));
            }
            Ok(plain(vec![SkillFile::new(
                skills::SKILL_FILE,
                read_capped(&p, skills::MAX_BYTES)?,
            )]))
        }
        Source::Raw(u) => {
            let out = work.join("SKILL.md");
            let etag = curl(&u, &out, work)?;
            Ok(Fetched {
                files: vec![SkillFile::new(
                    skills::SKILL_FILE,
                    read_capped(&out, skills::MAX_BYTES)?,
                )],
                commit: None,
                etag,
            })
        }
        Source::Archive(u, kind) => {
            let out = work.join(match kind {
                ArchiveKind::Zip => "archive.zip",
                ArchiveKind::TarGz => "archive.tar.gz",
            });
            let etag = curl(&u, &out, work)?;
            Ok(Fetched {
                files: from_archive(&out, kind, work)?,
                commit: None,
                etag,
            })
        }
        Source::GitHub {
            owner,
            repo,
            git_ref,
            path,
        } => {
            let (files, commit) = from_github(&owner, &repo, git_ref.as_deref(), &path, work)?;
            Ok(Fetched {
                files,
                commit: Some(commit),
                etag: None,
            })
        }
    }
}

/// Run a program with a timeout. Output goes to files, so a full pipe cannot block it.
fn run(cmd: &mut Command, timeout: Duration, work: &Path, what: &str) -> Result<String> {
    let (out_path, err_path) = (work.join(".out"), work.join(".err"));
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(fs::File::create(&out_path)?)
        .stderr(fs::File::create(&err_path)?)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .spawn()
        .with_context(|| format!("{what}: could not start it"))?;
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{what} took over {} s and was stopped", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let out = fs::read_to_string(&out_path).unwrap_or_default();
    if !status.success() {
        let err = fs::read_to_string(&err_path).unwrap_or_default();
        bail!("{what} failed: {}", err.trim());
    }
    Ok(out)
}

/// Download `url` to `out`, at most `MAX_BYTES`. The ETag, if the server sends one.
fn curl(url: &str, out: &Path, work: &Path) -> Result<Option<String>> {
    let headers = work.join(".headers");
    run(
        Command::new("curl")
            .args(["-fsSL", "--proto", "=https,http", "--max-filesize"])
            .arg(skills::MAX_BYTES.to_string())
            .arg("--max-time")
            .arg(FETCH_TIMEOUT.as_secs().to_string())
            .arg("-D")
            .arg(&headers)
            .arg("-o")
            .arg(out)
            .arg("--")
            .arg(url),
        FETCH_TIMEOUT + Duration::from_secs(5),
        work,
        "curl",
    )?;
    // Without a Content-Length, curl's own cap may not hold.
    if fs::metadata(out)?.len() > skills::MAX_BYTES {
        bail!("{url} is over {} bytes", skills::MAX_BYTES);
    }
    let headers = fs::read_to_string(headers).unwrap_or_default();
    Ok(headers.lines().rev().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case("etag")
            .then(|| v.trim().to_string())
    }))
}

/// Where the GitHub repos are: `https://github.com`, or a local dir of `<o>/<r>.git`
/// repos in debug builds (`BELAY_TEST_GITHUB`), so tests need no network.
fn github_base() -> String {
    if cfg!(debug_assertions) {
        if let Some(b) = std::env::var_os("BELAY_TEST_GITHUB") {
            return b.to_string_lossy().to_string();
        }
    }
    "https://github.com".into()
}

/// A shallow, sparse clone of just the skill's directory. The files and the commit.
fn from_github(
    owner: &str,
    repo: &str,
    git_ref: Option<&str>,
    path: &str,
    work: &Path,
) -> Result<(Vec<SkillFile>, String)> {
    for part in [owner, repo] {
        if part.starts_with('-') || part.contains(['/', '\\']) || part == ".." {
            bail!("`{owner}/{repo}` is not a GitHub repo name");
        }
    }
    let url = format!("{}/{owner}/{repo}.git", github_base());
    let dest = work.join("repo");
    let is_sha = git_ref
        .is_some_and(|r| (7..=40).contains(&r.len()) && r.chars().all(|c| c.is_ascii_hexdigit()));
    let mut clone = Command::new("git");
    clone.args(["clone", "-q", "--depth", "1", "--filter=blob:none"]);
    if !path.is_empty() {
        clone.arg("--sparse");
    }
    if let (Some(r), false) = (git_ref, is_sha) {
        clone.args(["--branch", r]);
    }
    clone.arg("--").arg(&url).arg(&dest);
    run(&mut clone, FETCH_TIMEOUT, work, "git clone")?;
    let git = |args: &[&str], what: &str| {
        run(
            Command::new("git").arg("-C").arg(&dest).args(args),
            FETCH_TIMEOUT,
            work,
            what,
        )
    };
    if let (Some(sha), true) = (git_ref, is_sha) {
        git(
            &[
                "fetch",
                "-q",
                "--depth",
                "1",
                "--filter=blob:none",
                "origin",
                sha,
            ],
            "git fetch",
        )?;
        git(&["checkout", "-q", "FETCH_HEAD"], "git checkout")?;
    }
    if !path.is_empty() {
        git(
            &["sparse-checkout", "set", "--", path],
            "git sparse-checkout",
        )?;
    }
    let commit = git(&["rev-parse", "HEAD"], "git rev-parse")?
        .trim()
        .to_string();
    let root = if path.is_empty() {
        dest.clone()
    } else {
        dest.join(path)
    };
    if !root.is_dir() {
        bail!("{url} has no directory `{path}` at {commit}");
    }
    Ok((read_tree(&root, path.is_empty())?, commit))
}

/// One archive entry from a listing.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    name: String,
    /// The type letter of `ls -l`: `-` file, `d` dir, `l` symlink, `h` hard link…
    kind: char,
    size: u64,
}

/// An archive's entries, from `tar -t`/`unzip -Z1` (exact names) and their verbose
/// listings (type and size), in the same order.
fn list_archive(file: &Path, kind: ArchiveKind, work: &Path) -> Result<Vec<Entry>> {
    let (names, verbose) = match kind {
        ArchiveKind::TarGz => (
            run(
                Command::new("tar").arg("-tzf").arg(file),
                LOCAL_TIMEOUT,
                work,
                "tar",
            )?,
            run(
                Command::new("tar").arg("-tvzf").arg(file),
                LOCAL_TIMEOUT,
                work,
                "tar",
            )?,
        ),
        ArchiveKind::Zip => (
            run(
                Command::new("unzip").arg("-Z1").arg(file),
                LOCAL_TIMEOUT,
                work,
                "unzip",
            )?,
            run(
                Command::new("unzip").arg("-Z").arg(file),
                LOCAL_TIMEOUT,
                work,
                "unzip",
            )?,
        ),
    };
    let names: Vec<&str> = names.lines().collect();
    let rows: Vec<(char, u64)> = verbose
        .lines()
        .filter_map(|l| {
            let t: Vec<&str> = l.split_whitespace().collect();
            let (perms, size) = match kind {
                ArchiveKind::TarGz => (t.first()?, t.get(2)?),
                // `-rw-r--r--  3.0 unx  6 tx stor 26-Nov-01 10:00 name`: skip the header
                // and footer, whose second word is no version.
                ArchiveKind::Zip => {
                    let v = t.get(1)?;
                    if t.len() < 9
                        || !v.contains('.')
                        || !v.chars().all(|c| c.is_ascii_digit() || c == '.')
                    {
                        return None;
                    }
                    (t.first()?, t.get(3)?)
                }
            };
            Some((perms.chars().next()?, size.parse().ok()?))
        })
        .collect();
    if names.len() != rows.len() {
        bail!("cannot read the archive's listing");
    }
    Ok(names
        .iter()
        .zip(rows)
        .map(|(n, (kind, size))| Entry {
            name: n.to_string(),
            kind,
            size,
        })
        .collect())
}

/// Refuse an archive belay will not extract: traversal, absolute paths, links, devices,
/// too many files, or too many bytes once extracted.
fn check_entries(entries: &[Entry]) -> Result<()> {
    let (mut files, mut bytes) = (0usize, 0u64);
    for e in entries {
        let name = e.name.trim_end_matches('/');
        if name.is_empty() || name == "." {
            continue;
        }
        let name = name.strip_prefix("./").unwrap_or(name);
        skills::check_path(name).map_err(|m| anyhow!("the archive is refused: {m}"))?;
        match e.kind {
            'd' => {}
            '-' => {
                files += 1;
                bytes += e.size;
            }
            'l' => bail!("the archive is refused: `{}` is a symlink", e.name),
            'h' => bail!("the archive is refused: `{}` is a hard link", e.name),
            _ => bail!("the archive is refused: `{}` is not a file", e.name),
        }
    }
    if files > skills::MAX_FILES {
        bail!(
            "the archive has {files} files: a skill may have at most {}",
            skills::MAX_FILES
        );
    }
    if bytes > skills::MAX_BYTES {
        bail!(
            "the archive holds {bytes} bytes: a skill may hold at most {}",
            skills::MAX_BYTES
        );
    }
    Ok(())
}

fn from_archive(file: &Path, kind: ArchiveKind, work: &Path) -> Result<Vec<SkillFile>> {
    check_entries(&list_archive(file, kind, work)?)?;
    let x = work.join("x");
    fs::create_dir_all(&x)?;
    match kind {
        ArchiveKind::TarGz => run(
            Command::new("tar")
                .arg("-xzf")
                .arg(file)
                .arg("-C")
                .arg(&x)
                .arg("--no-same-owner"),
            LOCAL_TIMEOUT,
            work,
            "tar",
        )?,
        ArchiveKind::Zip => run(
            Command::new("unzip")
                .args(["-q", "-o"])
                .arg(file)
                .arg("-d")
                .arg(&x),
            LOCAL_TIMEOUT,
            work,
            "unzip",
        )?,
    };
    // An archive of one top directory (as GitHub makes them) holds the skill in it.
    let root = if x.join(skills::SKILL_FILE).exists() {
        x
    } else {
        let tops: Vec<PathBuf> = fs::read_dir(&x)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        match tops.as_slice() {
            [one] if fs::symlink_metadata(one)?.is_dir() => one.clone(),
            _ => bail!("the archive has no {} at its root", skills::SKILL_FILE),
        }
    };
    read_tree(&root, false)
}

/// At most `cap` bytes of a file; more is an error.
fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    fs::File::open(path)
        .with_context(|| format!("read {}", path.display()))?
        .take(cap + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > cap {
        bail!("{} is over {cap} bytes", path.display());
    }
    Ok(buf)
}

/// Every file under `root`, symlinks marked (not followed), within the limits.
/// `skip_git`: leave out `root/.git` (a clone's).
fn read_tree(root: &Path, skip_git: bool) -> Result<Vec<SkillFile>> {
    let mut out = Vec::new();
    let mut total = 0u64;
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let mut names: Vec<_> = fs::read_dir(root.join(&rel))?
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        names.sort();
        for name in names {
            if skip_git && rel.as_os_str().is_empty() && name == ".git" {
                continue;
            }
            let rel = rel.join(&name);
            let path = root.join(&rel);
            let s = rel
                .to_str()
                .ok_or_else(|| anyhow!("`{}` is not UTF-8", rel.display()))?
                .to_string();
            let meta = fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                stack.push(rel);
                continue;
            }
            if out.len() >= skills::MAX_FILES {
                bail!(
                    "more than {} files: too many for a skill",
                    skills::MAX_FILES
                );
            }
            if meta.file_type().is_symlink() {
                out.push(SkillFile {
                    path: s,
                    bytes: Vec::new(),
                    executable: false,
                    symlink: true,
                });
                continue;
            }
            if !meta.is_file() {
                bail!("`{s}` is not a regular file");
            }
            let bytes = read_capped(&path, skills::MAX_BYTES - total)
                .map_err(|_| anyhow!("over {} bytes: too big for a skill", skills::MAX_BYTES))?;
            total += bytes.len() as u64;
            out.push(SkillFile {
                path: s,
                bytes,
                executable: meta.permissions().mode() & 0o111 != 0,
                symlink: false,
            });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Write checked files under `dest` (which must not exist yet).
fn write_files(dest: &Path, files: &[SkillFile]) -> Result<()> {
    fs::create_dir_all(dest)?;
    for f in files {
        skills::check_path(&f.path).map_err(|e| anyhow!(e))?;
        let p = dest.join(&f.path);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&p, &f.bytes)?;
        let mode = if f.executable { 0o755 } else { 0o644 };
        fs::set_permissions(&p, fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

// --- Staging ---------------------------------------------------------------------------

/// Where a staged skill came from.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Origin {
    url: String,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    etag: Option<String>,
}

struct Staged {
    inspected: Inspected,
    origin: Origin,
}

fn staging(digest: &str) -> PathBuf {
    dir().join("staging").join(digest)
}

/// Fetch, check, and keep the files under `staging/<digest>`.
fn fetch_and_stage(url: &str) -> Result<Staged> {
    let work = Scratch::new()?;
    let fetched = fetch(url, &work.0)?;
    let inspected = skills::inspect(&fetched.files).map_err(|e| anyhow!("{url}: {e}"))?;
    let origin = Origin {
        url: url.trim().to_string(),
        commit: fetched.commit,
        etag: fetched.etag,
    };
    let d = staging(&inspected.digest);
    let _ = fs::remove_dir_all(&d);
    write_files(&d.join("files"), &fetched.files)?;
    fs::write(
        d.join("source.json"),
        serde_json::to_string_pretty(&origin)?,
    )?;
    Ok(Staged { inspected, origin })
}

/// The staged files of `digest`, checked again: staging is under the data dir, which an
/// agent can write.
fn read_staged(digest: &str) -> Result<(Vec<SkillFile>, Inspected)> {
    let files = read_tree(&staging(digest).join("files"), false)
        .with_context(|| format!("the staged files of {digest} are gone"))?;
    let inspected = skills::inspect(&files).map_err(|e| anyhow!(e))?;
    if inspected.digest != digest {
        bail!("the staged files changed after they were staged; stage the skill again");
    }
    Ok((files, inspected))
}

/// A skill staged for the user to approve.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Pending {
    url: String,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    etag: Option<String>,
    digest: String,
    scope: Scope,
    /// The project's root, for `Scope::Project`.
    #[serde(default)]
    repo: Option<PathBuf>,
    /// "agent" or "user".
    staged_by: String,
    time_ms: u64,
    /// The digest of the installed version this one would replace (`belay skill update`).
    #[serde(default)]
    replaces: Option<String>,
}

fn pending_path() -> PathBuf {
    dir().join("pending.json")
}

fn load_pending() -> BTreeMap<String, Pending> {
    fs::read_to_string(pending_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_pending(p: &BTreeMap<String, Pending>) -> Result<()> {
    paths::ensure_private_dir(&dir())?;
    write_atomic(&pending_path(), &(serde_json::to_string_pretty(p)? + "\n"))
}

fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("belay-tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// One line in `log.jsonl`. belay's record of every skill it staged or installed.
/// [Verify] a session's ledger does not get these yet: that needs a daemon request (U8).
fn log(kind: &str, name: &str, digest: &str, url: &str, reason: &str) {
    let session = std::env::var_os("BELAY_SESSION_DIR").and_then(|d| {
        Path::new(&d)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
    });
    let line = serde_json::json!({
        "time_ms": paths::now_ms(),
        "type": kind,
        "name": name,
        "digest": digest,
        "url": url,
        "by": who(),
        "session": session,
        "reason": reason,
    });
    // Best effort: the log never fails the command that the user ran.
    let _ = (|| -> Result<()> {
        paths::ensure_private_dir(&dir())?;
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir().join("log.jsonl"))?;
        writeln!(f, "{line}")?;
        Ok(())
    })();
}

fn who() -> &'static str {
    if paths::users_terminal() {
        "user"
    } else {
        "agent"
    }
}

// --- Where skills go -------------------------------------------------------------------

fn repo_here() -> Result<PathBuf> {
    audit::repo_root(&std::env::current_dir()?)
        .map_err(|_| anyhow!("a project skill needs a git repo: run this inside one"))
}

/// The agent's skills dir for `scope`.
fn skills_root(scope: Scope, repo: Option<&Path>) -> Result<PathBuf> {
    let c = connector::for_agent("claude-code")?;
    if !c.capabilities().skills {
        bail!("{} does not load skills", c.agent());
    }
    c.skill_dir(scope, repo)
        .ok_or_else(|| anyhow!("{} has no {} skills dir", c.agent(), scope.name()))
}

/// Every skills dir there is from here: the user's, and this project's in a repo.
fn roots() -> Vec<(Scope, PathBuf)> {
    let mut out = Vec::new();
    if let Ok(r) = skills_root(Scope::User, None) {
        out.push((Scope::User, r));
    }
    if let Ok(repo) = repo_here() {
        if let Ok(r) = skills_root(Scope::Project, Some(&repo)) {
            out.push((Scope::Project, r));
        }
    }
    out
}

fn read_lock(root: &Path) -> Result<Lock> {
    match fs::read_to_string(root.join(LOCK_FILE)) {
        Ok(s) => serde_json::from_str(&s)
            .with_context(|| format!("read {}", root.join(LOCK_FILE).display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Lock::new()),
        Err(e) => Err(e.into()),
    }
}

fn write_lock(root: &Path, lock: &Lock) -> Result<()> {
    write_atomic(
        &root.join(LOCK_FILE),
        &(serde_json::to_string_pretty(lock)? + "\n"),
    )
}

/// Put `files` in `root/<name>`, replacing what is there, and pin it in the lock.
fn install(root: &Path, name: &str, files: &[SkillFile], entry: LockEntry) -> Result<PathBuf> {
    fs::create_dir_all(root)?;
    let target = root.join(name);
    let new = root.join(format!(".{name}.belay-new"));
    let old = root.join(format!(".{name}.belay-old"));
    let _ = fs::remove_dir_all(&new);
    write_files(&new, files)?;
    if fs::symlink_metadata(&target).is_ok() {
        let _ = fs::remove_dir_all(&old);
        fs::rename(&target, &old)?;
    }
    fs::rename(&new, &target)?;
    if let Ok(m) = fs::symlink_metadata(&old) {
        if m.file_type().is_symlink() {
            fs::remove_file(&old)?;
        } else {
            fs::remove_dir_all(&old)?;
        }
    }
    let mut lock = read_lock(root)?;
    lock.insert(name.to_string(), entry);
    write_lock(root, &lock)?;
    Ok(target)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drift {
    Clean,
    /// The files differ from the digest the user approved.
    Changed,
    Missing,
}

fn drift_of(root: &Path, name: &str, entry: &LockEntry) -> Drift {
    let d = root.join(name);
    if !d.is_dir() {
        return Drift::Missing;
    }
    match read_tree(&d, false) {
        Ok(files) if skills::digest(&files) == entry.digest => Drift::Clean,
        _ => Drift::Changed,
    }
}

// --- Commands --------------------------------------------------------------------------

fn size(n: u64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else {
        format!("{:.1} KB", n as f64 / 1024.0)
    }
}

fn render(ins: &Inspected, origin: &Origin) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Skill: {}", ins.meta.name);
    let _ = writeln!(out, "  {}", ins.meta.description);
    let pin = match (&origin.commit, &origin.etag) {
        (Some(c), _) => format!(" (commit {c})"),
        (None, Some(e)) => format!(" (etag {e})"),
        _ => String::new(),
    };
    let _ = writeln!(out, "Source: {}{pin}", origin.url);
    let total: u64 = ins.files.iter().map(|(_, n)| n).sum();
    let _ = writeln!(out, "Files ({}, {}):", ins.files.len(), size(total));
    for (p, n) in &ins.files {
        let _ = writeln!(out, "  {p}  {}", size(*n));
    }
    if ins.warnings.is_empty() {
        let _ = writeln!(out, "Warnings: none");
    } else {
        let _ = writeln!(out, "Warnings ({}):", ins.warnings.len());
        for w in &ins.warnings {
            let _ = writeln!(out, "  {}", w.render());
        }
    }
    let _ = writeln!(out, "Digest: {}", ins.digest);
    out
}

fn parse_scope(scope: Option<&str>) -> Result<Scope> {
    match scope {
        None => Ok(Scope::User),
        Some(s) => Scope::parse(s).ok_or_else(|| anyhow!("unknown scope {s}: use user or project")),
    }
}

fn pick_name(ins: &Inspected, name: Option<&str>) -> Result<String> {
    match name {
        None => Ok(ins.meta.name.clone()),
        Some(n) => skills::sanitize_name(n).ok_or_else(|| anyhow!("the name `{n}` is empty")),
    }
}

/// `belay skill preview <url>`: fetch and check; install nothing.
pub fn preview(url: &str) -> Result<String> {
    let s = fetch_and_stage(url)?;
    Ok(render(&s.inspected, &s.origin) + "Nothing is installed.\n")
}

/// Stage a skill for the user to approve: what an agent's `add` (and MCP) does.
pub fn stage(url: &str, scope: Option<&str>, name: Option<&str>) -> Result<String> {
    let scope = parse_scope(scope)?;
    let repo = match scope {
        Scope::Project => Some(repo_here()?),
        Scope::User => None,
    };
    skills_root(scope, repo.as_deref())?;
    let s = fetch_and_stage(url)?;
    let name = pick_name(&s.inspected, name)?;
    put_pending(&name, &s, scope, repo, None)?;
    Ok(render(&s.inspected, &s.origin)
        + &format!(
            "Staged {name} for the user. Nothing is installed: ask the user to run \
             `belay skill approve {name}` in their own terminal.\n"
        ))
}

fn put_pending(
    name: &str,
    s: &Staged,
    scope: Scope,
    repo: Option<PathBuf>,
    replaces: Option<String>,
) -> Result<()> {
    let mut pending = load_pending();
    pending.insert(
        name.to_string(),
        Pending {
            url: s.origin.url.clone(),
            commit: s.origin.commit.clone(),
            etag: s.origin.etag.clone(),
            digest: s.inspected.digest.clone(),
            scope,
            repo,
            staged_by: who().into(),
            time_ms: paths::now_ms(),
            replaces,
        },
    );
    save_pending(&pending)?;
    let warned = s.inspected.warnings.len();
    log(
        "skill_staged",
        name,
        &s.inspected.digest,
        &s.origin.url,
        &format!("Staged for the user's approval, with {warned} warning(s)."),
    );
    Ok(())
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// `belay skill add`: from the user's terminal, preview, confirm, install; from an
/// agent, stage it for the user.
pub fn add(url: &str, scope: Option<&str>, name: Option<&str>, yes: bool) -> Result<String> {
    if !paths::users_terminal() {
        return stage(url, scope, name);
    }
    let scope = parse_scope(scope)?;
    let repo = match scope {
        Scope::Project => Some(repo_here()?),
        Scope::User => None,
    };
    let root = skills_root(scope, repo.as_deref())?;
    let s = fetch_and_stage(url)?;
    let name = pick_name(&s.inspected, name)?;
    print!("{}", render(&s.inspected, &s.origin));
    if !yes
        && !confirm(&format!(
            "Install {name} in {}?",
            root.join(&name).display()
        ))?
    {
        return Ok("Nothing is installed.".into());
    }
    let (files, _) = read_staged(&s.inspected.digest)?;
    finish(&root, &name, &files, &s.origin, &s.inspected.digest)
}

fn finish(
    root: &Path,
    name: &str,
    files: &[SkillFile],
    origin: &Origin,
    digest: &str,
) -> Result<String> {
    log(
        "skill_approved",
        name,
        digest,
        &origin.url,
        "The user approved it in their own terminal.",
    );
    let at = install(
        root,
        name,
        files,
        LockEntry {
            url: origin.url.clone(),
            commit: origin.commit.clone(),
            etag: origin.etag.clone(),
            digest: digest.to_string(),
            approved_by: "user".into(),
            time_ms: paths::now_ms(),
        },
    )?;
    log(
        "skill_installed",
        name,
        digest,
        &origin.url,
        &format!("Installed in {} and pinned in its lock.", at.display()),
    );
    Ok(format!(
        "Installed {name} in {}. A running session may not see it until it restarts.",
        at.display()
    ))
}

/// `belay skill approve <name>` (the user's).
pub fn approve(name: &str, yes: bool) -> Result<String> {
    paths::users_only("Approving a skill")?;
    let mut pending = load_pending();
    let p = pending
        .get(name)
        .cloned()
        .ok_or_else(|| anyhow!("no skill {name} waits for approval (see `belay skill list`)"))?;
    let (files, ins) = read_staged(&p.digest)?;
    let root = skills_root(p.scope, p.repo.as_deref())?;
    let origin = Origin {
        url: p.url.clone(),
        commit: p.commit.clone(),
        etag: p.etag.clone(),
    };
    print!("{}", render(&ins, &origin));
    println!("Staged by the {}.", p.staged_by);
    if p.replaces.is_some() {
        let old = read_tree(&root.join(name), false).unwrap_or_default();
        println!("Changes from the installed version:");
        for c in skills::diff(&old, &files) {
            println!("  {}", c.render());
        }
    }
    if !yes && !confirm(&format!("Install {name} in {}?", root.join(name).display()))? {
        return Ok("Nothing is installed. It still waits; `belay skill reject` drops it.".into());
    }
    let said = finish(&root, name, &files, &origin, &p.digest)?;
    pending.remove(name);
    save_pending(&pending)?;
    drop_staging_if_unused(&p.digest, &pending);
    Ok(said)
}

fn drop_staging_if_unused(digest: &str, pending: &BTreeMap<String, Pending>) {
    if !pending.values().any(|p| p.digest == digest) {
        let _ = fs::remove_dir_all(staging(digest));
    }
}

/// `belay skill reject <name>` (the user's): drop a staged skill.
pub fn reject(name: &str) -> Result<String> {
    paths::users_only("Rejecting a skill")?;
    let mut pending = load_pending();
    let p = pending
        .remove(name)
        .ok_or_else(|| anyhow!("no skill {name} waits for approval"))?;
    save_pending(&pending)?;
    drop_staging_if_unused(&p.digest, &pending);
    log(
        "skill_rejected",
        name,
        &p.digest,
        &p.url,
        "The user rejected it.",
    );
    Ok(format!("Rejected {name}. Nothing was installed."))
}

/// `belay skill remove <name>` (the user's): uninstall.
pub fn remove(name: &str, scope: Option<&str>) -> Result<String> {
    paths::users_only("Removing a skill")?;
    let want = scope.map(|s| parse_scope(Some(s))).transpose()?;
    let found: Vec<(Scope, PathBuf)> = roots()
        .into_iter()
        .filter(|(s, r)| {
            want.is_none_or(|w| w == *s)
                && (r.join(name).exists() || read_lock(r).is_ok_and(|l| l.contains_key(name)))
        })
        .collect();
    let (scope, root) = match found.as_slice() {
        [] => bail!("no installed skill {name}"),
        [one] => one.clone(),
        _ => bail!("{name} is installed for the user and for this project: pass --scope"),
    };
    let mut lock = read_lock(&root)?;
    let entry = lock.remove(name);
    let d = root.join(name);
    if fs::symlink_metadata(&d).is_ok_and(|m| m.file_type().is_symlink()) {
        fs::remove_file(&d)?;
    } else if d.exists() {
        fs::remove_dir_all(&d)?;
    }
    write_lock(&root, &lock)?;
    let (digest, url) = entry.map(|e| (e.digest, e.url)).unwrap_or_default();
    log("skill_removed", name, &digest, &url, "The user removed it.");
    Ok(format!("Removed {name} ({}).", scope.name()))
}

/// `belay skill list`: installed (with drift) and waiting.
pub fn list() -> Result<String> {
    let mut out = String::new();
    let mut any = false;
    for (scope, root) in roots() {
        let lock = read_lock(&root)?;
        let mut names: Vec<String> = lock.keys().cloned().collect();
        if let Ok(rd) = fs::read_dir(&root) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if !n.starts_with('.') && e.path().is_dir() && !lock.contains_key(&n) {
                    names.push(n);
                }
            }
        }
        names.sort();
        for n in names {
            any = true;
            let Some(e) = lock.get(&n) else {
                let _ = writeln!(
                    out,
                    "{n} · {} · not installed by belay (no lock entry)",
                    scope.name()
                );
                continue;
            };
            let pin = e
                .commit
                .as_deref()
                .map(|c| format!(" @ {}", &c[..c.len().min(12)]))
                .unwrap_or_default();
            let state = match drift_of(&root, &n, e) {
                Drift::Clean => "matches its lock",
                Drift::Changed => "DRIFT: files differ from the approved digest",
                Drift::Missing => "DRIFT: its directory is gone",
            };
            let _ = writeln!(out, "{n} · {} · {}{pin} · {state}", scope.name(), e.url);
        }
    }
    let pending = load_pending();
    if !pending.is_empty() {
        let _ = writeln!(
            out,
            "\nWaiting for your approval (`belay skill approve <name>`):"
        );
        for (n, p) in &pending {
            let what = if p.replaces.is_some() {
                "update"
            } else {
                "new"
            };
            let _ = writeln!(
                out,
                "{n} · {} · {} · {what}, staged by the {}",
                p.scope.name(),
                p.url,
                p.staged_by
            );
        }
    }
    if !any && pending.is_empty() {
        out.push_str("No skills. `belay skill preview <url>` looks at one.\n");
    }
    Ok(out)
}

/// `belay skill update [name]`: fetch each installed skill again; a change waits for
/// approval like a new skill.
pub fn update(name: Option<&str>) -> Result<String> {
    let mut out = String::new();
    let mut seen = false;
    for (scope, root) in roots() {
        let repo = match scope {
            Scope::Project => repo_here().ok(),
            Scope::User => None,
        };
        for (n, e) in read_lock(&root)? {
            if name.is_some_and(|w| w != n) {
                continue;
            }
            seen = true;
            let s = match fetch_and_stage(&e.url) {
                Ok(s) => s,
                Err(err) => {
                    let _ = writeln!(out, "{n}: could not fetch: {err:#}");
                    continue;
                }
            };
            if s.inspected.digest == e.digest {
                let _ = writeln!(out, "{n}: up to date.");
                continue;
            }
            let old = read_tree(&root.join(&n), false).unwrap_or_default();
            let _ = writeln!(out, "{n}: changed upstream.");
            for c in skills::diff(&old, &s.inspected_files()?) {
                let _ = writeln!(out, "  {}", c.render());
            }
            for w in &s.inspected.warnings {
                let _ = writeln!(out, "  warning: {}", w.render());
            }
            put_pending(&n, &s, scope, repo.clone(), Some(e.digest.clone()))?;
            let _ = writeln!(out, "  It needs `belay skill approve {n}` (yours to run).");
        }
    }
    if !seen {
        match name {
            Some(n) => bail!("no installed skill {n}"),
            None => out.push_str("No installed skills to update.\n"),
        }
    }
    Ok(out)
}

impl Staged {
    fn inspected_files(&self) -> Result<Vec<SkillFile>> {
        read_staged(&self.inspected.digest).map(|(f, _)| f)
    }
}

/// `belay doctor`'s lines: (status, text) for skills whose files left their lock.
pub fn doctor_lines() -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let mut clean = 0;
    for (scope, root) in roots() {
        let lock = match read_lock(&root) {
            Ok(l) => l,
            Err(e) => {
                out.push(("warn", format!("skills lock: {e:#}")));
                continue;
            }
        };
        for (n, e) in &lock {
            match drift_of(&root, n, e) {
                Drift::Clean => clean += 1,
                Drift::Changed => out.push((
                    "warn",
                    format!(
                        "skill {n} ({}) differs from what you approved; see `belay skill list`",
                        scope.name()
                    ),
                )),
                Drift::Missing => out.push((
                    "warn",
                    format!(
                        "skill {n} ({}) is in the lock but its directory is gone",
                        scope.name()
                    ),
                )),
            }
        }
    }
    if clean > 0 {
        out.push(("ok", format!("{clean} skill(s) match their lock")));
    }
    let waiting = load_pending().len();
    if waiting > 0 {
        out.push((
            "info",
            format!("{waiting} skill(s) wait for `belay skill approve`"),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("belay-skill-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn urls_parse_to_sources() {
        let gh = |r: Option<&str>, p: &str| Source::GitHub {
            owner: "o".into(),
            repo: "r".into(),
            git_ref: r.map(str::to_string),
            path: p.into(),
        };
        let cases = [
            ("https://github.com/o/r", gh(None, "")),
            ("https://github.com/o/r.git", gh(None, "")),
            (
                "https://github.com/o/r/tree/main/skills/pdf",
                gh(Some("main"), "skills/pdf"),
            ),
            (
                "https://github.com/o/r/blob/v1/skills/pdf/SKILL.md",
                gh(Some("v1"), "skills/pdf"),
            ),
            (
                "https://raw.githubusercontent.com/o/r/main/x/SKILL.md",
                Source::Raw("https://raw.githubusercontent.com/o/r/main/x/SKILL.md".into()),
            ),
            (
                "https://example.com/s.tar.gz?x=1",
                Source::Archive(
                    "https://example.com/s.tar.gz?x=1".into(),
                    ArchiveKind::TarGz,
                ),
            ),
            (
                "https://github.com/o/r/archive/refs/heads/main.zip",
                Source::Archive(
                    "https://github.com/o/r/archive/refs/heads/main.zip".into(),
                    ArchiveKind::Zip,
                ),
            ),
            ("file:///tmp/s", Source::Local("/tmp/s".into())),
            ("./skills/x", Source::Local("./skills/x".into())),
        ];
        for (url, want) in cases {
            assert_eq!(parse_source(url).unwrap(), want, "{url}");
        }
        for bad in [
            "https://example.com/page",
            "ftp://x/y",
            "git@github.com:o/r",
            "https://github.com/o/r/tree/main/../x",
        ] {
            assert!(parse_source(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn archive_entries_with_traversal_links_or_bulk_are_refused() {
        let e = |name: &str, kind, size| Entry {
            name: name.into(),
            kind,
            size,
        };
        assert!(check_entries(&[e("s/", 'd', 0), e("s/SKILL.md", '-', 10)]).is_ok());
        for bad in [
            e("../evil", '-', 1),
            e("s/../../evil", '-', 1),
            e("/etc/passwd", '-', 1),
            e("s/link", 'l', 0),
            e("s/hard", 'h', 0),
            e("s/fifo", 'p', 0),
            e("s/big", '-', skills::MAX_BYTES + 1),
        ] {
            assert!(check_entries(&[bad]).is_err());
        }
    }

    /// Real archives through `tar` and `unzip`: a good one reads, a traversal or a
    /// symlink is refused before anything is extracted.
    #[test]
    fn archives_are_listed_and_checked_before_extracting() {
        let d = tmp("archive");
        let src = d.join("src/top");
        fs::create_dir_all(src.join("scripts")).unwrap();
        fs::write(src.join("SKILL.md"), "---\nname: a\ndescription: b\n---\n").unwrap();
        fs::write(src.join("scripts/run.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(
            src.join("scripts/run.sh"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let sh = |cmd: &str| {
            assert!(
                Command::new("sh")
                    .arg("-c")
                    .arg(cmd)
                    .current_dir(&d)
                    .status()
                    .unwrap()
                    .success(),
                "{cmd}"
            );
        };
        sh("tar czf good.tgz -C src top && (cd src && zip -qr ../good.zip top)");
        for (file, kind) in [
            ("good.tgz", ArchiveKind::TarGz),
            ("good.zip", ArchiveKind::Zip),
        ] {
            let work = d.join(format!("w-{file}"));
            fs::create_dir_all(&work).unwrap();
            let files = from_archive(&d.join(file), kind, &work).unwrap();
            let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
            assert_eq!(paths, ["SKILL.md", "scripts/run.sh"], "{file}");
            assert!(files[1].executable, "{file}");
        }
        sh("ln -s /etc/passwd src/top/link && tar czf link.tgz -C src top && (cd src && zip -qry ../link.zip top)");
        sh("tar czf dots.tgz -P src/top/../top/SKILL.md");
        for (file, kind, why) in [
            ("link.tgz", ArchiveKind::TarGz, "symlink"),
            ("link.zip", ArchiveKind::Zip, "symlink"),
            ("dots.tgz", ArchiveKind::TarGz, "leaves"),
        ] {
            let work = d.join(format!("w-{file}"));
            fs::create_dir_all(&work).unwrap();
            let err = from_archive(&d.join(file), kind, &work)
                .unwrap_err()
                .to_string();
            assert!(err.contains(why), "{file}: {err}");
            assert!(!work.join("x").exists(), "{file}: nothing extracted");
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_tree_marks_symlinks_and_keeps_to_the_limits() {
        let d = tmp("tree");
        fs::write(d.join("SKILL.md"), "x").unwrap();
        std::os::unix::fs::symlink("/etc/passwd", d.join("l")).unwrap();
        let files = read_tree(&d, false).unwrap();
        assert!(files.iter().any(|f| f.path == "l" && f.symlink));
        assert!(skills::validate(&files).unwrap_err().contains("symlink"));
        fs::remove_file(d.join("l")).unwrap();
        fs::write(d.join("big"), vec![b'a'; skills::MAX_BYTES as usize]).unwrap();
        assert!(read_tree(&d, false)
            .unwrap_err()
            .to_string()
            .contains("too big"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn install_replaces_and_pins_and_drift_is_seen() {
        let root = tmp("install");
        let entry = |digest: &str| LockEntry {
            url: "file:///x".into(),
            commit: None,
            etag: None,
            digest: digest.into(),
            approved_by: "user".into(),
            time_ms: 1,
        };
        let v1 = vec![
            SkillFile::new("SKILL.md", "one"),
            SkillFile::new("old.md", "o"),
        ];
        let at = install(&root, "s", &v1, entry(&skills::digest(&v1))).unwrap();
        assert_eq!(
            drift_of(&root, "s", &read_lock(&root).unwrap()["s"]),
            Drift::Clean
        );
        let v2 = vec![SkillFile::new("SKILL.md", "two")];
        install(&root, "s", &v2, entry(&skills::digest(&v2))).unwrap();
        assert!(!at.join("old.md").exists(), "replaced, not merged");
        let lock = read_lock(&root).unwrap();
        assert_eq!(lock["s"].digest, skills::digest(&v2));
        fs::write(at.join("SKILL.md"), "tampered").unwrap();
        assert_eq!(drift_of(&root, "s", &lock["s"]), Drift::Changed);
        fs::remove_dir_all(&at).unwrap();
        assert_eq!(drift_of(&root, "s", &lock["s"]), Drift::Missing);
        let names: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, [LOCK_FILE], "no scratch left");
        let _ = fs::remove_dir_all(&root);
    }
}
