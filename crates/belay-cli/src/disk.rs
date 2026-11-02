//! Disk discipline: where fleet and sandbox builds go, and how much disk belay's data
//! takes (`belay doctor`, `belay fleet`).
//!
//! Build cache. A Rust repo's `target/` is 1–3 GB per checkout. Fleets used to leave
//! one in every worker worktree. With `build_cache = shared` a fleet session gets
//! `CARGO_TARGET_DIR=<home>/cache/<repo key>/<slot>/target`: one dir per repo and slot
//! name, so the next fleet's `worker-1` starts warm, and disk is bounded by the pool
//! size, not by the number of fleets. Two running sessions never share one dir: cargo
//! puts binaries at `<target>/debug/<name>` and tests run them from there
//! (`CARGO_BIN_EXE_*`), so a shared dir lets one worker's tests run another's binary.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::paths;

/// belay's data above this many bytes gets a warning.
pub const WARN_BYTES: u64 = 10_000_000_000;

/// `build_cache` (fleets) and `sandbox_build_cache` (sandboxes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildCache {
    /// A target dir per repo and slot under `<home>/cache`.
    Shared,
    /// Each checkout keeps its own `target/`; `sccache` on PATH is used when found.
    Separate,
    /// belay sets nothing.
    Off,
}

impl BuildCache {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "shared" => Some(Self::Shared),
            "separate" => Some(Self::Separate),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Separate => "separate",
            Self::Off => "off",
        }
    }
}

pub fn cache_root() -> PathBuf {
    paths::home().join("cache")
}

/// A short, stable name for a repo: its directory name and a hash of its git common
/// dir, so every worktree of one repo has the same key.
pub fn repo_key(repo: &Path) -> String {
    let common = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()));
    let id = common.unwrap_or_else(|| repo.to_path_buf());
    let id = fs::canonicalize(&id).unwrap_or(id);
    // The repo's own name, not `.git`.
    let named = if id.file_name().is_some_and(|n| n == ".git") {
        id.parent().unwrap_or(&id)
    } else {
        &id
    };
    let name: String = named
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let hash = belay_core::text::sha256_hex(&id.to_string_lossy());
    format!("{name}-{}", &hash[..12])
}

/// What the environment already says about cargo's output and wrapper.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// `CARGO_TARGET_DIR` is set.
    pub target_dir: bool,
    /// `RUSTC_WRAPPER` is set.
    pub wrapper: bool,
    /// `sccache` is on PATH.
    pub sccache: bool,
}

impl Env {
    pub fn current() -> Self {
        Self {
            target_dir: paths::env_path("CARGO_TARGET_DIR").is_some(),
            wrapper: paths::env_path("RUSTC_WRAPPER").is_some(),
            sccache: on_path("sccache"),
        }
    }
}

/// Whether `dir`'s cargo config names a target dir (the user chose one).
fn config_sets_target(dir: &Path) -> bool {
    ["config.toml", "config"].iter().any(|f| {
        fs::read_to_string(dir.join(".cargo").join(f))
            .is_ok_and(|t| t.lines().any(|l| l.trim_start().starts_with("target-dir")))
    })
}

/// The variables a session working in `work` gets, for `mode`. `key` names its target
/// dir under the repo's cache (a fleet slot, or `sandbox`). Nothing for a checkout with
/// no `Cargo.toml`, or where the user chose a target dir or a wrapper.
pub fn cargo_env(
    mode: BuildCache,
    cache: &Path,
    work: &Path,
    key: &str,
    env: &Env,
) -> Vec<(String, String)> {
    if !work.join("Cargo.toml").is_file() {
        return Vec::new();
    }
    match mode {
        BuildCache::Off => Vec::new(),
        BuildCache::Shared if env.target_dir || config_sets_target(work) => Vec::new(),
        BuildCache::Shared => {
            let dir = cache.join(sanitize(key)).join("target");
            vec![(
                "CARGO_TARGET_DIR".into(),
                dir.to_string_lossy().into_owned(),
            )]
        }
        BuildCache::Separate if env.sccache && !env.wrapper => {
            vec![("RUSTC_WRAPPER".into(), "sccache".into())]
        }
        BuildCache::Separate => Vec::new(),
    }
}

fn sanitize(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

// ---- disk use

/// What belay's data takes, by part, and the largest single item.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    /// (part, bytes): fleets, sandboxes, sessions, cache.
    pub parts: Vec<(&'static str, u64)>,
    /// The largest fleet worktree, sandbox, session, or repo cache.
    pub largest: Option<(PathBuf, u64)>,
    /// The rest of belay's home (memory, learned fixes, config state).
    pub rest: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.parts.iter().map(|(_, b)| b).sum()
    }

    pub fn over(&self) -> bool {
        self.total() > WARN_BYTES
    }

    /// One line: `belay data 12.3 GB (fleets 9.1 GB, …); largest: <path> (3.0 GB).`
    pub fn line(&self) -> String {
        let parts: Vec<String> = self
            .parts
            .iter()
            .map(|(n, b)| format!("{n} {}", human(*b)))
            .collect();
        let mut out = format!("belay data {} ({})", human(self.total()), parts.join(", "));
        if let Some((p, b)) = &self.largest {
            out.push_str(&format!("; largest: {} ({})", p.display(), human(*b)));
        }
        out.push('.');
        out
    }
}

pub fn human(b: u64) -> String {
    if b >= 1_000_000_000 {
        format!("{:.1} GB", b as f64 / 1e9)
    } else {
        format!("{:.1} MB", b as f64 / 1e6)
    }
}

/// The disk use under belay's `home`. Items: each fleet worktree, each sandbox, each
/// session, each repo cache.
pub fn usage(home: &Path) -> Usage {
    let mut parts = Vec::new();
    let mut largest: Option<(PathBuf, u64)> = None;
    let mut note = |p: PathBuf, b: u64| {
        if largest.as_ref().is_none_or(|(_, l)| b > *l) {
            largest = Some((p, b));
        }
    };
    // Fleets: worktrees are the items; the rest of a fleet's dir counts to the part.
    let mut fleets = 0;
    for f in children(&home.join("fleets")) {
        let wts = f.join("worktrees");
        for wt in children(&wts) {
            let b = dir_size(&wt);
            fleets += b;
            note(wt, b);
        }
        fleets += children(&f)
            .into_iter()
            .filter(|p| *p != wts)
            .map(|p| size_of(&p))
            .sum::<u64>();
    }
    parts.push(("fleets", fleets));
    for part in ["sandboxes", "sessions", "cache"] {
        let mut total = 0;
        for item in children(&home.join(part)) {
            let b = size_of(&item);
            total += b;
            note(item, b);
        }
        parts.push((part, total));
    }
    let rest = children(home)
        .into_iter()
        .filter(|p| {
            !p.file_name().is_some_and(|n| {
                ["fleets", "sandboxes", "sessions", "cache"]
                    .iter()
                    .any(|k| n == *k)
            })
        })
        .map(|p| size_of(&p))
        .sum();
    Usage {
        parts,
        largest,
        rest,
    }
}

fn children(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    v.sort();
    v
}

fn size_of(p: &Path) -> u64 {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => dir_size(p),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// Bytes under `dir`; symlinks are not followed.
pub fn dir_size(dir: &Path) -> u64 {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("belay-disk-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn file(p: &Path, bytes: usize) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![0u8; bytes]).unwrap();
    }

    #[test]
    fn usage_counts_each_part_and_names_the_largest() {
        let h = tmp("usage");
        file(&h.join("fleets/f1/ledger.jsonl"), 100);
        file(&h.join("fleets/f1/worktrees/worker-1/target/x"), 5_000);
        file(&h.join("fleets/f1/worktrees/worker-2/a"), 300);
        file(&h.join("sandboxes/sb-1/work/a"), 2_000);
        file(&h.join("sessions/s1/ledger.jsonl"), 50);
        file(&h.join("cache/repo-abc/worker-1/target/x"), 4_000);
        file(&h.join("learn/fixes.jsonl"), 7);
        let u = usage(&h);
        assert_eq!(
            u.parts,
            vec![
                ("fleets", 5_400),
                ("sandboxes", 2_000),
                ("sessions", 50),
                ("cache", 4_000)
            ]
        );
        assert_eq!(u.total(), 11_450);
        assert_eq!(u.rest, 7);
        assert_eq!(
            u.largest,
            Some((h.join("fleets/f1/worktrees/worker-1"), 5_000))
        );
        assert!(!u.over());
        assert!(u.line().contains("largest: "), "{}", u.line());
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn cargo_env_follows_the_mode_and_the_user() {
        let w = tmp("cargo-env");
        let cache = Path::new("/c/repo-1");
        let none = Env::default();
        // No Cargo.toml: nothing.
        assert!(cargo_env(BuildCache::Shared, cache, &w, "worker-1", &none).is_empty());
        fs::write(w.join("Cargo.toml"), "[package]\n").unwrap();
        assert_eq!(
            cargo_env(BuildCache::Shared, cache, &w, "worker-1", &none),
            vec![(
                "CARGO_TARGET_DIR".to_string(),
                "/c/repo-1/worker-1/target".to_string()
            )]
        );
        // A slot name with a slash stays one dir.
        assert_eq!(
            cargo_env(BuildCache::Shared, cache, &w, "team/a", &none)[0].1,
            "/c/repo-1/team_a/target"
        );
        let user = Env {
            target_dir: true,
            ..Env::default()
        };
        assert!(cargo_env(BuildCache::Shared, cache, &w, "worker-1", &user).is_empty());
        assert!(cargo_env(BuildCache::Off, cache, &w, "worker-1", &none).is_empty());
        assert!(cargo_env(BuildCache::Separate, cache, &w, "worker-1", &none).is_empty());
        let sccache = Env {
            sccache: true,
            ..Env::default()
        };
        assert_eq!(
            cargo_env(BuildCache::Separate, cache, &w, "worker-1", &sccache),
            vec![("RUSTC_WRAPPER".to_string(), "sccache".to_string())]
        );
        // The repo's cargo config names a target dir: the user's choice stands.
        fs::create_dir_all(w.join(".cargo")).unwrap();
        fs::write(
            w.join(".cargo/config.toml"),
            "[build]\ntarget-dir = \"t\"\n",
        )
        .unwrap();
        assert!(cargo_env(BuildCache::Shared, cache, &w, "worker-1", &none).is_empty());
        let _ = fs::remove_dir_all(&w);
    }

    #[test]
    fn worktrees_of_one_repo_share_a_key() {
        let r = tmp("key");
        let git = |dir: &Path, args: &[&str]| {
            assert!(Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success());
        };
        let repo = r.join("myrepo");
        fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "i",
            ],
        );
        let wt = r.join("wt");
        git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
        let k = repo_key(&repo);
        assert!(k.starts_with("myrepo-"), "{k}");
        assert_eq!(k, repo_key(&wt));
        assert_ne!(k, repo_key(&r));
        let _ = fs::remove_dir_all(&r);
    }
}
