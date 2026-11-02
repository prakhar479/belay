//! docs/guide.md cannot drift from the CLI (C6).
//!
//! The command tree comes from clap itself, through `belay help` and `belay help <cmd>`:
//! clap lists only visible subcommands, so hidden ones (`hook`, `daemon`, `run`, …) are skipped.
//! `help` is used rather than `--help` because `belay claude --help` goes to `claude`.

use std::path::PathBuf;
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn guide() -> String {
    std::fs::read_to_string(root().join("docs/guide.md")).expect("docs/guide.md")
}

/// The text of one `## ` section of the guide, without its heading.
fn section(guide: &str, heading: &str) -> String {
    let start = guide
        .lines()
        .position(|l| l == format!("## {heading}"))
        .unwrap_or_else(|| panic!("docs/guide.md has no `## {heading}` section"));
    guide
        .lines()
        .skip(start + 1)
        .take_while(|l| !l.starts_with("## "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The visible subcommands `belay help <path…>` lists, without `help` itself.
fn subcommands(path: &[&str]) -> Vec<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_belay"))
        .arg("help")
        .args(path)
        .env("NO_COLOR", "1")
        .output()
        .expect("run belay help");
    assert!(
        out.status.success(),
        "`belay help {}` failed",
        path.join(" ")
    );
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .skip_while(|l| *l != "Commands:")
        .skip(1)
        .take_while(|l| !l.trim().is_empty())
        .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|n| *n != "help")
        .map(str::to_string)
        .collect()
}

fn words(s: &str) -> Vec<&str> {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .filter(|w| !w.is_empty())
        .collect()
}

/// The code spans in the command reference table, as word lists. A span names a
/// command family when it starts with `belay <name>`; any later word in it names a
/// subcommand, so `belay fleet up\|status\|down` covers all three.
fn table_spans(guide: &str) -> Vec<Vec<String>> {
    section(guide, "Command reference")
        .lines()
        .filter(|l| l.starts_with('|'))
        .flat_map(|l| l.split('`').skip(1).step_by(2).collect::<Vec<_>>())
        .map(|span| words(span).into_iter().map(str::to_string).collect())
        .collect()
}

fn covered(spans: &[Vec<String>], cmd: &str, sub: Option<&str>) -> bool {
    spans.iter().any(|w| {
        w.len() >= 2
            && w[0] == "belay"
            && w[1] == cmd
            && sub.is_none_or(|s| w[2..].iter().any(|x| x == s))
    })
}

#[test]
fn every_visible_command_is_in_the_command_reference() {
    let spans = table_spans(&guide());
    let top = subcommands(&[]);
    assert!(
        top.len() > 20,
        "too few commands parsed from `belay help`: {top:?}"
    );
    let mut missing = Vec::new();
    for cmd in &top {
        if !covered(&spans, cmd, None) {
            missing.push(format!("belay {cmd}"));
            continue;
        }
        for sub in subcommands(&[cmd]) {
            if !covered(&spans, cmd, Some(&sub)) {
                missing.push(format!("belay {cmd} {sub}"));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "docs/guide.md § Command reference is missing: {}",
        missing.join(", ")
    );
}

#[test]
fn every_config_key_is_in_the_config_section() {
    let src = std::fs::read_to_string(root().join("crates/belay-cli/src/config.rs")).unwrap();
    let start = src.find("const KEYS").expect("config.rs has KEYS");
    let body = &src[start..];
    let body = &body[body.find("= [").expect("KEYS array") + 3..];
    let body = &body[..body.find("];").expect("end of KEYS")];
    let keys: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('"'))
        .map(|l| l.trim_end_matches(',').trim_matches('"'))
        .collect();
    assert!(keys.len() > 20, "too few keys parsed from KEYS: {keys:?}");
    let config = section(&guide(), "Config");
    let present = words(&config);
    let missing: Vec<_> = keys.iter().filter(|k| !present.contains(k)).collect();
    assert!(
        missing.is_empty(),
        "docs/guide.md § Config is missing keys: {missing:?}"
    );
}
