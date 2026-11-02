//! `belay export`: one session as a sanitized JSON bundle, for a bug report or to share
//! what belay did (I6 E10). Paths under the project become `/project/…`, the home
//! directory `~`, the user name `user`; secrets are redacted (they already are on disk).
//! With `--action a-N --fixture <dir>`: what led to one action, as files to turn into a
//! test fixture when belay got it wrong.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use belay_core::{Body, Entry, Event};
use serde_json::{json, Value};

/// The cleaning `export` applies to a session's text: secrets redacted, the project,
/// home, and user name replaced.
fn cleaner(dir: &Path) -> impl Fn(&str) -> String {
    let cwd = crate::sessions::session_cwd(dir).unwrap_or_default();
    let home = std::env::var("HOME").unwrap_or_default();
    let user = std::env::var("USER").unwrap_or_default();
    move |text: &str| -> String {
        let mut t = belay_core::text::redact(text).into_owned();
        if cwd.len() > 1 {
            t = replace_path(&t, &cwd, "/project");
        }
        if home.len() > 1 {
            t = replace_path(&t, &home, "~");
        }
        if user.len() > 2 {
            t = t.replace(&user, "user");
        }
        t
    }
}

pub fn export(dir: &Path, outputs: bool) -> Result<String> {
    let entries = crate::store::read_entries(dir)?;
    let clean = cleaner(dir);
    // Clean the values, not the JSON text: a short user name (`seq`) must not rename keys.
    let ledger: Vec<Value> = entries
        .iter()
        .filter_map(|e| serde_json::to_value(e).ok())
        .map(|mut v| {
            clean_strings(&mut v, &clean);
            v
        })
        .collect();
    let mut blobs = serde_json::Map::new();
    if outputs {
        for entry in &entries {
            if let belay_core::Body::Output { blob, .. } = &entry.body {
                if let Ok(text) = crate::store::read_blob(dir, blob) {
                    blobs.insert(blob.clone(), Value::String(clean(&text)));
                }
            }
        }
    }
    let bundle = json!({
        "belay_export": 1,
        "belay_version": env!("CARGO_PKG_VERSION"),
        "session": dir.file_name().map(|n| n.to_string_lossy().to_string()),
        "ledger": ledger,
        "outputs": blobs,
    });
    Ok(serde_json::to_string_pretty(&bundle)?)
}

/// What the user is told about a fixture: it is their project's content.
pub const FIXTURE_WARNING: &str =
    "This content comes from your project. Review it before you commit it anywhere public.";

/// Outputs kept per fixture: a loop has many runs, and the newest show it.
const FIXTURE_OUTPUTS: usize = 5;

/// What led to action `id`, written into `out` (new, or empty): the edit before and
/// after, or the outputs, cleaned like `export`, with a README and the action itself.
/// Returns the files written.
pub fn fixture(dir: &Path, id: &str, out: &Path) -> Result<Vec<PathBuf>> {
    let entries = crate::store::read_entries(dir)?;
    let files = fixture_files(
        &entries,
        id,
        &|h| crate::store::read_blob(dir, h).ok(),
        &cleaner(dir),
    )?;
    if out.read_dir().is_ok_and(|mut d| d.next().is_some()) {
        bail!("{} is not empty: name a new directory", out.display());
    }
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let mut written = Vec::new();
    for (name, text) in files {
        let p = out.join(name);
        std::fs::write(&p, text).with_context(|| format!("writing {}", p.display()))?;
        written.push(p);
    }
    Ok(written)
}

/// The fixture's files by name. `blob(hash)` reads a stored text; `clean` is applied to
/// everything from the session.
fn fixture_files(
    entries: &[Entry],
    id: &str,
    blob: &dyn Fn(&str) -> Option<String>,
    clean: &dyn Fn(&str) -> String,
) -> Result<BTreeMap<String, String>> {
    let target = entries
        .iter()
        .find(|e| matches!(&e.body, Body::Action { id: a, .. } if a == id))
        .with_context(|| format!("no action `{id}` in this session"))?;
    let Body::Action { action, reason, .. } = &target.body else {
        unreachable!("found an action");
    };
    let text = |t: &str| -> String {
        let raw = t
            .strip_prefix("<blob:")
            .and_then(|h| h.strip_suffix('>'))
            .and_then(blob)
            .unwrap_or_else(|| t.to_string());
        clean(&raw)
    };
    let by_seq: BTreeMap<u64, &Entry> = entries.iter().map(|e| (e.seq, e)).collect();
    let mut causes = BTreeSet::new();
    let mut stack = target.causes.clone();
    while let Some(seq) = stack.pop() {
        if causes.insert(seq) {
            if let Some(e) = by_seq.get(&seq) {
                stack.extend(&e.causes);
            }
        }
    }
    let mut files = BTreeMap::new();
    let (mut edits, mut outputs) = (0, Vec::new());
    for e in causes.iter().filter_map(|s| by_seq.get(s)) {
        match &e.body {
            Body::Event {
                event:
                    Event::EditProposed {
                        path,
                        old_text,
                        new_text,
                        ..
                    }
                    | Event::FileChanged {
                        path,
                        old_text,
                        new_text,
                        ..
                    },
            } => {
                edits += 1;
                let n = if edits == 1 {
                    String::new()
                } else {
                    format!("-{edits}")
                };
                let ext = Path::new(path)
                    .extension()
                    .map(|x| format!(".{}", x.to_string_lossy()))
                    .unwrap_or_default();
                files.insert(format!("before{n}{ext}"), text(old_text));
                files.insert(format!("after{n}{ext}"), text(new_text));
                files.insert(format!("path{n}.txt"), clean(path) + "\n");
            }
            Body::Output { id, blob: h, .. } => {
                outputs.push((id.clone(), text(&format!("<blob:{h}>"))))
            }
            _ => {}
        }
    }
    for (oid, out) in outputs.into_iter().rev().take(FIXTURE_OUTPUTS) {
        files.insert(format!("output-{oid}.txt"), out);
    }
    let mut readme = String::new();
    let _ = writeln!(readme, "# belay fixture: action {id}\n");
    let _ = writeln!(readme, "{FIXTURE_WARNING}\n");
    let _ = writeln!(readme, "Rule: {}\n", crate::flags::rule_of(action, reason));
    let _ = writeln!(readme, "Reason: {}\n", clean(reason));
    let _ = writeln!(readme, "Files:");
    for name in files.keys() {
        let _ = writeln!(readme, "- {name}");
    }
    if files.is_empty() {
        let _ = writeln!(
            readme,
            "- none: nothing in the ledger led to this action but the action itself"
        );
    }
    let _ = writeln!(readme, "- action.json: the ledger entry");
    let mut entry = serde_json::to_value(target)?;
    clean_strings(&mut entry, clean);
    files.insert(
        "action.json".into(),
        serde_json::to_string_pretty(&entry)? + "\n",
    );
    files.insert("README.md".into(), readme);
    Ok(files)
}

/// Replace `prefix` (a directory, trailing slashes ignored) where it ends at a path
/// boundary: `/work/app` is replaced in `/work/app/x`, not in `/work/app2`.
fn replace_path(text: &str, prefix: &str, with: &str) -> String {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(prefix) {
        let end = i + prefix.len();
        let after = rest[end..].chars().next();
        out.push_str(&rest[..i]);
        if after.is_none_or(|c| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))) {
            out.push_str(with);
        } else {
            out.push_str(prefix);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Apply `clean` to every string in `v`, keys left as they are.
fn clean_strings(v: &mut Value, clean: &dyn Fn(&str) -> String) {
    match v {
        Value::String(s) => *s = clean(s),
        Value::Array(a) => a.iter_mut().for_each(|x| clean_strings(x, clean)),
        Value::Object(o) => o.values_mut().for_each(|x| clean_strings(x, clean)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(seq: u64, causes: Vec<u64>, body: Body) -> Entry {
        Entry {
            seq,
            ts_ms: seq,
            causes,
            body,
        }
    }

    #[test]
    fn a_guard_fixture_has_the_edit_before_and_after_cleaned() {
        let entries = vec![
            e(
                1,
                vec![],
                Body::Event {
                    event: Event::EditProposed {
                        tool_use_id: "t".into(),
                        path: "/w/app/tests/test_x.py".into(),
                        old_text: "<blob:ab>".into(),
                        new_text: "assert x == 2 # /w/app\n".into(),
                    },
                },
            ),
            e(
                2,
                vec![1],
                Body::Action {
                    id: "a-1".into(),
                    action: belay_core::ActionKind::Guard {
                        path: "/w/app/tests/test_x.py".into(),
                        decision: belay_core::guard::GuardDecision::Deny,
                    },
                    reason: "belay: this edit may weaken tests in /w/app/tests/test_x.py: expected value changed to the failing value.".into(),
                    applied: true,
                    unsupported: false,
                },
            ),
        ];
        let blob = |h: &str| (h == "ab").then(|| "assert x == 1\n".to_string());
        let clean = |t: &str| t.replace("/w/app", "/project");
        let f = fixture_files(&entries, "a-1", &blob, &clean).unwrap();
        assert_eq!(f["before.py"], "assert x == 1\n");
        assert_eq!(f["after.py"], "assert x == 2 # /project\n");
        assert_eq!(f["path.txt"], "/project/tests/test_x.py\n");
        assert!(f["README.md"].contains(FIXTURE_WARNING));
        assert!(
            f["README.md"].contains("Rule: guard: expected_to_actual"),
            "{}",
            f["README.md"]
        );
        assert!(!f["action.json"].contains("/w/app"));
        assert!(fixture_files(&entries, "a-9", &blob, &clean).is_err());
    }

    #[test]
    fn paths_are_replaced_at_boundaries_only() {
        assert_eq!(
            replace_path("/w/app/x /w/app2 /w/app", "/w/app/", "/project"),
            "/project/x /w/app2 /project"
        );
    }

    #[test]
    fn a_user_name_that_is_also_a_key_does_not_rename_keys() {
        let mut v = serde_json::json!({"seq": 1, "body": {"path": "/home/seq/x", "seq": "seq"}});
        clean_strings(&mut v, &|t: &str| t.replace("seq", "user"));
        assert_eq!(
            v,
            serde_json::json!({"seq": 1, "body": {"path": "/home/user/x", "seq": "user"}})
        );
    }
}
