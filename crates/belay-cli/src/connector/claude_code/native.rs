//! What Claude Code loads on its own at a session's start (M1): the CLAUDE.md files for
//! the working directory and its auto-memory index. Read only: belay never writes them.

use std::fs;
use std::path::{Path, PathBuf};

use super::project_dir_name;
use crate::connector::NativeSource;

/// Bytes read from one file, at most: an instruction file is short, and a huge one
/// should not slow the start.
const MAX_BYTES: usize = 256 * 1024;

/// The files, in the order Claude Code gives them weight: the user's, then the project's
/// from the outermost directory in, each with its local file; then auto-memory.
///
/// - `~/.claude/CLAUDE.md` (under `CLAUDE_CONFIG_DIR` when set).
/// - `CLAUDE.md`, `.claude/CLAUDE.md`, and `CLAUDE.local.md` in the working directory
///   and each parent. [Verify] Claude Code's docs, not a spike: `@path` imports and
///   `.claude/rules/` are not followed.
/// - Auto-memory: `<config>/projects/<project dir name of the main checkout>/memory/
///   MEMORY.md`. Seen on this machine with Claude Code 2.1.316: a worktree's session uses
///   the main checkout's directory, and only `MEMORY.md` (an index) is in the context at
///   the start; the topic files beside it are read on demand, so they are left out.
pub fn native_in(cwd: &Path, config: &Path) -> Vec<NativeSource> {
    let mut paths = vec![config.join("CLAUDE.md")];
    let mut dirs: Vec<&Path> = cwd.ancestors().collect();
    dirs.reverse();
    for d in dirs {
        for name in ["CLAUDE.md", ".claude/CLAUDE.md", "CLAUDE.local.md"] {
            paths.push(d.join(name));
        }
    }
    paths.push(
        config
            .join("projects")
            .join(project_dir_name(&main_checkout(cwd)))
            .join("memory/MEMORY.md"),
    );
    let mut seen = Vec::new();
    paths
        .into_iter()
        .filter_map(|p| {
            let real = fs::canonicalize(&p).ok()?;
            if seen.contains(&real) {
                return None;
            }
            seen.push(real);
            let bytes = fs::read(&p).ok()?;
            let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_BYTES)]).into_owned();
            Some(NativeSource {
                label: label(&p, cwd),
                path: p,
                text,
            })
        })
        .collect()
}

/// The main checkout's root for a worktree (its `.git` file points into
/// `<main>/.git/worktrees/`), else the nearest directory with `.git`, else `cwd`.
fn main_checkout(cwd: &Path) -> PathBuf {
    for d in cwd.ancestors() {
        let git = d.join(".git");
        if git.is_dir() {
            return d.to_path_buf();
        }
        if let Ok(s) = fs::read_to_string(&git) {
            let gitdir = s.trim().strip_prefix("gitdir:").map(str::trim);
            return match gitdir.and_then(|g| g.split_once("/.git/worktrees/")) {
                Some((main, _)) => PathBuf::from(main),
                None => d.to_path_buf(),
            };
        }
    }
    cwd.to_path_buf()
}

/// How the user would name the file: relative to the working directory, else from `~`.
fn label(path: &Path, cwd: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(cwd) {
        return rel.display().to_string();
    }
    if let Some(home) = crate::paths::env_path("HOME") {
        if let Ok(rel) = path.strip_prefix(&home) {
            return format!("~/{}", rel.display());
        }
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_claude_md_files_up_the_tree_the_users_and_the_auto_memory_index() {
        let root = std::env::temp_dir().join(format!("belay-native-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let main = root.join("main");
        let wt = root.join("wt");
        let config = root.join("config");
        let mem = config
            .join("projects")
            .join(project_dir_name(&main))
            .join("memory");
        for d in [
            main.join(".git/worktrees/wt"),
            wt.join("sub/.claude"),
            mem.clone(),
        ] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(
            wt.join(".git"),
            format!("gitdir: {}/.git/worktrees/wt\n", main.display()),
        )
        .unwrap();
        fs::write(config.join("CLAUDE.md"), "user rule").unwrap();
        fs::write(root.join("CLAUDE.md"), "parent rule").unwrap();
        fs::write(wt.join("CLAUDE.md"), "project rule").unwrap();
        fs::write(wt.join("CLAUDE.local.md"), "local rule").unwrap();
        fs::write(wt.join("sub/.claude/CLAUDE.md"), "sub rule").unwrap();
        fs::write(mem.join("MEMORY.md"), "- [x](x.md) — index").unwrap();
        fs::write(mem.join("x.md"), "topic, read on demand").unwrap();

        let cwd = wt.join("sub");
        let got: Vec<(String, String)> = native_in(&cwd, &config)
            .into_iter()
            .map(|n| (n.label, n.text))
            .collect();
        let texts: Vec<&str> = got.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(
            texts,
            [
                "user rule",
                "parent rule",
                "project rule",
                "local rule",
                "sub rule",
                "- [x](x.md) — index"
            ]
        );
        assert_eq!(got[4].0, ".claude/CLAUDE.md");
        // A directory beside the project: not its files.
        let other = native_in(&root.join("other"), &config);
        assert!(other
            .iter()
            .all(|n| n.text != "project rule" && !n.text.contains("index")));
        fs::remove_dir_all(&root).unwrap();
    }
}
