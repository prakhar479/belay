//! The commons (C1): every belay session keeps a card that says who it is, what it is
//! doing, and how it is going. belay observes most of it; the agent adds its task, a
//! note, and what it offers. Pure: the daemon writes the card, readers render it.
//! Design: docs/commons-2026-10-22.md.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The card file's format.
pub const CARD_FORMAT: u32 = 1;
/// Longest task, note, and offer an agent may set, in characters.
pub const MAX_TASK: usize = 160;
pub const MAX_NOTE: usize = 200;
pub const MAX_OFFER: usize = 40;
pub const MAX_OFFERS: usize = 8;
/// Recently changed files a card lists.
pub const MAX_FILES: usize = 8;
/// Topics a card joins, at most.
pub const MAX_TOPICS: usize = 8;
/// A handoff's text, at most.
pub const MAX_HANDOFF: usize = 600;
/// A session that ended longer ago than this hands nothing over.
pub const HANDOFF_MAX_AGE_MS: u64 = 7 * 24 * 3_600_000;
/// A commons message's text, at most.
pub const MAX_MESSAGE: usize = 2000;
/// Messages a session may send in a minute: to one session, and in all.
pub const PER_RECIPIENT_PER_MIN: usize = 10;
pub const PER_SENDER_PER_MIN: usize = 30;
/// Message kinds.
pub const KINDS: [&str; 5] = ["question", "answer", "notice", "request", "handoff"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CardStatus {
    /// In a turn, with recent activity.
    Working,
    /// The turn ended: at the prompt, waiting for the user (or the next message).
    Waiting,
    /// An edit was denied, and nothing has gone through since.
    Blocked,
    /// belay steered a loop, and the failure still stands.
    Looping,
    /// In a turn, but nothing for longer than the idle gap.
    Idle,
    /// The session's daemon stopped.
    Ended,
}

impl CardStatus {
    pub fn name(self) -> &'static str {
        match self {
            CardStatus::Working => "working",
            CardStatus::Waiting => "waiting",
            CardStatus::Blocked => "blocked",
            CardStatus::Looping => "looping",
            CardStatus::Idle => "idle",
            CardStatus::Ended => "ended",
        }
    }
}

/// The last test or build run: `passed` is None when its result is not known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastTest {
    pub command: String,
    pub passed: Option<bool>,
    pub failures: usize,
    pub at_ms: u64,
}

/// What the agent says about itself. Shown as its own words.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCard {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub offers: Vec<String>,
    /// Topics it joined: messages to `topic:<t>` reach it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<String>,
    /// What it leaves for the next session here (C4): "done: …; open: …; next: …".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<String>,
}

/// Who the session is, from the daemon: its id, the name belay gave it, and its fleet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub id: String,
    pub default_name: String,
    pub fleet: Option<String>,
    pub slot: Option<String>,
}

/// A session's card, as written to `<session>/card.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub card: u32,
    pub id: String,
    pub name: String,
    pub agent: String,
    pub mode: String,
    pub started_ms: u64,
    /// The repo's main checkout (one name for all its worktrees), or the cwd.
    pub repo: String,
    /// This session's worktree, or the cwd.
    pub top: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    /// The agent's own words.
    #[serde(default)]
    pub said: AgentCard,
    pub status: CardStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_test: Option<LastTest>,
    /// Recently changed files, repo-relative, newest first, with when.
    #[serde(default)]
    pub files: Vec<(String, u64)>,
    pub tool_calls: u64,
    pub loops: u64,
    pub blocks: u64,
    /// The last event of this session (not peers'), for idle and "active … ago".
    pub last_activity_ms: u64,
    /// The session's daemon stopped. The status stays as it was (a handoff says if it
    /// ended looping or blocked).
    #[serde(default)]
    pub ended: bool,
}

impl Card {
    /// Ended: marked so now, or by a build that set the status.
    pub fn is_ended(&self) -> bool {
        self.ended || self.status == CardStatus::Ended
    }

    /// The status a reader shows at `now`: a card is written only when something
    /// happens, so a working session that went quiet reads as idle.
    pub fn status_at(&self, now_ms: u64, idle_gap_ms: u64) -> CardStatus {
        if self.status == CardStatus::Working
            && now_ms.saturating_sub(self.last_activity_ms) > idle_gap_ms
        {
            CardStatus::Idle
        } else {
            self.status
        }
    }
}

/// What the controller knows, for the status.
#[derive(Debug, Clone, Copy, Default)]
pub struct Facts {
    pub turn_open: bool,
    pub last_activity_ms: u64,
    /// The last denied edit, and the last tool call or edit that went through.
    pub last_deny_ms: Option<u64>,
    pub last_progress_ms: u64,
    /// The last loop steer, and whether the last run still failed.
    pub last_steer_ms: Option<u64>,
    pub last_pass_ms: Option<u64>,
    pub last_run_failed: bool,
}

/// The status from the facts, most pressing first: blocked, looping, then the turn.
pub fn status(f: &Facts, now_ms: u64, idle_gap_ms: u64) -> CardStatus {
    if f.last_deny_ms.is_some_and(|d| d >= f.last_progress_ms) {
        return CardStatus::Blocked;
    }
    if f.last_run_failed
        && f.last_steer_ms
            .is_some_and(|s| f.last_pass_ms.is_none_or(|p| s > p))
    {
        return CardStatus::Looping;
    }
    if !f.turn_open {
        return CardStatus::Waiting;
    }
    if now_ms.saturating_sub(f.last_activity_ms) > idle_gap_ms {
        return CardStatus::Idle;
    }
    CardStatus::Working
}

/// How often this session sent: per recipient, and in all. Times in ms, last minute.
#[derive(Debug, Default, Clone)]
pub struct Limiter {
    sent: BTreeMap<String, Vec<u64>>,
    all: Vec<u64>,
}

impl Limiter {
    /// May this session send to `recipients` now? On yes, the sends count.
    pub fn take(&mut self, recipients: &[String], now_ms: u64) -> Result<(), String> {
        let recent = |v: &mut Vec<u64>| v.retain(|&t| now_ms.saturating_sub(t) < 60_000);
        recent(&mut self.all);
        if self.all.len() + recipients.len() > PER_SENDER_PER_MIN {
            return Err(format!(
                "this session sent {} messages in the last minute; the limit is {PER_SENDER_PER_MIN}.",
                self.all.len()
            ));
        }
        for r in recipients {
            let v = self.sent.entry(r.clone()).or_default();
            recent(v);
            if v.len() >= PER_RECIPIENT_PER_MIN {
                return Err(format!(
                    "{r} got {PER_RECIPIENT_PER_MIN} messages from this session in the last minute; wait for an answer."
                ));
            }
        }
        for r in recipients {
            self.sent.entry(r.clone()).or_default().push(now_ms);
            self.all.push(now_ms);
        }
        self.sent.retain(|_, v| !v.is_empty());
        Ok(())
    }
}

/// An agent's text for its card: one line, secrets redacted, at most `max` characters.
pub fn clean(text: &str, max: usize) -> String {
    let one: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let red = crate::text::redact(&one).into_owned();
    crate::text::truncate_chars(&red, max)
}

/// A card name: lowercase letters, digits, `-`, `_`, `.`; 1 to 40 characters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
        && !name.starts_with('.')
        // Addresses `belay msg` reads as keywords, and the labels for the user's words.
        && !matches!(name, "user" | "repo" | "shell")
}

// --- Handoff (C4) --------------------------------------------------------------------

/// What the last session in a repo left, from its card: its own words and what belay saw.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Handoff {
    pub from: String,
    pub ended_ms: u64,
    pub text: Option<String>,
    pub task: Option<String>,
    pub files: Vec<String>,
    pub last_test: Option<LastTest>,
    pub looping: bool,
    pub blocked: bool,
}

impl Handoff {
    pub fn from_card(c: &Card) -> Self {
        Handoff {
            from: c.name.clone(),
            ended_ms: c.last_activity_ms,
            text: c.said.handoff.clone(),
            task: c.said.task.clone(),
            files: c.files.iter().map(|(p, _)| p.clone()).collect(),
            last_test: c.last_test.clone(),
            looping: c.status == CardStatus::Looping,
            blocked: c.status == CardStatus::Blocked,
        }
    }

    /// Whether there is anything to hand over.
    pub fn is_empty(&self) -> bool {
        self.text.is_none()
            && self.task.is_none()
            && self.files.is_empty()
            && self.last_test.is_none()
    }
}

/// The start line for a handoff: only the parts it has.
pub fn handoff_text(h: &Handoff, now_ms: u64) -> String {
    let mins = now_ms.saturating_sub(h.ended_ms) / 60_000;
    let when = match mins {
        0 => "just now".to_string(),
        1..=119 => format!("{mins} min ago"),
        m if m < 48 * 60 => format!("{} h ago", m / 60),
        m => format!("{} days ago", m / (24 * 60)),
    };
    let mut parts = vec![format!(
        "belay: the last session in this repo ({}, ended {when})",
        h.from
    )];
    let mut said = Vec::new();
    if let Some(t) = &h.text {
        said.push(format!("left: {t}"));
    }
    if let Some(t) = &h.task {
        said.push(format!("it was on: {t}"));
    }
    if !h.files.is_empty() {
        let shown: Vec<&str> = h.files.iter().take(5).map(String::as_str).collect();
        let more = h.files.len().saturating_sub(5);
        said.push(format!(
            "it changed {}{}",
            shown.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        ));
    }
    if let Some(t) = &h.last_test {
        let how = match t.passed {
            Some(true) => "passed".to_string(),
            Some(false) if t.failures > 0 => format!("failed ({})", t.failures),
            Some(false) => "failed".to_string(),
            None => "ran".to_string(),
        };
        said.push(format!("its last test `{}` {how}", t.command));
    }
    if h.looping {
        said.push(if h.last_test.is_some() {
            "it was stuck in a loop on that failure".into()
        } else {
            "it was stuck in a loop".into()
        });
    }
    if h.blocked {
        said.push("it had an edit blocked by belay".into());
    }
    parts[0].push_str(&format!(": {}.", said.join("; ")));
    parts.push("Check `git status` before you go on.".into());
    parts.join(" ")
}

// --- Findings (C3) -------------------------------------------------------------------

/// The findings file's format.
pub const FINDING_FORMAT: u32 = 1;
pub const MAX_FINDING: usize = 1000;
pub const MAX_CODE_LINES: usize = 30;
pub const MAX_CODE_CHARS: usize = 2000;
pub const MAX_TAGS: usize = 8;
/// Hits `belay ask` shows, at most.
pub const MAX_HITS: usize = 5;
pub const FINDING_KINDS: [&str; 5] = ["gotcha", "decision", "how", "fix", "note"];

/// Something a session (or the user) published for others to pull (C3). Text crosses
/// projects; `code` is shown only in its own project (owner, 2026-10-22).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub finding: u32,
    pub id: String,
    pub kind: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub project: String,
    /// A card name, or `user`.
    pub by: String,
    #[serde(default)]
    pub session: String,
    pub at_ms: u64,
}

/// A finding as an agent or the user gave it: cleaned to what will be kept. An unknown
/// kind is a note; tags follow the name rule; text is one paragraph, redacted.
#[allow(clippy::too_many_arguments)]
pub fn clean_finding(
    id: &str,
    kind: Option<&str>,
    text: &str,
    tags: &[String],
    files: &[String],
    code: Option<&str>,
    project: &str,
    by: &str,
    session: &str,
    at_ms: u64,
) -> Finding {
    let kind = kind
        .filter(|k| FINDING_KINDS.contains(k))
        .unwrap_or("note")
        .to_string();
    let mut tags: Vec<String> = tags
        .iter()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| valid_name(t))
        .collect();
    tags.sort();
    tags.dedup();
    tags.truncate(MAX_TAGS);
    let files: Vec<String> = files
        .iter()
        .map(|f| crate::text::truncate_chars(f.trim(), 200))
        .filter(|f| !f.is_empty())
        .take(MAX_FILES)
        .collect();
    let code = code
        .map(str::trim_end)
        .filter(|c| !c.trim().is_empty())
        .map(|c| {
            let red = crate::text::redact(c).into_owned();
            let lines: Vec<&str> = red.lines().collect();
            let mut out: String = lines
                .iter()
                .take(MAX_CODE_LINES)
                .map(|l| format!("{l}\n"))
                .collect();
            if lines.len() > MAX_CODE_LINES {
                out.push_str(&format!(
                    "… ({} more lines)\n",
                    lines.len() - MAX_CODE_LINES
                ));
            }
            crate::text::truncate_chars(out.trim_end(), MAX_CODE_CHARS)
        });
    Finding {
        finding: FINDING_FORMAT,
        id: id.to_string(),
        kind,
        text: clean(text, MAX_FINDING),
        tags,
        files,
        code,
        project: project.to_string(),
        by: by.to_string(),
        session: session.to_string(),
        at_ms,
    }
}

/// A search result: the finding as the asker may see it (code only in its project).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hit {
    pub finding: Finding,
    pub here: bool,
    pub score: u32,
}

const STOP: [&str; 24] = [
    "the", "and", "for", "with", "how", "what", "why", "when", "does", "this", "that", "from",
    "are", "was", "not", "can", "use", "run", "get", "set", "into", "out", "has", "have",
];

/// The words of a query or a text: lowercase alphanumeric runs of 3 or more, not stop words.
pub fn words(text: &str) -> Vec<String> {
    let mut w: Vec<String> = text
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|x| x.len() >= 3 && !STOP.contains(x))
        .map(String::from)
        .collect();
    w.sort();
    w.dedup();
    w
}

/// Findings that answer `query`, best first: a query word in a tag counts 3, in a file
/// path 2, in the text 1; this project adds 1; then newer first, then by id.
/// `here_only`: this project's alone. Code from another project is left out.
pub fn search(query: &str, findings: &[Finding], project: &str, here_only: bool) -> Vec<Hit> {
    let q = words(query);
    if q.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<Hit> = findings
        .iter()
        .filter(|f| !here_only || f.project == project)
        .filter_map(|f| {
            let score = crate::memory::word_score(&q, &f.tags, &f.files, &f.text);
            if score == 0 {
                return None;
            }
            let here = f.project == project;
            let mut finding = f.clone();
            if !here {
                finding.code = None;
            }
            Some(Hit {
                finding,
                here,
                score: score + u32::from(here),
            })
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(b.finding.at_ms.cmp(&a.finding.at_ms))
            .then(a.finding.id.cmp(&b.finding.id))
    });
    hits.truncate(MAX_HITS);
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAP: u64 = 600_000;

    #[test]
    fn status_from_facts_most_pressing_first() {
        let base = Facts {
            turn_open: true,
            last_activity_ms: 1_000,
            last_progress_ms: 1_000,
            ..Facts::default()
        };
        assert_eq!(status(&base, 2_000, GAP), CardStatus::Working);
        assert_eq!(status(&base, 1_000 + GAP + 1, GAP), CardStatus::Idle);
        let at_prompt = Facts {
            turn_open: false,
            ..base
        };
        assert_eq!(status(&at_prompt, 2_000, GAP), CardStatus::Waiting);
        // A deny after the last progress: blocked, even at the prompt.
        let denied = Facts {
            last_deny_ms: Some(1_500),
            ..at_prompt
        };
        assert_eq!(status(&denied, 2_000, GAP), CardStatus::Blocked);
        // Progress since the deny: no longer blocked.
        let moved_on = Facts {
            last_progress_ms: 1_600,
            ..denied
        };
        assert_eq!(status(&moved_on, 2_000, GAP), CardStatus::Waiting);
        // A steer, and the run still fails: looping; a pass after the steer ends it.
        let looping = Facts {
            last_steer_ms: Some(1_200),
            last_run_failed: true,
            ..base
        };
        assert_eq!(status(&looping, 2_000, GAP), CardStatus::Looping);
        let passed = Facts {
            last_pass_ms: Some(1_300),
            last_run_failed: false,
            ..looping
        };
        assert_eq!(status(&passed, 2_000, GAP), CardStatus::Working);
    }

    #[test]
    fn agent_text_is_one_line_redacted_and_capped() {
        let t = clean(
            &format!("fix\nthe   parser  token=ghp_{}", "a".repeat(36)),
            MAX_TASK,
        );
        assert!(!t.contains('\n') && !t.contains("ghp_aaaa"), "{t}");
        assert!(t.starts_with("fix the parser"), "{t}");
        assert!(clean(&"x".repeat(500), MAX_TASK).chars().count() <= MAX_TASK + 1);
    }

    #[test]
    fn the_limiter_caps_per_recipient_and_per_sender() {
        let mut l = Limiter::default();
        let one = vec!["api".to_string()];
        for i in 0..PER_RECIPIENT_PER_MIN as u64 {
            assert!(l.take(&one, 1_000 + i).is_ok());
        }
        let e = l.take(&one, 2_000).unwrap_err();
        assert!(e.contains("api got 10 messages"), "{e}");
        // Another recipient is fine; a minute later, api is too.
        assert!(l.take(&["web".to_string()], 2_000).is_ok());
        assert!(l.take(&one, 62_000).is_ok());
        // In all: 30 a minute.
        let mut l = Limiter::default();
        let many: Vec<String> = (0..PER_SENDER_PER_MIN).map(|i| format!("s{i}")).collect();
        assert!(l.take(&many, 1).is_ok());
        assert!(l
            .take(&["x".to_string()], 2)
            .unwrap_err()
            .contains("limit is 30"));
    }

    #[test]
    fn a_handoff_says_only_what_it_has() {
        let h = Handoff {
            from: "atlas-1".into(),
            ended_ms: 0,
            text: Some("open: fix the parser test".into()),
            task: None,
            files: vec!["src/parse.py".into()],
            last_test: Some(LastTest {
                command: "pytest".into(),
                passed: Some(false),
                failures: 2,
                at_ms: 0,
            }),
            looping: false,
            blocked: false,
        };
        let t = handoff_text(&h, 2 * 3_600_000);
        assert_eq!(
            t,
            "belay: the last session in this repo (atlas-1, ended 2 h ago): left: open: fix the parser test; it changed src/parse.py; its last test `pytest` failed (2). Check `git status` before you go on."
        );
        assert!(Handoff::default().is_empty());
    }

    #[test]
    fn names() {
        for ok in ["api", "atlas-2", "w_1.b"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            "API",
            "a b",
            "../x",
            ".hidden",
            "user",
            "repo",
            "shell",
            &"x".repeat(41),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    fn finding(
        id: &str,
        text: &str,
        tags: &[&str],
        files: &[&str],
        project: &str,
        at: u64,
    ) -> Finding {
        let tags: Vec<String> = tags.iter().map(|s| s.to_string()).collect();
        let files: Vec<String> = files.iter().map(|s| s.to_string()).collect();
        clean_finding(
            id,
            Some("gotcha"),
            text,
            &tags,
            &files,
            Some("asyncio_mode = \"auto\""),
            project,
            "atlas-1",
            "s",
            at,
        )
    }

    #[test]
    fn search_ranks_tags_over_files_over_text_and_hides_other_projects_code() {
        let fs = vec![
            finding(
                "f-1",
                "set asyncio_mode in pyproject",
                &["pytest", "asyncio"],
                &[],
                "/r/atlas",
                10,
            ),
            finding("f-2", "pytest needs DB_URL", &[], &[], "/r/belay", 20),
            finding(
                "f-3",
                "the runner",
                &[],
                &["tests/asyncio/conftest.py"],
                "/r/belay",
                30,
            ),
            finding("f-4", "nothing related", &["cargo"], &[], "/r/belay", 40),
        ];
        let hits = search("pytest asyncio error", &fs, "/r/belay", false);
        let ids: Vec<&str> = hits.iter().map(|h| h.finding.id.as_str()).collect();
        assert_eq!(ids, ["f-1", "f-3", "f-2"], "{hits:?}");
        assert!(
            hits[0].finding.code.is_none() && !hits[0].here,
            "other project: no code"
        );
        let mine = search("asyncio", &fs, "/r/atlas", false);
        assert_eq!(
            mine[0].finding.code.as_deref(),
            Some("asyncio_mode = \"auto\"")
        );
        assert_eq!(
            search("asyncio", &fs, "/r/belay", true).len(),
            1,
            "here only"
        );
        assert!(
            search("the and how", &fs, "/r/belay", false).is_empty(),
            "stop words only"
        );
        assert_eq!(
            search("pytest asyncio", &fs, "/r/belay", false),
            search("pytest asyncio", &fs, "/r/belay", false)
        );
    }

    #[test]
    fn a_finding_is_cleaned_to_what_is_kept() {
        let f = clean_finding(
            "f-1",
            Some("shout"),
            &format!("token=ghp_{} and\nmore", "a".repeat(36)),
            &["Py Test".into(), "pytest".into(), "pytest".into()],
            &[],
            Some(&"line\n".repeat(40)),
            "/r",
            "api",
            "s",
            1,
        );
        assert_eq!(f.kind, "note");
        assert!(
            !f.text.contains("ghp_aaaa") && !f.text.contains('\n'),
            "{}",
            f.text
        );
        assert_eq!(f.tags, ["pytest"]);
        let code = f.code.unwrap();
        assert!(code.ends_with("(10 more lines)"), "{code}");
    }

    #[test]
    fn an_ended_card_still_hands_over_how_it_ended() {
        let c = Card {
            card: CARD_FORMAT,
            id: "1".into(),
            name: "x-1".into(),
            agent: "claude-code".into(),
            mode: "optimize".into(),
            started_ms: 0,
            repo: "/r".into(),
            top: "/r".into(),
            fleet: None,
            slot: None,
            said: AgentCard::default(),
            status: CardStatus::Looping,
            last_test: None,
            files: vec!["a.py".into()].into_iter().map(|p| (p, 0)).collect(),
            tool_calls: 0,
            loops: 1,
            blocks: 0,
            last_activity_ms: 0,
            ended: true,
        };
        let h = Handoff::from_card(&c);
        assert!(h.looping);
        let t = handoff_text(&h, 0);
        assert!(
            t.contains("it was stuck in a loop") && !t.contains("on that failure"),
            "{t}"
        );
    }
}
