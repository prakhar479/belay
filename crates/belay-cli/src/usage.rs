//! `belay usage` (V2): where the model's tokens went, from the agent's transcripts.
//!
//! Three parts. Totals: tokens and an estimated cost per project, session or day.
//! Where it went: for sessions belay recorded, the tokens of messages sent inside a
//! loop, after a failing run, or after a repeated read (joined by time). The 5-hour
//! limit: the cost in the 5 hours before each limit hit; their median is the user's
//! usual budget. `belay status` reads that estimate from a cache kept by transcript
//! size, so the status line stays fast.
//!
//! Cost is an estimate from a dated price table: it is never the bill.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use belay_core::record::SteerTrigger;
use belay_core::{ActionKind, Body, Entry, Event, ToolKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::connector::TranscriptSource;
use crate::report::{k, Usage};

/// One model message: when, which model, and its tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageUsage {
    pub at_ms: u64,
    pub id: String,
    pub model: String,
    pub usage: Usage,
}

/// What a transcript says about usage, from some byte offset on.
#[derive(Debug, Default, Clone)]
pub struct UsageLog {
    pub messages: Vec<MessageUsage>,
    /// When the user hit the 5-hour limit (unix ms).
    pub limit_hits: Vec<u64>,
    /// The first working directory the transcript names.
    pub cwd: Option<String>,
    /// The byte offset after the last whole line read.
    pub end: u64,
}

/// The rolling window of a plan's usage limit.
pub const WINDOW_MS: u64 = 5 * 3600 * 1000;

// ---------------------------------------------------------------------------------
// Prices

/// The date of the price table below. `prices` in the config overrides it.
pub const PRICES_AS_OF: &str = "2026-10-06";

/// USD per million tokens. A key matches a model id that contains it; the longest
/// matching key wins. Cache reads are as listed; cache writes are 1.25 × input (the
/// 5-minute cache).
const PRICE_TABLE: &[(&str, f64, f64, f64)] = &[
    ("fable", 10.0, 50.0, 1.0),
    ("claude-fable-5-1", 10.0, 50.0, 0.25),
    ("mythos", 10.0, 50.0, 1.0),
    ("claude-mythos-5-1", 10.0, 50.0, 0.25),
    ("opus", 4.0, 20.0, 0.2),
    ("claude-opus-5", 5.0, 25.0, 0.5),
    ("claude-opus-5-5", 4.0, 20.0, 0.2),
    ("claude-opus-4", 15.0, 75.0, 1.5),
    ("claude-opus-4-5", 5.0, 25.0, 0.5),
    ("claude-opus-4-6", 5.0, 25.0, 0.5),
    ("claude-opus-4-7", 5.0, 25.0, 0.5),
    ("claude-opus-4-8", 5.0, 25.0, 0.5),
    ("sonnet", 2.0, 10.0, 0.2),
    ("claude-sonnet-4", 3.0, 15.0, 0.3),
    ("claude-3-7-sonnet", 3.0, 15.0, 0.3),
    ("haiku", 0.1, 0.5, 0.01),
    ("claude-haiku-4-5", 1.0, 5.0, 0.1),
    ("claude-3-5-haiku", 0.8, 4.0, 0.08),
];

/// The key a model no entry matches is priced as.
const FALLBACK_KEY: &str = "sonnet";

#[derive(Debug, Clone, PartialEq)]
pub struct Price {
    pub key: String,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// `prices = opus 4/20, claude-haiku-5-5 0.1/0.5/0.01/0.125`: a model key, then USD per
/// million input/output tokens, then optionally cache read and cache write (default
/// 0.1 × and 1.25 × input).
pub fn parse_prices(s: &str) -> Result<Vec<Price>> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (key, nums) = part
            .split_once(char::is_whitespace)
            .ok_or_else(|| anyhow!("prices: `{part}` needs a model and in/out prices"))?;
        let nums: Vec<f64> = nums
            .trim()
            .split('/')
            .map(|n| n.trim().parse::<f64>())
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| anyhow!("prices: `{part}`: {e}"))?;
        if !(2..=4).contains(&nums.len()) || nums.iter().any(|n| !n.is_finite() || *n < 0.0) {
            bail!("prices: `{part}` needs in/out[/cache read[/cache write]] in USD per million tokens");
        }
        out.push(Price {
            key: key.trim().to_string(),
            input: nums[0],
            output: nums[1],
            cache_read: nums.get(2).copied().unwrap_or(nums[0] * 0.1),
            cache_write: nums.get(3).copied().unwrap_or(nums[0] * 1.25),
        });
    }
    Ok(out)
}

#[derive(Debug, Clone)]
pub struct Prices(Vec<Price>);

impl Prices {
    /// The table, with the config's entries over it. A bad `prices` value was refused
    /// when the config loaded, so it reads as no override here.
    pub fn new(overrides: &str) -> Prices {
        let mut v: Vec<Price> = PRICE_TABLE
            .iter()
            .map(|&(key, input, output, cache_read)| Price {
                key: key.to_string(),
                input,
                output,
                cache_read,
                cache_write: input * 1.25,
            })
            .collect();
        for p in parse_prices(overrides).unwrap_or_default() {
            v.retain(|q| q.key != p.key);
            v.push(p);
        }
        Prices(v)
    }

    pub fn for_model(&self, model: &str) -> &Price {
        let model = model.to_lowercase();
        self.0
            .iter()
            .filter(|p| model.contains(&p.key.to_lowercase()))
            .max_by_key(|p| p.key.len())
            .or_else(|| self.0.iter().find(|p| p.key == FALLBACK_KEY))
            .unwrap_or(&self.0[0])
    }

    /// Estimated USD.
    pub fn cost(&self, model: &str, u: &Usage) -> f64 {
        let p = self.for_model(model);
        (u.input as f64 * p.input
            + u.output as f64 * p.output
            + u.cache_read as f64 * p.cache_read
            + u.cache_creation as f64 * p.cache_write)
            / 1e6
    }
}

// ---------------------------------------------------------------------------------
// Totals

#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Tally {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub messages: u64,
    /// Estimated USD.
    pub cost_est: f64,
}

impl Tally {
    fn add(&mut self, u: &Usage, cost: f64) {
        self.input += u.input;
        self.output += u.output;
        self.cache_read += u.cache_read;
        self.cache_write += u.cache_creation;
        self.messages += u.messages;
        self.cost_est += cost;
    }

    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

/// Every token of a message: what the plan counts, cache reads included.
fn tokens(u: &Usage) -> u64 {
    u.input + u.output + u.cache_read + u.cache_creation
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum By {
    Project,
    Session,
    Day,
}

/// Totals over every transcript under `root` since `since_ms`, grouped by `by`. A
/// message copied into a resumed conversation's file counts once.
pub fn totals(
    src: &dyn TranscriptSource,
    root: &Path,
    since_ms: u64,
    by: By,
    prices: &Prices,
) -> (Tally, BTreeMap<String, Tally>) {
    let mut total = Tally::default();
    let mut groups: BTreeMap<String, Tally> = BTreeMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    for file in src.list(root) {
        if modified_ms(&file.path).is_some_and(|m| m < since_ms) {
            continue;
        }
        let log = src.usage_log(&file.path, 0);
        for m in log.messages {
            if m.at_ms < since_ms || !seen.insert(m.id.clone()) {
                continue;
            }
            let cost = prices.cost(&m.model, &m.usage);
            total.add(&m.usage, cost);
            let key = match by {
                By::Project => project_of(root, &file.path, log.cwd.as_deref()),
                By::Session => session_of(root, &file.path),
                By::Day => local_day(m.at_ms),
            };
            groups.entry(key).or_default().add(&m.usage, cost);
        }
    }
    (total, groups)
}

fn modified_ms(p: &Path) -> Option<u64> {
    let t = fs::metadata(p).ok()?.modified().ok()?;
    let d = t.duration_since(std::time::UNIX_EPOCH).ok()?;
    u64::try_from(d.as_millis()).ok()
}

/// The transcript's working directory, else its project dir under `root`.
fn project_of(root: &Path, path: &Path, cwd: Option<&str>) -> String {
    if let Some(c) = cwd {
        return c.to_string();
    }
    nth_under(root, path, 0).unwrap_or_else(|| "?".into())
}

/// `<project>/<session>.jsonl`, or a subagent's `<project>/<session>/subagents/x.jsonl`.
fn session_of(root: &Path, path: &Path) -> String {
    nth_under(root, path, 1)
        .map(|s| s.trim_end_matches(".jsonl").to_string())
        .unwrap_or_else(|| "?".into())
}

fn nth_under(root: &Path, path: &Path, n: usize) -> Option<String> {
    path.strip_prefix(root)
        .ok()?
        .components()
        .nth(n)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------------
// Local time (libc: no date crate for three calls).

fn local_tm(ms: u64) -> libc::tm {
    let t = (ms / 1000) as libc::time_t;
    // SAFETY: localtime_r writes only into `tm`, which outlives the call.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    }
}

fn from_tm(mut tm: libc::tm) -> u64 {
    tm.tm_isdst = -1;
    // SAFETY: mktime reads and normalizes `tm`, which we own.
    let t = unsafe { libc::mktime(&mut tm) };
    u64::try_from(t).unwrap_or(0) * 1000
}

/// `2026-11-02` in local time.
pub fn local_day(ms: u64) -> String {
    let tm = local_tm(ms);
    format!(
        "{:04}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday
    )
}

/// Local midnight at the start of the day `ms` falls in.
pub fn local_midnight(ms: u64) -> u64 {
    let mut tm = local_tm(ms);
    tm.tm_hour = 0;
    tm.tm_min = 0;
    tm.tm_sec = 0;
    from_tm(tm)
}

/// `YYYY-MM-DD`: local midnight of that day.
pub fn parse_date(s: &str) -> Result<u64> {
    let bad = || anyhow!("--since takes a date like 2026-11-01, got {s}");
    let mut it = s.trim().split('-').map(|x| x.parse::<i32>().ok());
    let (Some(Some(y)), Some(Some(m)), Some(Some(d)), None) =
        (it.next(), it.next(), it.next(), it.next())
    else {
        return Err(bad());
    };
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(bad());
    }
    // SAFETY: a zeroed tm is a valid value; fields are set below.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = y - 1900;
    tm.tm_mon = m - 1;
    tm.tm_mday = d;
    Ok(from_tm(tm))
}

// ---------------------------------------------------------------------------------
// Where it went

/// Tokens of messages sent in each kind of episode, for sessions belay recorded.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Where {
    /// Sessions whose transcript had messages in the period.
    pub sessions: u64,
    pub total: u64,
    /// From a loop's first failure to belay's steer.
    pub loops: u64,
    /// After a failing tool run, up to the next tool call (not counted in a loop).
    pub failing_runs: u64,
    /// After a read of content already in context, up to the next tool call.
    pub repeated_reads: u64,
    /// Output tokens summaries kept out of context (est., from the ledger).
    pub kept_out: u64,
}

impl Where {
    fn add(&mut self, o: &Where) {
        self.sessions += o.sessions;
        self.total += o.total;
        self.loops += o.loops;
        self.failing_runs += o.failing_runs;
        self.repeated_reads += o.repeated_reads;
        self.kept_out += o.kept_out;
    }
}

/// A run's reaction has no later tool call to end it: it ends this much later.
const OPEN_EPISODE_MS: u64 = 10 * 60 * 1000;

/// Join a session's ledger to its transcript's messages by time. Each message counts
/// in at most one episode: a loop, then a failing run, then a repeated read.
pub fn attribute(entries: &[Entry], msgs: &[MessageUsage]) -> Where {
    // When the agent acted: a message that follows an episode is its reaction until
    // the next of these.
    let mut acts: Vec<u64> = entries
        .iter()
        .filter(|e| {
            matches!(
                &e.body,
                Body::Event {
                    event: Event::ToolRequested { .. }
                        | Event::ToolCompleted { .. }
                        | Event::PromptSubmitted { .. }
                        | Event::TurnEnded
                }
            )
        })
        .map(|e| e.ts_ms)
        .collect();
    acts.sort_unstable();
    let until_next = |ts: u64| -> (u64, u64) {
        let i = acts.partition_point(|&a| a <= ts);
        (ts, acts.get(i).copied().unwrap_or(ts + OPEN_EPISODE_MS))
    };
    let mut loops: Vec<(u64, u64)> = Vec::new();
    let mut failing: Vec<(u64, u64)> = Vec::new();
    let mut repeated: Vec<(u64, u64)> = Vec::new();
    let mut kept_out = 0u64;
    let mut reads_by_seq: BTreeMap<u64, (&str, u64)> = BTreeMap::new();
    let mut reads = belay_core::session::Reads::default();
    for e in entries {
        match &e.body {
            Body::Event {
                event:
                    Event::ToolCompleted {
                        kind,
                        input,
                        failed,
                        ..
                    },
            } => {
                if *kind == ToolKind::Read && !failed {
                    let path = input.get("file_path").and_then(Value::as_str).unwrap_or("");
                    reads_by_seq.insert(e.seq, (path, e.ts_ms));
                } else if *failed && !matches!(kind, ToolKind::Read | ToolKind::Search) {
                    failing.push(until_next(e.ts_ms));
                }
            }
            Body::Event {
                event:
                    Event::Compacted { .. }
                    | Event::ContextRestarted { .. }
                    | Event::ConversationChanged { .. },
            } => reads.on_compacted(),
            Body::Output { blob, .. } => {
                let read = e.causes.first().and_then(|c| reads_by_seq.get(c));
                if let Some(&(path, ts)) = read {
                    if !path.is_empty() && reads.on_read(path, blob) {
                        repeated.push(until_next(ts));
                    }
                }
            }
            Body::Action {
                action:
                    ActionKind::Shape {
                        from_tokens,
                        to_tokens,
                        ..
                    },
                ..
            } => kept_out += from_tokens.saturating_sub(*to_tokens) as u64,
            Body::Action {
                action:
                    ActionKind::Steer {
                        trigger:
                            SteerTrigger::Loop {
                                duration_ms: Some(d),
                                ..
                            },
                        ..
                    },
                ..
            } => loops.push((e.ts_ms.saturating_sub(*d), e.ts_ms)),
            _ => {}
        }
    }
    // A loop includes its own ends; a reaction starts after the run it follows.
    let in_loop = |t: u64| loops.iter().any(|&(a, b)| a <= t && t <= b);
    let after = |w: &[(u64, u64)], t: u64| w.iter().any(|&(a, b)| a < t && t <= b);
    let mut out = Where {
        sessions: u64::from(!msgs.is_empty()),
        kept_out,
        ..Where::default()
    };
    for m in msgs {
        let n = tokens(&m.usage);
        out.total += n;
        if in_loop(m.at_ms) {
            out.loops += n;
        } else if after(&failing, m.at_ms) {
            out.failing_runs += n;
        } else if after(&repeated, m.at_ms) {
            out.repeated_reads += n;
        }
    }
    out
}

/// Transcripts a session's ledger names.
fn transcripts_of(entries: &[Entry]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
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
            if !out.contains(t) {
                out.push(t.clone());
            }
        }
    }
    out
}

/// "Where it went" over every belay session active since `since_ms`. Several sessions
/// can name one transcript (a resume, a restarted daemon): a session takes only the
/// messages of its own time span, and each message counts once.
fn where_it_went(src: &dyn TranscriptSource, since_ms: u64) -> Where {
    let mut all = Where::default();
    let mut seen: HashSet<String> = HashSet::new();
    for dir in crate::paths::list_sessions() {
        let Ok(entries) = crate::store::read_entries(&dir) else {
            continue;
        };
        let (Some(first), Some(last)) = (entries.first(), entries.last()) else {
            continue;
        };
        if last.ts_ms < since_ms {
            continue;
        }
        let span = first.ts_ms.max(since_ms)..=last.ts_ms + OPEN_EPISODE_MS;
        let msgs: Vec<MessageUsage> = transcripts_of(&entries)
            .iter()
            .flat_map(|t| {
                src.usage_log(&crate::report::find_transcript(Path::new(t)), 0)
                    .messages
            })
            .filter(|m| span.contains(&m.at_ms) && seen.insert(m.id.clone()))
            .collect();
        all.add(&attribute(&entries, &msgs));
    }
    all
}

/// "of 1.2M tokens: 180k in loops, 90k after failing runs, 20k after repeated reads;
/// 40k kept out by summaries (est.)".
pub fn where_line(w: &Where) -> String {
    format!(
        "of {} tokens: {} in loops, {} after failing runs, {} after repeated reads; {} kept out by summaries (est.)",
        k(w.total),
        k(w.loops),
        k(w.failing_runs),
        k(w.repeated_reads),
        k(w.kept_out)
    )
}

// ---------------------------------------------------------------------------------
// The 5-hour limit

/// A limit hit, and the estimated cost in the window before it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub at_ms: u64,
    pub cost_est: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Rec {
    at: u64,
    id: String,
    cost: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Mark {
    size: u64,
    end: u64,
}

/// What the last scan learned. Bump `CACHE_VERSION` when its meaning changes.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cache {
    version: u32,
    /// The prices the costs were computed with: other prices start a new scan.
    prices: String,
    files: BTreeMap<String, Mark>,
    /// Messages of the last window: a hit found later needs them.
    recent: Vec<Rec>,
    pub hits: Vec<Hit>,
}

const CACHE_VERSION: u32 = 1;

fn cache_path() -> PathBuf {
    crate::paths::home().join("usage-cache.json")
}

fn prices_key(overrides: &str) -> String {
    format!("{PRICES_AS_OF}|{overrides}")
}

/// Bring the cache up to date: read only what each transcript gained since.
pub fn refresh(
    mut cache: Cache,
    src: &dyn TranscriptSource,
    root: &Path,
    prices: &Prices,
    prices_key: &str,
    now_ms: u64,
) -> Cache {
    if cache.version != CACHE_VERSION || cache.prices != prices_key {
        cache = Cache {
            version: CACHE_VERSION,
            prices: prices_key.to_string(),
            ..Cache::default()
        };
    }
    let mut files = BTreeMap::new();
    let mut recs: Vec<Rec> = Vec::new();
    let mut new_hits: Vec<u64> = Vec::new();
    for file in src.list(root) {
        let key = file.path.to_string_lossy().into_owned();
        let Ok(meta) = fs::metadata(&file.path) else {
            continue;
        };
        let size = meta.len();
        let old = cache.files.get(&key).copied();
        let from = match old {
            Some(m) if m.size == size => {
                files.insert(key, m);
                continue;
            }
            // Shorter than what was read: rewritten, so read it all again.
            Some(m) if size >= m.end => m.end,
            _ => 0,
        };
        let log = src.usage_log(&file.path, from);
        recs.extend(log.messages.iter().map(|m| Rec {
            at: m.at_ms,
            id: m.id.clone(),
            cost: prices.cost(&m.model, &m.usage),
        }));
        new_hits.extend(log.limit_hits);
        files.insert(key, Mark { size, end: log.end });
    }
    cache.files = files;
    // A message read again (a rewritten file, a resumed conversation) replaces itself.
    let fresh: HashSet<&str> = recs.iter().map(|r| r.id.as_str()).collect();
    let mut all: Vec<Rec> = cache
        .recent
        .iter()
        .filter(|r| !fresh.contains(r.id.as_str()))
        .cloned()
        .collect();
    let mut seen = HashSet::new();
    all.extend(recs.into_iter().filter(|r| seen.insert(r.id.clone())));
    all.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.id.cmp(&b.id)));
    new_hits.sort_unstable();
    for hit in new_hits {
        // Hits within one window are one limit: several sessions (a fleet) hit it
        // together, or the user tried again before it reset.
        if cache.hits.iter().any(|h| h.at_ms.abs_diff(hit) < WINDOW_MS) {
            continue;
        }
        let cost_est = window_cost(&all, hit);
        cache.hits.push(Hit {
            at_ms: hit,
            cost_est,
        });
    }
    cache.hits.sort_by_key(|h| h.at_ms);
    let keep_from = now_ms.saturating_sub(WINDOW_MS);
    cache.recent = all.into_iter().filter(|r| r.at > keep_from).collect();
    cache
}

/// Estimated cost of the messages in the window that ends at `end`.
fn window_cost(recs: &[Rec], end: u64) -> f64 {
    let start = end.saturating_sub(WINDOW_MS);
    recs.iter()
        .filter(|r| r.at > start && r.at <= end)
        .map(|r| r.cost)
        .sum()
}

/// The usual cost of a 5-hour window: the median over hits. None under 2 hits.
pub fn estimate(hits: &[Hit]) -> Option<f64> {
    if hits.len() < 2 {
        return None;
    }
    let mut v: Vec<f64> = hits.iter().map(|h| h.cost_est).collect();
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

/// The 5-hour picture now.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Budget {
    pub hits: usize,
    /// Median cost of a window that ended in a hit (est. USD).
    pub usual_est: Option<f64>,
    /// Cost of the last 5 hours (est. USD).
    pub last_5h_est: f64,
}

impl Budget {
    pub fn percent(&self) -> Option<u64> {
        let usual = self.usual_est.filter(|u| *u > 0.0)?;
        Some((self.last_5h_est / usual * 100.0).round() as u64)
    }
}

pub fn budget_of(cache: &Cache, now_ms: u64) -> Budget {
    Budget {
        hits: cache.hits.len(),
        usual_est: estimate(&cache.hits),
        last_5h_est: window_cost(&cache.recent, now_ms),
    }
}

/// Load, refresh and save the cache (a failed save only costs the next call time).
pub fn budget(src: &dyn TranscriptSource, root: &Path, overrides: &str, now_ms: u64) -> Budget {
    let path = cache_path();
    let old: Cache = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let cache = refresh(
        old.clone(),
        src,
        root,
        &Prices::new(overrides),
        &prices_key(overrides),
        now_ms,
    );
    if cache != old {
        if let Ok(bytes) = serde_json::to_vec(&cache) {
            let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
            if fs::create_dir_all(crate::paths::home()).is_ok() && fs::write(&tmp, bytes).is_ok() {
                let _ = fs::rename(&tmp, &path);
            }
        }
    }
    budget_of(&cache, now_ms)
}

/// The status line's segment: " · ~43% of usual 5-hour budget", only with an estimate.
pub fn status_segment(b: &Budget) -> String {
    b.percent()
        .map(|p| format!(" · ~{p}% of usual 5-hour budget"))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------------
// `belay usage`

#[derive(Debug, Clone, Copy)]
pub enum Since {
    Today,
    Week,
    Date(u64),
}

#[derive(Serialize)]
struct Json<'a> {
    since_ms: u64,
    prices_as_of: &'static str,
    total: &'a Tally,
    by: Vec<Group<'a>>,
    where_it_went: &'a Where,
    limit: &'a Budget,
}

#[derive(Serialize)]
struct Group<'a> {
    key: &'a str,
    #[serde(flatten)]
    tally: &'a Tally,
}

pub fn run(since: Since, by: By, json: bool) -> Result<String> {
    let cfg = crate::config::load();
    let now = crate::paths::now_ms();
    let since_ms = match since {
        Since::Today => local_midnight(now),
        Since::Week => local_midnight(now).saturating_sub(6 * 24 * 3600 * 1000),
        Since::Date(ms) => ms,
    };
    let connector = crate::connector::for_agent("claude-code")?;
    let src = connector
        .transcript()
        .ok_or_else(|| anyhow!("this agent keeps no transcripts"))?;
    let root = crate::paths::claude_projects_dir();
    let prices = Prices::new(&cfg.prices);
    let (total, groups) = totals(src.as_ref(), &root, since_ms, by, &prices);
    let went = where_it_went(src.as_ref(), since_ms);
    let limit = budget(src.as_ref(), &root, &cfg.prices, now);
    if json {
        let j = Json {
            since_ms,
            prices_as_of: PRICES_AS_OF,
            total: &total,
            by: groups
                .iter()
                .map(|(key, tally)| Group { key, tally })
                .collect(),
            where_it_went: &went,
            limit: &limit,
        };
        return Ok(serde_json::to_string_pretty(&j)? + "\n");
    }
    Ok(render(since_ms, by, &total, &groups, &went, &limit))
}

fn usd(x: f64) -> String {
    format!("~${x:.2}")
}

pub fn render(
    since_ms: u64,
    by: By,
    total: &Tally,
    groups: &BTreeMap<String, Tally>,
    went: &Where,
    limit: &Budget,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Usage since {}", local_day(since_ms));
    let _ = writeln!(
        out,
        "  {} tokens: {} input · {} output · {} cache read · {} cache write · {} messages",
        k(total.tokens()),
        k(total.input),
        k(total.output),
        k(total.cache_read),
        k(total.cache_write),
        total.messages
    );
    let _ = writeln!(
        out,
        "  Cost: {} (est., prices of {PRICES_AS_OF}; not your bill)",
        usd(total.cost_est)
    );
    if !groups.is_empty() {
        let title = match by {
            By::Project => "By project",
            By::Session => "By session",
            By::Day => "By day",
        };
        let _ = writeln!(out, "{title}:");
        let mut rows: Vec<(&String, &Tally)> = groups.iter().collect();
        if by != By::Day {
            rows.sort_by(|a, b| b.1.cost_est.total_cmp(&a.1.cost_est).then(a.0.cmp(b.0)));
        }
        for (key, t) in rows {
            let _ = writeln!(
                out,
                "  {:>8} tokens  {:>9}  {key}",
                k(t.tokens()),
                usd(t.cost_est)
            );
        }
    }
    if went.sessions > 0 {
        let _ = writeln!(
            out,
            "Where it went ({} belay session{}): {}",
            went.sessions,
            if went.sessions == 1 { "" } else { "s" },
            where_line(went)
        );
    } else {
        let _ = writeln!(out, "Where it went: no belay session in this period.");
    }
    match limit.usual_est {
        Some(usual) => {
            let _ = writeln!(
                out,
                "5-hour limit: hit {} times; usual budget {} (est., median); last 5 hours {} (~{}%)",
                limit.hits,
                usd(usual),
                usd(limit.last_5h_est),
                limit.percent().unwrap_or(0)
            );
        }
        None => {
            let _ = writeln!(
                out,
                "5-hour limit: hit {} time{}; no estimate yet (needs 2). Last 5 hours {}.",
                limit.hits,
                if limit.hits == 1 { "" } else { "s" },
                usd(limit.last_5h_est)
            );
        }
    }
    out
}

#[cfg(test)]
mod tests;
