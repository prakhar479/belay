//! Memory v2: one store for what sessions and the user keep (facts, decisions, gotchas,
//! fixes, handoffs, notes), each with where it came from. Pure: ranking, consolidation,
//! and the token budget. The CLI reads and writes the files (`memorystore.rs`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::commons::{clean, valid_name, words};
use crate::text::{estimate_tokens, sha256_hex};

/// A memory's text, at most, in characters.
pub const MAX_TEXT: usize = 1000;
pub const MAX_TAGS: usize = 8;
pub const MAX_FILES: usize = 8;
/// Live memories a project keeps; past this, the least used are superseded.
pub const MAX_PER_PROJECT: usize = 500;
/// Score parts (before the ×10 that leaves room for recency).
const TAG: u32 = 3;
const FILE: u32 = 2;
const TEXT: u32 = 1;
/// The same failure signature is the strongest evidence a memory fits.
const SIGNATURE: u32 = 10;
/// Tokens a memory costs beyond its text: its id, kind, and line break.
const OVERHEAD_TOKENS: usize = 4;
/// What `superseded_by` says when the project cap, not a duplicate, retired a memory.
pub const CAPPED: &str = "cap";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Session,
    #[default]
    Project,
    User,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Fact,
    Decision,
    Gotcha,
    Fix,
    Handoff,
    #[default]
    Note,
}

impl Kind {
    pub const ALL: [Kind; 6] = [
        Kind::Fact,
        Kind::Decision,
        Kind::Gotcha,
        Kind::Fix,
        Kind::Handoff,
        Kind::Note,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Fact => "fact",
            Kind::Decision => "decision",
            Kind::Gotcha => "gotcha",
            Kind::Fix => "fix",
            Kind::Handoff => "handoff",
            Kind::Note => "note",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == s)
    }
}

/// Who a memory comes from: what belay saw, what an agent said, or what the user said.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    Observed,
    #[default]
    Agent,
    User,
}

impl Provenance {
    pub fn name(self) -> &'static str {
        match self {
            Provenance::Observed => "belay",
            Provenance::Agent => "agent",
            Provenance::User => "user",
        }
    }
}

/// One memory. Every field has a default, so a line from a newer or older build reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Memory {
    pub id: String,
    pub scope: Scope,
    pub kind: Kind,
    pub provenance: Provenance,
    /// The project key (repo root); None for the user's own, cross-project memories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub text: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// A failure signature (`perception::signature`) the memory is about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub created_ms: u64,
    pub last_used_ms: u64,
    pub uses: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    /// The user vouched for it (`belay memory approve`). Provenance stays as it was.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub approved: bool,
}

impl Memory {
    /// A memory as given: text one paragraph, redacted, capped; tags follow the name
    /// rule, sorted; files trimmed. A user's own memory is approved.
    #[allow(clippy::too_many_arguments)] // one field each; a builder would hide the rule
    pub fn new(
        id: &str,
        scope: Scope,
        kind: Kind,
        provenance: Provenance,
        project: Option<&str>,
        text: &str,
        tags: &[String],
        files: &[String],
        now_ms: u64,
    ) -> Memory {
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
        Memory {
            id: id.to_string(),
            scope,
            kind,
            provenance,
            project: project.map(String::from),
            text: clean(text, MAX_TEXT),
            tags,
            files,
            signature: None,
            created_ms: now_ms,
            last_used_ms: now_ms,
            uses: 0,
            superseded_by: None,
            approved: provenance == Provenance::User,
        }
    }

    pub fn is_live(&self) -> bool {
        self.superseded_by.is_none()
    }

    /// The user stands behind it: theirs, or approved by them.
    pub fn vouched(&self) -> bool {
        self.approved || self.provenance == Provenance::User
    }

    fn last_seen_ms(&self) -> u64 {
        self.created_ms.max(self.last_used_ms)
    }
}

/// What to recall: words, tags, files, a failure signature, and the asking project.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Query {
    pub text: String,
    pub tags: Vec<String>,
    pub files: Vec<String>,
    pub signature: Option<String>,
    pub project: Option<String>,
    pub now_ms: u64,
}

/// How well `query` words match one item: a word in a tag counts 3, in a file path 2,
/// in the text 1 (each word counts once, at its best place). Shared with `commons::search`.
pub fn word_score(query: &[String], tags: &[String], files: &[String], text: &str) -> u32 {
    let text = words(text);
    let paths: Vec<String> = files.iter().flat_map(|p| words(p)).collect();
    query
        .iter()
        .map(|w| {
            if tags.iter().any(|t| t == w || t.split('-').any(|p| p == w)) {
                TAG
            } else if paths.contains(w) {
                FILE
            } else if text.contains(w) {
                TEXT
            } else {
                0
            }
        })
        .sum()
}

/// One path names the other: equal, or one ends with `/` and the other.
fn same_file(a: &str, b: &str) -> bool {
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    !short.is_empty()
        && (long == short
            || long.ends_with(short) && long[..long.len() - short.len()].ends_with('/'))
}

/// A small bonus for memories seen lately, 0 to 5: never more than one text match.
fn recency(age_ms: u64) -> u32 {
    const DAY: u64 = 24 * 3_600_000;
    match age_ms / DAY {
        0 => 5,
        1..=6 => 4,
        7..=29 => 3,
        30..=89 => 2,
        90..=364 => 1,
        _ => 0,
    }
}

/// Live memories that fit `q`, best first, with their scores. Ties break by id, so the
/// order does not depend on the input's. A query with no words, tags, files, or
/// signature recalls everything of its project and the user's, newest first.
pub fn rank(memories: &[Memory], q: &Query) -> Vec<(String, u32)> {
    let qwords = words(&q.text);
    let qtags: Vec<String> = q.tags.iter().map(|t| t.trim().to_lowercase()).collect();
    let open = qwords.is_empty() && qtags.is_empty() && q.files.is_empty() && q.signature.is_none();
    let mut out: Vec<(String, u32)> = memories
        .iter()
        .filter(|m| m.is_live())
        .filter_map(|m| {
            let here = q.project.is_some() && m.project == q.project;
            let mut base = word_score(&qwords, &m.tags, &m.files, &m.text);
            base += TAG * qtags.iter().filter(|t| m.tags.contains(t)).count() as u32;
            base += FILE
                * q.files
                    .iter()
                    .filter(|f| m.files.iter().any(|g| same_file(f, g)))
                    .count() as u32;
            if q.signature.is_some() && m.signature == q.signature {
                base += SIGNATURE;
            }
            if open {
                if !(here || m.project.is_none()) {
                    return None;
                }
            } else if base == 0 {
                return None;
            }
            base += u32::from(here) + u32::from(m.vouched());
            let score = base * 10 + recency(q.now_ms.saturating_sub(m.last_seen_ms()));
            Some((m.id.clone(), score))
        })
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// What consolidation retires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// The same text (normalized) as a newer memory of its project and kind.
    Duplicate { id: String, of: String },
    /// Over the project's cap, and among its least used.
    Capped { id: String },
}

impl Change {
    pub fn id(&self) -> &str {
        match self {
            Change::Duplicate { id, .. } | Change::Capped { id } => id,
        }
    }

    pub fn superseded_by(&self) -> &str {
        match self {
            Change::Duplicate { of, .. } => of,
            Change::Capped { .. } => CAPPED,
        }
    }
}

/// Lowercase, whitespace collapsed: two memories with this the same say the same.
fn normalized(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Dedupe and cap with the default cap.
pub fn consolidate(memories: &[Memory]) -> Vec<Change> {
    consolidate_capped(memories, MAX_PER_PROJECT)
}

/// Retire duplicates (the newest of each stays) and, per project, the least used past
/// `cap` (fewest uses, then longest unseen, then id). Only live memories count.
pub fn consolidate_capped(memories: &[Memory], cap: usize) -> Vec<Change> {
    let live: Vec<&Memory> = memories.iter().filter(|m| m.is_live()).collect();
    let mut groups: BTreeMap<(Option<&str>, Kind, String), Vec<&Memory>> = BTreeMap::new();
    for m in &live {
        let key = (
            m.project.as_deref(),
            m.kind,
            sha256_hex(&normalized(&m.text)),
        );
        groups.entry(key).or_default().push(m);
    }
    let mut changes = Vec::new();
    for group in groups.values() {
        let keep = group
            .iter()
            .max_by(|a, b| {
                a.created_ms
                    .cmp(&b.created_ms)
                    .then_with(|| a.id.cmp(&b.id))
            })
            .expect("a group has a member");
        let mut dups: Vec<&&Memory> = group.iter().filter(|m| m.id != keep.id).collect();
        dups.sort_by(|a, b| a.id.cmp(&b.id));
        changes.extend(dups.into_iter().map(|m| Change::Duplicate {
            id: m.id.clone(),
            of: keep.id.clone(),
        }));
    }
    let mut per_project: BTreeMap<Option<&str>, Vec<&Memory>> = BTreeMap::new();
    for m in live
        .iter()
        .filter(|m| !changes.iter().any(|c| c.id() == m.id))
    {
        per_project.entry(m.project.as_deref()).or_default().push(m);
    }
    for mut ms in per_project.into_values() {
        if ms.len() <= cap {
            continue;
        }
        ms.sort_by(|a, b| {
            a.uses
                .cmp(&b.uses)
                .then(a.last_seen_ms().cmp(&b.last_seen_ms()))
                .then_with(|| a.id.cmp(&b.id))
        });
        let over = ms.len() - cap;
        changes.extend(
            ms.into_iter()
                .take(over)
                .map(|m| Change::Capped { id: m.id.clone() }),
        );
    }
    changes
}

/// Mark the memories `changes` retire. Returns how many changed.
pub fn apply(memories: &mut [Memory], changes: &[Change]) -> usize {
    let mut n = 0;
    for c in changes {
        if let Some(m) = memories.iter_mut().find(|m| m.id == c.id() && m.is_live()) {
            m.superseded_by = Some(c.superseded_by().to_string());
            n += 1;
        }
    }
    n
}

/// The ranked memories that fit in `max_tokens`, in rank order. One too big is skipped,
/// and smaller ones after it may still fit.
pub fn budget<'a>(
    ranked: &[(String, u32)],
    memories: &'a [Memory],
    max_tokens: usize,
) -> Vec<&'a Memory> {
    let mut left = max_tokens;
    let mut out = Vec::new();
    for (id, _) in ranked {
        let Some(m) = memories.iter().find(|m| &m.id == id) else {
            continue;
        };
        let cost = estimate_tokens(&m.text) + OVERHEAD_TOKENS;
        if cost <= left {
            left -= cost;
            out.push(m);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem(id: &str, project: Option<&str>, text: &str, at: u64) -> Memory {
        Memory::new(
            id,
            Scope::Project,
            Kind::Gotcha,
            Provenance::Agent,
            project,
            text,
            &[],
            &[],
            at,
        )
    }

    const DAY: u64 = 24 * 3_600_000;

    #[test]
    fn rank_weighs_signature_tags_files_text_project_and_recency() {
        let now = 400 * DAY;
        let mut sig = mem("m-1", Some("/r"), "unrelated words", now - 300 * DAY);
        sig.signature = Some("abc".into());
        let mut tagged = mem("m-2", Some("/r"), "set the mode", now - 300 * DAY);
        tagged.tags = vec!["asyncio".into()];
        let mut filed = mem("m-3", Some("/r"), "the runner", now - 300 * DAY);
        filed.files = vec!["tests/asyncio/conftest.py".into()];
        let texted = mem("m-4", Some("/r"), "asyncio needs a loop", now - 300 * DAY);
        let fresh = mem("m-5", Some("/r"), "asyncio again", now);
        let none = mem("m-6", Some("/r"), "cargo build", now);
        let mut gone = mem("m-7", Some("/r"), "asyncio superseded", now);
        gone.superseded_by = Some("m-5".into());
        let all = vec![sig, tagged, filed, texted, fresh, none, gone];
        let q = Query {
            text: "asyncio".into(),
            signature: Some("abc".into()),
            project: Some("/r".into()),
            now_ms: now,
            ..Query::default()
        };
        let ids: Vec<String> = rank(&all, &q).into_iter().map(|(id, _)| id).collect();
        // Signature (10+1), tag (3+1), file (2+1), fresh text (1+1, recent), text (1+1).
        assert_eq!(ids, ["m-1", "m-2", "m-3", "m-5", "m-4"]);
        // Query files and tags match as such; a path suffix counts.
        let q = Query {
            files: vec!["conftest.py".into()],
            tags: vec!["AsyncIO".into()],
            now_ms: now,
            ..Query::default()
        };
        let ids: Vec<String> = rank(&all, &q).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, ["m-2", "m-3"]);
        // No terms: everything of the project, newest first.
        let q = Query {
            project: Some("/r".into()),
            now_ms: now,
            ..Query::default()
        };
        let ids: Vec<String> = rank(&all, &q).into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, ["m-5", "m-6", "m-1", "m-2", "m-3", "m-4"]);
    }

    #[test]
    fn user_vouched_memories_rank_higher_and_ties_break_by_id() {
        let mut a = mem("m-2", None, "pytest flaky", 0);
        let b = mem("m-1", None, "pytest slow", 0);
        let q = Query {
            text: "pytest".into(),
            ..Query::default()
        };
        let ids = |v: &[Memory]| -> Vec<String> { rank(v, &q).into_iter().map(|r| r.0).collect() };
        assert_eq!(ids(&[a.clone(), b.clone()]), ["m-1", "m-2"]);
        a.approved = true;
        assert_eq!(ids(&[b, a]), ["m-2", "m-1"]);
    }

    #[test]
    fn consolidate_dedupes_within_project_and_kind_and_caps() {
        let a = mem("m-1", Some("/r"), "Use  UV for installs", 1);
        let b = mem("m-2", Some("/r"), "use uv for installs", 2);
        let c = mem("m-3", Some("/other"), "use uv for installs", 3);
        let mut d = mem("m-4", Some("/r"), "use uv for installs", 4);
        d.kind = Kind::Decision;
        let changes = consolidate(&[a.clone(), b.clone(), c.clone(), d.clone()]);
        assert_eq!(
            changes,
            [Change::Duplicate {
                id: "m-1".into(),
                of: "m-2".into()
            }]
        );
        let mut all = vec![a, b, c, d];
        assert_eq!(apply(&mut all, &changes), 1);
        assert_eq!(all[0].superseded_by.as_deref(), Some("m-2"));
        assert!(consolidate(&all).is_empty(), "idempotent");

        // Cap: the fewest uses go first, then the longest unseen.
        let mut ms: Vec<Memory> = (1..=5)
            .map(|i| mem(&format!("m-{i}"), Some("/r"), &format!("text {i}"), i))
            .collect();
        ms[0].uses = 3;
        ms[4].uses = 1;
        let capped = consolidate_capped(&ms, 2);
        let ids: Vec<&str> = capped.iter().map(Change::id).collect();
        assert_eq!(ids, ["m-2", "m-3", "m-4"]);
        assert!(capped.iter().all(|c| c.superseded_by() == CAPPED));
    }

    #[test]
    fn budget_takes_the_best_that_fit() {
        let big = mem("m-1", None, &"x".repeat(400), 0); // 100 + 4 tokens
        let small = mem("m-2", None, "short", 0); // 2 + 4
        let mid = mem("m-3", None, &"y".repeat(40), 0); // 10 + 4
        let all = vec![big, small, mid];
        let ranked = vec![("m-1".into(), 30), ("m-3".into(), 20), ("m-2".into(), 10)];
        let ids = |n| -> Vec<&str> {
            budget(&ranked, &all, n)
                .into_iter()
                .map(|m| m.id.as_str())
                .collect()
        };
        assert_eq!(ids(200), ["m-1", "m-3", "m-2"]);
        assert_eq!(ids(20), ["m-3", "m-2"]);
        assert_eq!(ids(13), ["m-2"]);
        assert!(ids(0).is_empty());
    }

    #[test]
    fn a_memory_reads_from_old_and_new_lines() {
        let m: Memory = serde_json::from_str(r#"{"id":"m-1","text":"hi","later":1}"#).unwrap();
        assert_eq!(
            (m.kind, m.scope, m.provenance),
            (Kind::Note, Scope::Project, Provenance::Agent)
        );
        let line = serde_json::to_string(&m).unwrap();
        assert!(
            !line.contains("approved") && !line.contains("superseded"),
            "{line}"
        );
        let user = Memory::new(
            "m-2",
            Scope::User,
            Kind::Fact,
            Provenance::User,
            None,
            " a\n b ",
            &["Py".into(), "bad tag".into()],
            &[],
            0,
        );
        assert!(user.approved);
        assert_eq!(
            (user.text.as_str(), user.tags.as_slice()),
            ("a b", ["py".to_string()].as_slice())
        );
    }
}
