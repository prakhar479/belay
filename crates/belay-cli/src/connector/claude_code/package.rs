//! The agent-facing package (skill, slash commands, MCP server) built into the binary, so
//! `belay claude` can hand it to Claude Code with `--plugin-dir` when the plugin is not
//! installed. The hooks file is embedded too (a test keeps the list whole) but never
//! written for the launcher: the launcher sets its own hooks.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::paths;

macro_rules! embed {
    ($($p:literal),* $(,)?) => {
        &[$(($p, include_bytes!(concat!("../../../../../plugin/", $p)) as &[u8])),*]
    };
}

/// Every file under `plugin/`, by its path there. `embedded_list_covers_plugin_dir` fails
/// when a file is missing.
pub const FILES: &[(&str, &[u8])] = embed![
    ".claude-plugin/plugin.json",
    ".mcp.json",
    "commands/brief.md",
    "commands/handoff.md",
    "commands/recall.md",
    "commands/remember.md",
    "commands/report.md",
    "hooks/hooks.json",
    "skills/belay/SKILL.md",
];

/// The launcher's hooks run the session, so the plugin's would run beside them.
const SKIPPED: &str = "hooks/hooks.json";

/// `<belay home>/plugin/<version>`.
pub fn dir() -> PathBuf {
    paths::home().join("plugin").join(env!("CARGO_PKG_VERSION"))
}

/// Writes the package under `root`, each file only if missing or different, atomically.
pub fn write_to(root: &Path) -> Result<()> {
    paths::ensure_private_dir(root.parent().unwrap_or(root))?;
    paths::ensure_private_dir(root)?;
    for (rel, bytes) in FILES.iter().filter(|(r, _)| *r != SKIPPED) {
        let path = root.join(rel);
        if fs::read(&path).is_ok_and(|old| old == *bytes) {
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        let mut f = fs::File::create(&tmp).with_context(|| format!("write {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, &path).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(())
}

/// The directory for `--plugin-dir`, written now. `None` on any error: the agent then
/// runs without the package, as before.
pub fn prepare() -> Option<PathBuf> {
    let d = dir();
    write_to(&d).ok().map(|()| d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, base, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
            }
        }
    }

    #[test]
    fn embedded_list_covers_plugin_dir() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugin");
        let mut on_disk = Vec::new();
        walk(&base, &base, &mut on_disk);
        on_disk.sort();
        let mut listed: Vec<String> = FILES.iter().map(|(p, _)| p.to_string()).collect();
        listed.sort();
        assert_eq!(on_disk, listed);
    }

    #[test]
    fn writes_without_hooks_and_is_idempotent() {
        let top = std::env::temp_dir().join(format!("belay-pkg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&top);
        let root = top.join("p/0");
        write_to(&root).unwrap();
        assert!(root.join("skills/belay/SKILL.md").is_file());
        assert!(root.join(".mcp.json").is_file());
        assert!(!root.join("hooks").exists());
        fs::write(root.join(".mcp.json"), "stale").unwrap();
        write_to(&root).unwrap();
        assert_ne!(fs::read(root.join(".mcp.json")).unwrap(), b"stale");
        fs::remove_dir_all(&top).unwrap();
    }
}
