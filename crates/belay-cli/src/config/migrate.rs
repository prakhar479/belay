//! `belay config migrate`: rewrite a 0.1 config file's keys and values in place.
//! Comments, order, blank lines, line endings, and keys belay does not know stay as
//! they are; only the lines with a 0.1 key or a `look` value change.

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, Write as _};
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

use super::{line_key, LOOK_KEYS, OLD_VALUES, RENAMED};

/// One changed line: 1-based, its text before, and after (`None`: dropped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub line: usize,
    pub before: String,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Migration {
    /// The whole new file.
    pub text: String,
    pub changes: Vec<Change>,
    /// What changed, one sentence each, for the user.
    pub notes: Vec<String>,
}

/// The migrated text of a config file. Refuses (changes nothing) a file with a line
/// that is not `key = value`, a comment, or blank.
pub fn migrate(text: &str) -> Result<Migration> {
    let (bom, body) = match text.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", text),
    };
    for (n, raw) in body.lines().enumerate() {
        let code = raw.split('#').next().unwrap_or("").trim();
        if code.is_empty() {
            continue;
        }
        match code.split_once('=') {
            Some((k, _)) if !k.trim().is_empty() => {}
            _ => bail!(
                "line {} is not `key = value`: {}. Fix or remove it first; nothing changed",
                n + 1,
                raw.trim()
            ),
        }
    }
    // The line where each key is first set, to say which line an old key yields to.
    let mut first: std::collections::BTreeMap<&str, usize> = Default::default();
    for (n, raw) in body.lines().enumerate() {
        if let Some(k) = line_key(raw) {
            first.entry(k).or_insert(n + 1);
        }
    }
    let mut m = Migration {
        text: bom.to_string(),
        ..Default::default()
    };
    for (n, piece) in body.split_inclusive('\n').enumerate() {
        let line_no = n + 1;
        let content = piece.trim_end_matches(['\n', '\r']);
        let ending = &piece[content.len()..];
        let (after, note) = rewrite(content, &first);
        match after {
            Some(ref a) if a == content => m.text.push_str(piece),
            Some(a) => {
                m.text.push_str(&a);
                m.text.push_str(ending);
                m.changes.push(Change {
                    line: line_no,
                    before: content.to_string(),
                    after: Some(a),
                });
            }
            None => m.changes.push(Change {
                line: line_no,
                before: content.to_string(),
                after: None,
            }),
        }
        if let Some(note) = note {
            m.notes.push(format!("line {line_no}: {note}"));
        }
    }
    Ok(m)
}

/// One line, rewritten: `None` drops it. The note says what changed.
fn rewrite(
    content: &str,
    first: &std::collections::BTreeMap<&str, usize>,
) -> (Option<String>, Option<String>) {
    let Some(key) = line_key(content) else {
        return (Some(content.to_string()), None);
    };
    let code_end = content.find('#').unwrap_or(content.len());
    let key_at = content.find(key).unwrap_or(0);
    let (mut out, mut notes) = (content.to_string(), Vec::new());
    let mut new_key = key;
    if let Some((old, new)) = RENAMED.iter().find(|(k, _)| *k == key) {
        match new {
            None => {
                return (
                    None,
                    Some(format!("dropped `{old}`: belay has no modes now")),
                )
            }
            Some(new) => {
                if let Some(at) = first.get(new) {
                    return (
                        None,
                        Some(format!(
                            "dropped `{old}`: `{new}` is already set (line {at}), and it stays"
                        )),
                    );
                }
                out.replace_range(key_at..key_at + key.len(), new);
                notes.push(format!("`{old}` is now `{new}`"));
                new_key = new;
            }
        }
    }
    if LOOK_KEYS.contains(&new_key) {
        let code_end = code_end + out.len() - content.len();
        if let Some(eq) = out[..code_end].find('=') {
            let value = out[eq + 1..code_end].trim();
            if value.trim_matches('"') == "look" {
                let at = eq + 1 + out[eq + 1..].find("look").unwrap_or(0);
                out.replace_range(at..at + 4, "on");
                notes.push(format!("`{new_key} = look` is now `{new_key} = on`"));
            }
        }
    }
    let code_end = code_end + out.len() - content.len();
    if let Some(eq) = out[..code_end].find('=') {
        let raw = out[eq + 1..code_end].trim();
        let value = raw.trim_matches('"');
        if let Some((_, _, new)) = OLD_VALUES
            .iter()
            .find(|(k, v, _)| *k == new_key && *v == value)
        {
            let at = eq + 1 + out[eq + 1..].find(raw).unwrap_or(0);
            let notes_new = format!("`{new_key} = {value}` is now `{new_key} = {new}`");
            out.replace_range(at..at + raw.len(), new);
            notes.push(notes_new);
        }
    }
    let note = (!notes.is_empty()).then(|| notes.join("; "));
    (Some(out), note)
}

/// A unified-style diff of the changes, one hunk per changed line.
pub fn diff(path: &Path, m: &Migration) -> String {
    let mut out = format!("--- {0}\n+++ {0} (migrated)\n", path.display());
    for c in &m.changes {
        let _ = writeln!(out, "@@ line {} @@", c.line);
        let _ = writeln!(out, "-{}", c.before);
        if let Some(a) = &c.after {
            let _ = writeln!(out, "+{a}");
        }
    }
    out
}

/// `belay config migrate [--yes]`.
pub fn run(yes: bool) -> Result<()> {
    let path = super::path();
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("No config file at {}: nothing to migrate.", path.display());
            return Ok(());
        }
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| anyhow!("{} is not UTF-8 text; nothing changed", path.display()))?;
    let m = migrate(&text).with_context(|| format!("{}", path.display()))?;
    if m.changes.is_empty() {
        println!("{} has no 0.1 keys: nothing to migrate.", path.display());
        return Ok(());
    }
    print!("{}", diff(&path, &m));
    for n in &m.notes {
        println!("{n}");
    }
    if !yes && !confirm("Write these changes? [y/N] ")? {
        println!("Nothing changed.");
        return Ok(());
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let name = path
        .file_name()
        .map_or_else(|| "config.toml".into(), |n| n.to_string_lossy().to_string());
    let backup = path.with_file_name(format!("{name}.bak-{secs}"));
    fs::write(&backup, &text).with_context(|| format!("write {}", backup.display()))?;
    // Written in place, not renamed over, so a symlinked config (dotfiles) stays a link.
    fs::write(&path, &m.text).with_context(|| format!("write {}", path.display()))?;
    println!(
        "Updated {}. The old file is {}.",
        path.display(),
        backup.display()
    );
    Ok(())
}

fn confirm(prompt: &str) -> Result<bool> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (input, output, notes that must appear)
    const TABLE: &[(&str, &str, &[&str])] = &[
        // Renamed keys, comments and spacing kept.
        (
            "# mine\nfleet = on   # peers\nlearn=off\n",
            "# mine\nawareness = on   # peers\nmemory=off\n",
            &["`fleet` is now `awareness`", "`learn` is now `memory`"],
        ),
        // `look` values, quoted or not, on old and new keys.
        (
            "commons = look\nlearn = \"look\" # x\nfleet = look\n",
            "commons = on\nmemory = \"on\" # x\nawareness = on\n",
            &["`commons = look` is now `commons = on`", "`memory = look`"],
        ),
        // plugin_mode is dropped.
        (
            "plugin_mode = optimize # old\nguard = warn\n",
            "guard = warn\n",
            &["dropped `plugin_mode`"],
        ),
        // Old and new both set: the new one stays, wherever it is.
        (
            "fleet = off\nawareness = on\n",
            "awareness = on\n",
            &["dropped `fleet`: `awareness` is already set (line 2)"],
        ),
        // Unknown keys, bad values, CRLF, a BOM, and no final newline stay.
        (
            "\u{feff}future_key = 1\r\nloop_threshold = many\r\nlearn = on",
            "\u{feff}future_key = 1\r\nloop_threshold = many\r\nmemory = on",
            &[],
        ),
        // V3: the 0.1 `notify` booleans become levels; a level stays.
        (
            "notify = true # pings\n",
            "notify = all # pings\n",
            &["`notify = true` is now `notify = all`"],
        ),
        (
            "notify = \"false\"\n",
            "notify = off\n",
            &["`notify = false`"],
        ),
        ("notify = attention\n", "notify = attention\n", &[]),
        // A `look` key whose value is not look, and `look` text in a comment.
        (
            "guard = deny # look here\nawareness = off # not look\n",
            "guard = deny # look here\nawareness = off # not look\n",
            &[],
        ),
    ];

    #[test]
    fn migrate_table() {
        for (input, want, notes) in TABLE {
            let m = migrate(input).unwrap();
            assert_eq!(m.text, *want, "input {input:?}");
            for n in *notes {
                assert!(
                    m.notes.iter().any(|x| x.contains(n)),
                    "{n} in {:?}",
                    m.notes
                );
            }
            // Idempotent: a second run changes nothing.
            let again = migrate(&m.text).unwrap();
            assert!(again.changes.is_empty(), "{:?}", again.changes);
            assert_eq!(again.text, m.text);
            // What the old file meant, the new one means, with no 0.1 notes left.
            let (before, after) = (
                super::super::parse_full(input),
                super::super::parse_full(want),
            );
            assert_eq!(before.config.fields(), after.config.fields(), "{input:?}");
            assert!(
                after.notes.iter().all(|n| n.old.is_none()),
                "{:?}",
                after.notes
            );
        }
    }

    #[test]
    fn unparsable_file_is_refused() {
        for bad in ["fleet = on\n[section]\n", "= on\n", "just words\n"] {
            let e = migrate(bad).unwrap_err().to_string();
            assert!(e.contains("nothing changed"), "{e}");
        }
    }

    #[test]
    fn diff_shows_each_change() {
        let m = migrate("# c\nfleet = look\nplugin_mode = x\n").unwrap();
        let d = diff(Path::new("/c.toml"), &m);
        assert_eq!(
            d,
            "--- /c.toml\n+++ /c.toml (migrated)\n@@ line 2 @@\n-fleet = look\n+awareness = on\n@@ line 3 @@\n-plugin_mode = x\n"
        );
    }
}
