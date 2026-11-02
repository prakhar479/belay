//! Memory v2: one store for what sessions and the user keep (facts, decisions, gotchas,
//! fixes, handoffs, notes), each with where it came from. Pure: ranking, consolidation,
//! and the token budget. The CLI reads and writes the files (`memorystore.rs`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::commons::{clean, valid_name, words, Card};
use crate::learn::KnownFix;
use crate::perception::Failure;
use crate::text::{estimate_tokens, sha256_hex, truncate_chars};

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
    /// The belay session it came from, for handoffs: a newer one replaces it (U4b).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
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
            session: None,
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

// --- Memory in sessions (U4b): handoffs, recall at start, the carry pack ------------

/// A learned fix as a memory: what belay saw, keyed by its failure signature. Its id is
/// stable, so a session can name the fixes it used.
pub fn fix_memory(k: &KnownFix) -> Memory {
    let what = match (&k.test, k.message.is_empty()) {
        (Some(t), true) => t.clone(),
        _ => k.message.clone(),
    };
    let text = format!(
        "`{what}` was fixed by changes to {}; `{}` then passed (learned).",
        k.files.join(", "),
        k.command
    );
    let key = format!("{}|{}|{}|{}", k.signature, k.project, k.session, k.at_ms);
    let mut m = Memory::new(
        &format!("fix-{}", &sha256_hex(&key)[..10]),
        Scope::Project,
        Kind::Fix,
        Provenance::Observed,
        Some(&k.project),
        &text,
        &[],
        &k.files,
        k.at_ms,
    );
    m.signature = Some(k.signature.clone());
    m
}

/// The first line of a failure: its test, then its message's first line.
pub fn failure_line(f: &Failure) -> String {
    let first = f.message.lines().next().unwrap_or("").trim();
    let line = match &f.test {
        Some(t) if !first.is_empty() => format!("{t}: {first}"),
        Some(t) => t.clone(),
        None => first.to_string(),
    };
    truncate_chars(&line, 200)
}

/// The handoff a session leaves at its end or before a compaction, from what belay saw:
/// the card's task, files, and last test, the open failure, and a loop or block. The
/// agent's own `belay handoff` text is added, labelled. None when there is nothing to
/// hand over. The id is empty: the store gives one.
pub fn handoff_from(
    card: &Card,
    failure: Option<&Failure>,
    project: &str,
    now_ms: u64,
) -> Option<Memory> {
    use crate::commons::CardStatus;
    let mut parts = Vec::new();
    if let Some(t) = &card.said.task {
        parts.push(format!("Task: {t}."));
    }
    let files: Vec<String> = card.files.iter().map(|(p, _)| p.clone()).collect();
    if !files.is_empty() {
        let shown: Vec<&str> = files.iter().take(5).map(String::as_str).collect();
        let more = files.len().saturating_sub(5);
        let more = if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        };
        parts.push(format!("Changed: {}{more}.", shown.join(", ")));
    }
    if let Some(t) = &card.last_test {
        let how = match t.passed {
            Some(true) => "passed".to_string(),
            Some(false) if t.failures > 0 => format!("failed ({} failing)", t.failures),
            Some(false) => "failed".to_string(),
            None => "ran".to_string(),
        };
        parts.push(format!("Last test: `{}` {how}.", t.command));
    }
    if let Some(f) = failure {
        parts.push(format!("Open failure: {}.", failure_line(f)));
    }
    match card.status {
        CardStatus::Looping => parts.push("It was looping on a failure.".into()),
        CardStatus::Blocked => parts.push("The test guard had blocked its last edit.".into()),
        _ => {}
    }
    if parts.is_empty() && card.said.handoff.is_none() {
        return None;
    }
    if let Some(t) = &card.said.handoff {
        parts.push(format!("The agent's handoff: {t}"));
    }
    let mut m = Memory::new(
        "",
        Scope::Project,
        Kind::Handoff,
        Provenance::Observed,
        Some(project),
        &format!("From {}. {}", card.name, parts.join(" ")),
        &[],
        &files,
        now_ms,
    );
    m.signature = failure.map(|f| f.signature.clone());
    m.session = Some(card.id.clone()).filter(|s| !s.is_empty());
    Some(m)
}

/// The session a handoff came from: its own field, or an adapted card's `h-<id>`.
fn handoff_session(m: &Memory) -> Option<&str> {
    m.session.as_deref().or_else(|| m.id.strip_prefix("h-"))
}

/// What a session starts with: the newest handoff another session left in the project,
/// then the best other memories, all within `max_tokens`.
#[derive(Debug, Default)]
pub struct Recall<'a> {
    pub handoff: Option<&'a Memory>,
    pub memories: Vec<&'a Memory>,
}

impl Recall<'_> {
    pub fn ids(&self) -> Vec<String> {
        self.handoff
            .iter()
            .chain(&self.memories)
            .map(|m| m.id.clone())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.handoff.is_none() && self.memories.is_empty()
    }
}

fn cost(m: &Memory) -> usize {
    estimate_tokens(&m.text) + OVERHEAD_TOKENS
}

/// The recall at a session's start. Fixes wait for their failure, and only one handoff
/// is shown: the newest one not from `session`.
pub fn recall<'a>(
    memories: &'a [Memory],
    project: &str,
    session: &str,
    max_tokens: usize,
    now_ms: u64,
) -> Recall<'a> {
    let handoff = memories
        .iter()
        .filter(|m| m.is_live() && m.kind == Kind::Handoff)
        .filter(|m| m.project.as_deref() == Some(project))
        .filter(|m| handoff_session(m) != Some(session))
        .filter(|m| cost(m) <= max_tokens)
        .max_by(|a, b| {
            a.created_ms
                .cmp(&b.created_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
    let left = max_tokens - handoff.map_or(0, cost);
    let rest: Vec<Memory> = memories
        .iter()
        .filter(|m| !matches!(m.kind, Kind::Handoff | Kind::Fix))
        .cloned()
        .collect();
    let q = Query {
        project: Some(project.to_string()),
        now_ms,
        ..Query::default()
    };
    let ranked = rank(&rest, &q);
    let ids: Vec<String> = budget(&ranked, &rest, left)
        .into_iter()
        .map(|m| m.id.clone())
        .collect();
    let memories_out = ids
        .iter()
        .filter_map(|id| memories.iter().find(|m| &m.id == id))
        .collect();
    Recall {
        handoff,
        memories: memories_out,
    }
}

/// "5 min ago", "3 h ago", "2 days ago".
pub fn ago(ms: u64) -> String {
    match ms / 60_000 {
        0 => "just now".to_string(),
        m @ 1..=119 => format!("{m} min ago"),
        m if m < 48 * 60 => format!("{} h ago", m / 60),
        m => format!("{} days ago", m / (24 * 60)),
    }
}

/// What the agent reads at its start for a recall.
pub fn recall_text(r: &Recall, now_ms: u64) -> String {
    let mut out = String::from("belay: memory for this project (`belay memory list` shows it all; check that each still fits):");
    if let Some(h) = r.handoff {
        out.push_str(&format!(
            "\n- The last session's handoff ({}, {}): {}",
            h.id,
            ago(now_ms.saturating_sub(h.created_ms)),
            h.text
        ));
    }
    for m in &r.memories {
        out.push_str(&format!(
            "\n- {} ({}, by {}): {}",
            m.id,
            m.kind.name(),
            m.provenance.name(),
            m.text
        ));
    }
    out
}

/// What a compaction may have cut from the agent's context, from this session alone.
#[derive(Debug, Default)]
pub struct Carry<'a> {
    pub task: Option<&'a str>,
    pub failure: Option<&'a Failure>,
    /// Guard blocks and the user's decisions on them, oldest first.
    pub guard: &'a [String],
    /// Memories this session was given, oldest first.
    pub used: &'a [Memory],
    /// The conversation was resumed in a new run (U8), not compacted.
    pub resumed: bool,
}

/// The carry pack: what the session is on, its open failure, guard blocks and
/// decisions, and the memories it used, in that order until `max_tokens`. None when
/// there is nothing to carry.
pub fn carry_text(c: &Carry, max_tokens: usize) -> Option<String> {
    let head = if c.resumed {
        "belay: carried over from this conversation's earlier run (belay's own record):"
    } else {
        "belay: carried over from before the compaction (this session's own state):"
    };
    let mut lines = Vec::new();
    if let Some(t) = c.task {
        lines.push(format!("- Task: {t}"));
    }
    if let Some(f) = c.failure {
        lines.push(format!("- Open failure: {}", failure_line(f)));
    }
    for g in c.guard {
        lines.push(format!("- Guard: {g}"));
    }
    for m in c.used {
        lines.push(format!("- {} ({}): {}", m.id, m.kind.name(), m.text));
    }
    let mut left = max_tokens.saturating_sub(estimate_tokens(head));
    let mut out = head.to_string();
    let mut any = false;
    for l in lines {
        let cost = estimate_tokens(&l) + 1;
        if cost > left {
            continue;
        }
        left -= cost;
        out.push('\n');
        out.push_str(&l);
        any = true;
    }
    any.then_some(out)
}

/// Text the agent loads on its own at its start: its instruction files and its own
/// memory. The daemon reads it through the connector; here it is only data, so the core
/// never names an agent's files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostText {
    /// Where the text is, as the user would name it (a file path).
    pub label: String,
    pub text: String,
}

/// Where a host text already states a memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostHit<'a> {
    pub label: &'a str,
    /// 1-based line of the host text where it says it.
    pub line: usize,
}

/// Tokens a memory needs before containment counts: a two-word memory is contained in
/// too many lines that mean something else.
const HOST_MIN_TOKENS: usize = 3;
/// Content words a memory needs before word overlap counts.
const HOST_MIN_WORDS: usize = 4;
/// Share of the memory's content words one host line must hold, in percent.
const HOST_MEMORY_PCT: usize = 80;
/// Share of that line's content words the memory must hold, in percent: a long line
/// that happens to hold the memory's words says more than the memory.
const HOST_LINE_PCT: usize = 50;

/// Lowercase alphanumeric runs: what containment compares, punctuation and markup aside.
fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_lowercase)
}

/// The first place a host text already states `text`: its tokens in a row (across line
/// breaks), or one line that shares most of its content words and adds few. None when no
/// host text says it. Deterministic: sources in order, then lines in order.
pub fn host_states<'a>(text: &str, host: &'a [HostText]) -> Option<HostHit<'a>> {
    let mine: Vec<String> = tokens(text).collect();
    let my_words = words(text);
    host.iter().find_map(|h| {
        let line = contained(&mine, &h.text).or_else(|| overlapping(&my_words, &h.text))?;
        Some(HostHit {
            label: &h.label,
            line,
        })
    })
}

fn contained(mine: &[String], host: &str) -> Option<usize> {
    if mine.len() < HOST_MIN_TOKENS {
        return None;
    }
    let theirs: Vec<(String, usize)> = host
        .lines()
        .enumerate()
        .flat_map(|(i, l)| tokens(l).map(move |t| (t, i + 1)))
        .collect();
    theirs
        .windows(mine.len())
        .find(|w| w.iter().zip(mine).all(|((t, _), m)| t == m))
        .map(|w| w[0].1)
}

fn overlapping(mine: &[String], host: &str) -> Option<usize> {
    if mine.len() < HOST_MIN_WORDS {
        return None;
    }
    host.lines().enumerate().find_map(|(i, l)| {
        let theirs = words(l);
        let common = mine
            .iter()
            .filter(|w| theirs.binary_search(w).is_ok())
            .count();
        (common * 100 >= HOST_MEMORY_PCT * mine.len()
            && common * 100 >= HOST_LINE_PCT * theirs.len())
        .then_some(i + 1)
    })
}

/// Memories a host text does not already state, and the first one it does for each
/// left out (by label), in order. Fixes stay: they wait for a failure, not the start.
pub fn without_host<'a>(
    memories: &'a [Memory],
    host: &[HostText],
) -> (Vec<&'a Memory>, Vec<String>) {
    let mut kept = Vec::new();
    let mut out = Vec::new();
    for m in memories {
        match host_states(&m.text, host).filter(|_| m.kind != Kind::Fix) {
            Some(hit) => out.push(hit.label.to_string()),
            None => kept.push(m),
        }
    }
    (kept, out)
}

/// "1 memory was left out: CLAUDE.md already says it." for a recall's reason; None when
/// none was.
pub fn host_left_out(labels: &[String]) -> Option<String> {
    let mut names: Vec<&str> = labels.iter().map(String::as_str).collect();
    names.sort();
    names.dedup();
    match labels.len() {
        0 => None,
        1 => Some(format!(
            "1 memory was left out: {} already says it.",
            names[0]
        )),
        n => Some(format!(
            "{n} memories were left out: {} already say them.",
            names.join(", ")
        )),
    }
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

    #[test]
    fn recall_takes_the_newest_handoff_of_another_session_and_carry_fits_its_budget() {
        let mut a = mem("m-1", Some("/r"), "older handoff", 1);
        a.kind = Kind::Handoff;
        a.session = Some("s1".into());
        let mut b = mem("h-s3", Some("/r"), "a card's handoff", 3);
        b.kind = Kind::Handoff;
        let mut own = mem("m-2", Some("/r"), "mine", 4);
        own.kind = Kind::Handoff;
        own.session = Some("s2".into());
        let mut elsewhere = mem("m-5", Some("/x"), "other repo", 9);
        elsewhere.kind = Kind::Handoff;
        let all = vec![a, b, own, elsewhere];
        let r = recall(&all, "/r", "s2", 600, 10);
        assert_eq!(r.handoff.map(|m| m.id.as_str()), Some("h-s3"));
        let r = recall(&all, "/r", "s3", 600, 10);
        assert_eq!(r.handoff.map(|m| m.id.as_str()), Some("m-2"));

        let used = vec![mem("m-9", None, &"z".repeat(400), 1)];
        let guard = vec!["a-1 blocked an edit of t.py.".to_string()];
        let c = Carry {
            task: Some("the task"),
            failure: None,
            guard: &guard,
            used: &used,
            resumed: false,
        };
        let text = carry_text(&c, 40).unwrap();
        assert!(text.contains("- Task: the task") && text.contains("- Guard: a-1"));
        assert!(!text.contains("m-9"), "over the budget: {text}");
        assert!(carry_text(&Carry::default(), 600).is_none());
    }

    #[test]
    fn host_states_by_containment_or_high_overlap_with_one_line() {
        let host = [
            HostText {
                label: "~/.claude/CLAUDE.md".into(),
                text: "# Me\n- Answer in short sentences.\n".into(),
            },
            HostText {
                label: "CLAUDE.md".into(),
                text: "# Repo\n\n- Run `make check` before you say a task\n  is done.\n\
                       - Integration tests need DATABASE_URL pointing at the local postgres instance.\n\
                       - Releases are cut from the main branch by the release script, \
                       which also tags, signs, and uploads every artifact to the registry.\n"
                    .into(),
            },
        ];
        // (memory, where the host says it)
        let table: [(&str, Option<(&str, usize)>); 9] = [
            // Contained, across a line break, case and markup aside.
            (
                "run make check before you say a task is done",
                Some(("CLAUDE.md", 3)),
            ),
            (
                "Answer in short sentences",
                Some(("~/.claude/CLAUDE.md", 2)),
            ),
            // Most of its words, on one line that says little more.
            (
                "integration tests need DATABASE_URL for the local postgres",
                Some(("CLAUDE.md", 5)),
            ),
            // The same words on a much longer line: that line says more.
            ("releases cut from main branch", None),
            // Too short to tell.
            ("make check", None),
            // Half the words: something else.
            ("integration tests flake on the CI postgres runner", None),
            ("pytest needs DB_URL", None),
            ("", None),
            // Its words in another order are not contained, and too few overlap.
            ("done is task a", None),
        ];
        for (text, want) in table {
            let got = host_states(text, &host).map(|h| (h.label, h.line));
            assert_eq!(got, want, "{text}");
        }
        let ms = [
            mem("m-1", None, "Answer in short sentences.", 1),
            mem("m-2", None, "pytest needs DB_URL", 1),
            Memory {
                kind: Kind::Fix,
                ..mem(
                    "m-3",
                    None,
                    "run make check before you say a task is done",
                    1,
                )
            },
        ];
        let (kept, out) = without_host(&ms, &host);
        let kept: Vec<&str> = kept.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            (kept, out.clone()),
            (vec!["m-2", "m-3"], vec!["~/.claude/CLAUDE.md".to_string()])
        );
        assert_eq!(
            host_left_out(&out).as_deref(),
            Some("1 memory was left out: ~/.claude/CLAUDE.md already says it.")
        );
        assert_eq!(host_left_out(&[]), None);
    }
}
