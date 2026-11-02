//! `belay brief`: what a session changed, what it checked, and what to look at before a
//! commit or merge (V1). The facts come from `belay_core::brief`; this adds the git diff
//! from the session's start snapshot, the model tokens, and the text.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;
use belay_core::brief::{build, Brief, GitChange, Input, Tokens};
use belay_core::{Body, Entry, Event};

use crate::{audit, report, sessions, store, undo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Text,
    Markdown,
    Json,
}

/// The brief of the session in `dir` (and the runs it resumed).
pub fn of_session(dir: &Path) -> Result<Brief> {
    let runs = store::read_chain(dir)?;
    let entries = store::chain_entries(&runs);
    let label = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let cwd = runs.iter().find_map(|r| sessions::session_cwd(&r.dir));
    let root = cwd
        .as_deref()
        .and_then(|c| audit::repo_root(Path::new(c)).ok());
    let git = root.as_deref().and_then(|r| git_since_start(r, &runs, dir));
    let base = root
        .as_ref()
        .map(|r| r.to_string_lossy().to_string())
        .or(cwd);
    let blobs = |h: &str| store::read_chain_blob(&runs, h);
    let mut brief = build(&Input {
        session: &label,
        entries: &entries,
        blobs: &blobs,
        root: base.as_deref(),
        git,
    });
    brief.tokens = tokens(&entries);
    Ok(brief)
}

/// The changes from the session's start to the working tree now: the start snapshot
/// (`t-0`, or the first one a resumed chain kept), else the experimental audit's base.
/// None outside git, or with neither.
fn git_since_start(root: &Path, runs: &[store::Run], dir: &Path) -> Option<Vec<GitChange>> {
    let start = runs.iter().find_map(|r| {
        let sid = r.dir.file_name()?.to_string_lossy().to_string();
        let snaps = undo::list(root, &sid).ok()?;
        snaps
            .into_iter()
            .find(|s| s.id.starts_with("t-"))
            .map(|s| s.commit)
    });
    let start = start.or_else(|| {
        let base = std::fs::read_to_string(runs.first()?.dir.join("audit-base")).ok()?;
        let mut lines = base.lines();
        (Path::new(lines.next()?) == root).then(|| lines.next().map(str::to_string))?
    })?;
    let now = audit::worktree_tree(root, dir).ok()?;
    let policy = crate::config::load_policy(root).0;
    let counts = numstat(root, &start, &now).unwrap_or_default();
    let changes = audit::changes(root, &start, &now).ok()?;
    Some(
        changes
            .into_iter()
            .filter(|c| !policy.audit_ignores(&c.path))
            .map(|c| {
                let (added, removed) = counts
                    .iter()
                    .find(|(p, ..)| *p == c.path)
                    .map_or((0, 0), |(_, a, r)| (*a, *r));
                GitChange {
                    change: c,
                    added,
                    removed,
                }
            })
            .collect(),
    )
}

/// Lines added and removed per path. A binary file counts as 0 and 0.
fn numstat(root: &Path, from: &str, to: &str) -> Result<Vec<(String, u64, u64)>> {
    let out = audit::git(
        root,
        &["diff", "--numstat", "-z", "--no-renames", from, to],
        None,
    )?;
    Ok(out
        .split('\0')
        .filter_map(|rec| {
            let mut f = rec.splitn(3, '\t');
            let a = f.next()?.parse().unwrap_or(0);
            let r = f.next()?.parse().unwrap_or(0);
            Some((f.next()?.to_string(), a, r))
        })
        .collect())
}

/// Model tokens from the session's transcripts, one per conversation.
fn tokens(entries: &[Entry]) -> Option<Tokens> {
    let mut paths: Vec<&str> = Vec::new();
    for e in entries {
        if let Body::Event {
            event:
                Event::SessionStarted {
                    transcript_path: Some(t),
                    ..
                }
                | Event::ConversationChanged {
                    transcript_path: Some(t),
                    ..
                },
        } = &e.body
        {
            if !paths.contains(&t.as_str()) {
                paths.push(t);
            }
        }
    }
    let usage = paths
        .iter()
        .filter_map(|t| report::usage_from_transcript(&report::find_transcript(Path::new(t))))
        .fold(None, |acc: Option<Tokens>, u| {
            let a = acc.unwrap_or_default();
            Some(Tokens {
                prompt: a.prompt + u.prompt_total(),
                output: a.output + u.output,
            })
        });
    usage
}

fn header(b: &Brief) -> String {
    let mut h = format!("Session {}", b.session);
    if let Some(ms) = b.duration_ms {
        let _ = write!(h, " · {}", report::minutes(ms));
    }
    if let Some(t) = b.tokens {
        let _ = write!(
            h,
            " · {} prompt tokens, {} output",
            report::k(t.prompt),
            report::k(t.output)
        );
    }
    h
}

fn counts(added: Option<u64>, removed: Option<u64>) -> String {
    match (added, removed) {
        (Some(a), Some(r)) => format!("+{a} −{r}"),
        _ => "edited".into(),
    }
}

fn belay_line(b: &Brief) -> String {
    let d = &b.belay;
    let n = |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    format!(
        "{} ({} tokens kept out) · {} · {} · {}",
        n(d.summaries, "summary", "summaries"),
        report::k(d.tokens_kept_out),
        n(d.guard_blocks, "guard block", "guard blocks"),
        n(d.loops, "loop nudge", "loop nudges"),
        n(d.memories_recalled, "memory recalled", "memories recalled"),
    )
}

/// The exit summary's lines from the brief: Changed, Checked, Look at, indented.
pub fn exit_lines(b: &Brief) -> Vec<String> {
    b.headline().iter().map(|l| format!("  {l}")).collect()
}

pub fn render(b: &Brief, format: Format) -> String {
    match format {
        Format::Json => serde_json::to_string_pretty(b).unwrap_or_default() + "\n",
        Format::Text => text(b),
        Format::Markdown => markdown(b),
    }
}

fn text(b: &Brief) -> String {
    let mut out = format!("{}\n", header(b));
    for line in b.headline() {
        let _ = writeln!(out, "{line}");
    }
    if !b.changed.is_empty() {
        out.push_str("\nChanged\n");
        for c in &b.changed {
            let _ = writeln!(out, "  {}  {}", c.path, counts(c.added, c.removed));
        }
    }
    if !b.checked.is_empty() {
        out.push_str("\nChecked\n");
        for c in &b.checked {
            let _ = writeln!(
                out,
                "  {} {}: {} ({}, {})",
                c.status.mark(),
                c.command,
                c.status.text(),
                runs(c.runs),
                c.output
            );
        }
    }
    if !b.look_at.is_empty() {
        out.push_str("\nLook at\n");
        for (i, l) in b.look_at.iter().enumerate() {
            let _ = writeln!(out, "  {}. {} [{}]", i + 1, l.text, l.evidence);
        }
    }
    let _ = writeln!(out, "\nbelay: {}", belay_line(b));
    out.push_str("`belay why <id>` for any id; `belay show o-N` for an output.\n");
    out
}

fn runs(n: u64) -> String {
    format!("{n} run{}", if n == 1 { "" } else { "s" })
}

fn markdown(b: &Brief) -> String {
    let mut out = String::from("## Summary by belay\n\n");
    let _ = writeln!(out, "{}\n", header(b));
    for line in b.headline() {
        let _ = writeln!(out, "- {line}");
    }
    if !b.changed.is_empty() {
        out.push_str("\n### Changed\n\n| File | Lines |\n| --- | --- |\n");
        for c in &b.changed {
            let _ = writeln!(out, "| `{}` | {} |", c.path, counts(c.added, c.removed));
        }
    }
    if !b.checked.is_empty() {
        out.push_str("\n### Checked\n\n");
        for c in &b.checked {
            let _ = writeln!(
                out,
                "- {} `{}`: {} ({}, `{}`)",
                c.status.mark(),
                c.command,
                c.status.text(),
                runs(c.runs),
                c.output
            );
        }
    }
    if !b.look_at.is_empty() {
        out.push_str("\n### Look at\n\n");
        for (i, l) in b.look_at.iter().enumerate() {
            let _ = writeln!(out, "{}. {} (`{}`)", i + 1, l.text, l.evidence);
        }
    }
    let _ = writeln!(
        out,
        "\n<sub>belay: {}. From belay's ledger, not the agent's summary.</sub>",
        belay_line(b)
    );
    out
}

/// `belay brief`.
pub fn run(session: Option<&str>, format: Format) -> Result<i32> {
    let dir: PathBuf = sessions::resolve(session, false)?;
    print!("{}", render(&of_session(&dir)?, format));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_core::brief::{Changed, Check, CheckStatus, Did, LookAt};

    fn sample() -> Brief {
        Brief {
            session: "s1".into(),
            duration_ms: Some(12 * 60_000),
            tokens: Some(Tokens {
                prompt: 340_000,
                output: 12_000,
            }),
            changed: vec![Changed {
                path: "src/app.py".into(),
                added: Some(3),
                removed: Some(1),
                last_edit: Some(7),
            }],
            checked: vec![Check {
                command: "pytest".into(),
                status: CheckStatus::NotRunSinceLastEdit,
                runs: 2,
                output: "o-2".into(),
            }],
            look_at: vec![LookAt {
                kind: "changed_after_pass".into(),
                text: "src/app.py changed after the last passing run".into(),
                evidence: "7".into(),
            }],
            belay: Did {
                summaries: 2,
                tokens_kept_out: 9_200,
                ..Did::default()
            },
        }
    }

    #[test]
    fn text_starts_with_the_three_lines_and_names_evidence() {
        let t = render(&sample(), Format::Text);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(
            lines[..4],
            [
                "Session s1 · 12 min · 340.0k prompt tokens, 12.0k output",
                "Changed: 1 file (+3 −1)",
                "Checked: ⚠ pytest (not run since the last edit)",
                "Look at: 1 item",
            ]
        );
        assert!(
            t.contains("  1. src/app.py changed after the last passing run [7]"),
            "{t}"
        );
        assert!(
            t.contains("belay: 2 summaries (9.2k tokens kept out) · 0 guard blocks"),
            "{t}"
        );
        assert_eq!(
            exit_lines(&sample())[1],
            "  Checked: ⚠ pytest (not run since the last edit)"
        );
    }

    #[test]
    fn markdown_is_a_pr_section_and_json_has_stable_keys() {
        let m = render(&sample(), Format::Markdown);
        assert!(m.starts_with("## Summary by belay\n"), "{m}");
        assert!(m.contains("| `src/app.py` | +3 −1 |"), "{m}");
        assert!(
            m.contains("- ⚠ `pytest`: not run since the last edit (2 runs, `o-2`)"),
            "{m}"
        );
        let v: serde_json::Value = serde_json::from_str(&render(&sample(), Format::Json)).unwrap();
        assert_eq!(v["checked"][0]["status"], "not_run_since_last_edit");
        assert_eq!(v["tokens"]["prompt"], 340_000);
    }
}
