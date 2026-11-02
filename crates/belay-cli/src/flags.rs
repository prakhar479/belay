//! When belay is wrong: `belay flag`, `belay review`, and the flag lines of
//! `belay report`. Flags are how belay learns its false positives. Nobody flagged
//! anything while it took an id and a note (retro 2026-11-02), so the id and the note
//! are both optional now. The agent may flag too: its flags are labelled as its own and
//! shown apart. A flag lifts nothing (rule 12): it is a note for review.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use belay_core::record::SteerTrigger;
use belay_core::{ActionKind, Body, Entry};

use crate::store::{self, Flag};
use crate::{paths, report, sessions};

/// The rule an action comes from, to group flags by: `guard: <rules>` (guard::rule_of),
/// `loop`, `cycle`, `verify`, or the action's kind.
pub fn rule_of(action: &ActionKind, reason: &str) -> String {
    match action {
        ActionKind::Guard { path, .. } => {
            // The reason is "belay: this edit may weaken tests in <path>: r1; r2." with
            // maybe a sentence after it. Each r is what guard::rule_of reads.
            let list = reason
                .split_once(&format!("{path}: "))
                .map_or(reason, |(_, rest)| rest);
            let list = list.split(". ").next().unwrap_or(list);
            let rules: BTreeSet<&str> = list
                .trim_end_matches('.')
                .split("; ")
                .map(belay_core::guard::rule_of)
                .collect();
            format!(
                "guard: {}",
                rules.into_iter().collect::<Vec<_>>().join(", ")
            )
        }
        ActionKind::Steer {
            trigger: SteerTrigger::Loop { .. },
            ..
        } => "loop".into(),
        ActionKind::Steer {
            trigger: SteerTrigger::Cycle { .. },
            ..
        } => "cycle".into(),
        ActionKind::Verify { .. } => "verify".into(),
        other => serde_json::to_value(other)
            .ok()
            .and_then(|v| v["type"].as_str().map(str::to_string))
            .unwrap_or_else(|| "other".into()),
    }
}

/// What `belay flag` takes as a target; anything else is the start of the note.
fn is_target(s: &str) -> bool {
    let num = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    s == "session"
        || num(s)
        || s.strip_prefix("a-").is_some_and(num)
        || s.strip_prefix("o-").is_some_and(num)
}

/// The target and note of `belay flag [TARGET] [NOTE…]`: with no target, the session's
/// most recent warning.
fn target_and_note(entries: &[Entry], words: Vec<String>) -> Result<(String, String)> {
    let mut words = words.into_iter().peekable();
    let target = match words.next_if(|w| is_target(w)) {
        Some(t) => t,
        None => match report::did(entries).warnings.pop() {
            Some(last) => last,
            None => {
                bail!("no warning in this session to flag; name what was wrong: `belay flag a-N`")
            }
        },
    };
    Ok((target, words.collect::<Vec<_>>().join(" ")))
}

/// `belay flag`: record a flag in `session` (default: the newest of this repo). From
/// the user's terminal it is the user's; anywhere else, the agent's.
pub fn flag(words: Vec<String>, session: Option<&str>) -> Result<String> {
    let dir = sessions::resolve(session, false)?;
    let entries = all_entries(&dir);
    let (target, note) = target_and_note(&entries, words)?;
    let found = report::find_target(&entries, &target);
    if target != "session" && found.is_none() {
        bail!("no entry `{target}` in {}", dir.display());
    }
    let by = if paths::users_terminal() {
        "user"
    } else {
        "agent"
    };
    store::append_flag(
        &dir,
        &Flag {
            ts_ms: paths::now_ms(),
            target: target.clone(),
            note,
            by: by.into(),
        },
    )?;
    let rule = found
        .and_then(rule_of_entry)
        .map(|r| format!(" ({r})"))
        .unwrap_or_default();
    Ok(if by == "user" {
        format!("Flagged {target}{rule}. `belay review` lists all flags.")
    } else {
        format!("Flagged {target}{rule} as the agent's view. It changes nothing; the user sees it in `belay review`.")
    })
}

/// Every run of the session's conversation: ids go on across resumed runs (U8).
fn all_entries(dir: &Path) -> Vec<Entry> {
    store::read_chain(dir)
        .map(|runs| runs.into_iter().flat_map(|r| r.entries).collect())
        .unwrap_or_default()
}

fn rule_of_entry(e: &Entry) -> Option<String> {
    match &e.body {
        Body::Action { action, reason, .. } => Some(rule_of(action, reason)),
        _ => None,
    }
}

/// The flag lines of `belay report`: each flag, then the warnings nobody flagged.
pub fn report_lines(entries: &[Entry], flags: &[Flag], full: bool) -> String {
    let mut out = String::new();
    if full || !flags.is_empty() {
        let _ = writeln!(out, "Flags: {}", flags.len());
    }
    for f in flags {
        let rule = report::find_target(entries, &f.target)
            .and_then(rule_of_entry)
            .map(|r| format!(" ({r})"))
            .unwrap_or_default();
        let by = if f.by == "agent" { " [agent]" } else { "" };
        let note = if f.note.is_empty() {
            String::new()
        } else {
            format!(": {}", f.note)
        };
        let _ = writeln!(out, "  - {}{rule}{by}{note}", f.target);
    }
    let flagged: BTreeSet<&str> = flags.iter().map(|f| f.target.as_str()).collect();
    let open: Vec<String> = report::did(entries)
        .warnings
        .into_iter()
        .filter(|w| !flagged.contains(w.as_str()))
        .collect();
    if let Some(last) = open.last() {
        let _ = writeln!(
            out,
            "Warnings without a flag: {} (wrong? `belay flag {last}`)",
            open.len()
        );
    } else if full {
        out.push_str("Warnings without a flag: 0\n");
    }
    out
}

struct Item {
    session: PathBuf,
    flag: Flag,
    rule: String,
    what: String,
    reason: Option<String>,
}

/// `belay review`: every flag across `sessions`, the user's grouped by rule, then the
/// agent's, newest first, each with a command to look closer and one to make a fixture.
pub fn review(sessions: &[PathBuf]) -> String {
    let mut items = Vec::new();
    for dir in sessions {
        let list = store::read_flags(dir);
        if list.is_empty() {
            continue;
        }
        let entries = all_entries(dir);
        for flag in list {
            let found = report::find_target(&entries, &flag.target);
            let (rule, reason) = match found.map(|e| &e.body) {
                Some(Body::Action { action, reason, .. }) => {
                    (rule_of(action, reason), Some(reason.clone()))
                }
                Some(_) => ("other".into(), None),
                None if flag.target == "session" => ("session".into(), None),
                None => ("other".into(), None),
            };
            let what = match found {
                Some(e) => report::describe_entry(e),
                None => format!("{} (whole session)", flag.target),
            };
            items.push(Item {
                session: dir.clone(),
                flag,
                rule,
                what,
                reason,
            });
        }
    }
    render_review(items)
}

fn render_review(mut items: Vec<Item>) -> String {
    // Newest first; ties in a stable order.
    items.sort_by(|a, b| {
        b.flag
            .ts_ms
            .cmp(&a.flag.ts_ms)
            .then_with(|| a.session.cmp(&b.session))
            .then_with(|| a.flag.target.cmp(&b.flag.target))
    });
    let sessions: BTreeSet<&Path> = items.iter().map(|i| i.session.as_path()).collect();
    let agent = items.iter().filter(|i| i.flag.by == "agent").count();
    let mut out = format!("{} flags in {} sessions", items.len(), sessions.len());
    if agent > 0 {
        let _ = write!(out, " ({agent} from the agent)");
    }
    out.push('\n');
    let (users, agents): (Vec<&Item>, Vec<&Item>) =
        items.iter().partition(|i| i.flag.by != "agent");
    groups(&mut out, &users);
    if !agents.is_empty() {
        out.push_str(
            "\nFrom the agent: its view, not yours. A flag changes nothing until you act on it.\n",
        );
        groups(&mut out, &agents);
    }
    out
}

/// `items` (newest first) grouped by rule, the group with the newest flag first.
fn groups(out: &mut String, items: &[&Item]) {
    let mut order: Vec<&str> = Vec::new();
    let mut by_rule: BTreeMap<&str, Vec<&Item>> = BTreeMap::new();
    for i in items {
        if !by_rule.contains_key(i.rule.as_str()) {
            order.push(&i.rule);
        }
        by_rule.entry(&i.rule).or_default().push(i);
    }
    for rule in order {
        let list = &by_rule[rule];
        let _ = writeln!(out, "\n{rule} · {}", list.len());
        for i in list {
            let s = i.session.display();
            let target = &i.flag.target;
            let label = i
                .session
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            let _ = writeln!(out, "  - {target} in {label}: {}", short(&i.what));
            if let Some(r) = &i.reason {
                let _ = writeln!(out, "    reason: {}", short(r));
            }
            if !i.flag.note.is_empty() {
                let _ = writeln!(out, "    note: {}", i.flag.note);
            }
            let look = if target.starts_with("o-") {
                format!("belay show {target} --session {s}")
            } else if target == "session" {
                format!("belay report --session {s}")
            } else {
                format!("belay why {target} --session {s}")
            };
            let _ = writeln!(out, "    look: {look}");
            if target.starts_with("a-") {
                let _ = writeln!(
                    out,
                    "    fixture: belay export --action {target} --fixture flagged-{target} --session {s}"
                );
            }
        }
    }
}

fn short(s: &str) -> String {
    belay_core::text::truncate_chars(s, 200)
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_core::guard::GuardDecision;

    fn action(seq: u64, id: &str, action: ActionKind, reason: &str) -> Entry {
        Entry {
            seq,
            ts_ms: seq,
            causes: vec![],
            body: Body::Action {
                id: id.into(),
                action,
                reason: reason.into(),
                applied: true,
                unsupported: false,
            },
        }
    }

    fn guard(seq: u64, id: &str, why: &str) -> Entry {
        action(
            seq,
            id,
            ActionKind::Guard {
                path: "t.py".into(),
                decision: GuardDecision::Deny,
            },
            &format!("belay: this edit may weaken tests in t.py: {why}. If the user wants it anyway, they can run `! belay allow`."),
        )
    }

    fn flag(ts_ms: u64, target: &str, note: &str, by: &str) -> Flag {
        Flag {
            ts_ms,
            target: target.into(),
            note: note.into(),
            by: by.into(),
        }
    }

    #[test]
    fn rules_name_the_guard_rule_or_the_action() {
        let g = guard(1, "a-1", "skip marker added; assertions removed (2 → 1)");
        assert_eq!(
            rule_of_entry(&g).unwrap(),
            "guard: assertions_removed, skip"
        );
        let l = action(
            2,
            "a-2",
            ActionKind::Steer {
                trigger: SteerTrigger::Loop {
                    signatures: vec![],
                    duration_ms: None,
                },
                text: String::new(),
            },
            "",
        );
        assert_eq!(rule_of_entry(&l).unwrap(), "loop");
        let b = action(
            3,
            "a-3",
            ActionKind::BlockCommand {
                command: "x".into(),
            },
            "",
        );
        assert_eq!(rule_of_entry(&b).unwrap(), "block_command");
    }

    #[test]
    fn flag_with_no_id_takes_the_newest_warning_and_keeps_the_note() {
        let entries = [
            guard(1, "a-1", "skip marker added"),
            guard(2, "a-2", "skip marker added"),
        ];
        let w = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            target_and_note(&entries, w(&[])).unwrap(),
            ("a-2".into(), String::new())
        );
        assert_eq!(
            target_and_note(&entries, w(&["this", "is", "fine"])).unwrap(),
            ("a-2".into(), "this is fine".into())
        );
        assert_eq!(
            target_and_note(&entries, w(&["a-1"])).unwrap(),
            ("a-1".into(), String::new())
        );
        assert!(target_and_note(&[], w(&[])).is_err());
    }

    #[test]
    fn review_groups_by_rule_newest_first_with_the_agents_apart() {
        let s = PathBuf::from("/s/one");
        let item = |ts, target: &str, rule: &str, by| Item {
            session: s.clone(),
            flag: flag(
                ts,
                target,
                if by == "agent" {
                    "the test was wrong"
                } else {
                    ""
                },
                by,
            ),
            rule: rule.into(),
            what: format!("action {target}"),
            reason: Some("r".into()),
        };
        let out = render_review(vec![
            item(1, "a-1", "guard: skip", "user"),
            item(3, "a-3", "loop", "user"),
            item(2, "a-2", "guard: skip", "user"),
            item(4, "a-4", "guard: skip", "agent"),
        ]);
        assert!(
            out.starts_with("4 flags in 1 sessions (1 from the agent)\n"),
            "{out}"
        );
        let at = |t: &str| out.find(t).unwrap_or_else(|| panic!("{t} in {out}"));
        assert!(at("\nloop · 1") < at("\nguard: skip · 2"), "{out}");
        assert!(at("- a-2 ") < at("- a-1 "), "{out}");
        assert!(at("From the agent") < at("- a-4 "), "{out}");
        assert!(out
            .contains("fixture: belay export --action a-1 --fixture flagged-a-1 --session /s/one"));
        assert!(out.contains("note: the test was wrong"));
    }

    #[test]
    fn report_lists_flags_and_counts_warnings_nobody_flagged() {
        let entries = [
            guard(1, "a-1", "skip marker added"),
            guard(2, "a-2", "skip marker added"),
        ];
        let out = report_lines(&entries, &[flag(5, "a-1", "", "agent")], false);
        assert!(
            out.contains("Flags: 1\n  - a-1 (guard: skip) [agent]\n"),
            "{out}"
        );
        assert!(
            out.contains("Warnings without a flag: 1 (wrong? `belay flag a-2`)"),
            "{out}"
        );
        assert_eq!(report_lines(&[], &[], false), "");
    }
}
