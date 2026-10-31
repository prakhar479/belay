//! The core never names an agent's tools: it plans with `ToolKind`. Connectors map names.

use std::path::Path;

/// Source text outside tests: a whole `tests.rs` is skipped, and other files are cut at
/// their first `#[cfg(test)]`.
fn non_test_sources(dir: &Path, out: &mut Vec<(String, String)>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            non_test_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            && path.file_name().is_some_and(|n| n != "tests.rs")
        {
            let text = std::fs::read_to_string(&path).unwrap();
            let code = text.split("#[cfg(test)]").next().unwrap_or("").to_string();
            out.push((path.display().to_string(), code));
        }
    }
}

#[test]
fn core_logic_has_no_claude_tool_names() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    non_test_sources(&src, &mut files);
    assert!(files.len() > 10, "found only {} source files", files.len());
    let offenders: Vec<_> = files
        .iter()
        .filter(|(_, code)| code.contains("\"Bash\""))
        .map(|(path, _)| path.as_str())
        .collect();
    assert!(
        offenders.is_empty(),
        "these files name Claude Code's Bash tool; use ToolKind::Shell: {offenders:?}"
    );
}
