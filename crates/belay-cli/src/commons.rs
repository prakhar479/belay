//! The commons (C1), the I/O side: each session's `card.json`, written by its daemon;
//! the board (`belay sessions`) that reads every card on the machine.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use belay_core::commons::Card;

use crate::paths;
use crate::sessions::is_live;

/// Sessions that ended this long ago still show with `--all`.
const ENDED_SHOWN_MS: u64 = 24 * 3_600_000;

pub fn card_path(dir: &Path) -> PathBuf {
    dir.join("card.json")
}

pub fn read_card(dir: &Path) -> Option<Card> {
    let text = fs::read(card_path(dir)).ok()?;
    let card: Card = serde_json::from_slice(&text).ok()?;
    (card.card == belay_core::commons::CARD_FORMAT).then_some(card)
}

/// Write `card` atomically (a reader never sees half a card).
pub fn write_card(dir: &Path, card: &Card) -> std::io::Result<()> {
    let tmp = dir.join(format!(".card.json.{}", std::process::id()));
    let text = serde_json::to_vec_pretty(card).map_err(std::io::Error::other)?;
    fs::write(&tmp, text)?;
    fs::rename(&tmp, card_path(dir))
}

/// The card as it must be written, without the parts that change on every event, for
/// "did anything change".
pub fn shape_of(card: &Card) -> String {
    let mut c = card.clone();
    c.last_activity_ms = 0;
    serde_json::to_string(&c).unwrap_or_default()
}

/// Mark this session's card ended (its daemon is stopping).
pub fn mark_ended(dir: &Path) {
    if let Some(mut c) = read_card(dir) {
        // The status stays: a handoff says whether it ended looping or blocked.
        c.ended = true;
        let _ = write_card(dir, &c);
    }
}

/// Every card under `root`, with whether its daemon still answers. Sorted by repo, then
/// name, then id.
pub fn read_cards(root: &Path) -> Vec<(Card, bool)> {
    let mut out: Vec<(Card, bool)> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter_map(|d| {
            let c = read_card(&d)?;
            let live = !c.is_ended() && is_live(&d);
            Some((c, live))
        })
        .collect();
    out.sort_by(|(a, _), (b, _)| (&a.repo, &a.name, &a.id).cmp(&(&b.repo, &b.name, &b.id)));
    out
}

/// Live cards other than `me`.
pub fn live_others<'a>(
    cards: &'a [(Card, bool)],
    me: &'a str,
) -> impl Iterator<Item = &'a Card> + 'a {
    cards
        .iter()
        .filter(move |(c, live)| *live && c.id != me)
        .map(|(c, _)| c)
}

/// The name belay gives a session: its fleet slot, or `<repo dir name>-<n>`, the
/// smallest `n` no live session uses.
pub fn default_name(cards: &[(Card, bool)], me: &str, repo: &str, slot: Option<&str>) -> String {
    let taken: std::collections::BTreeSet<&str> =
        live_others(cards, me).map(|c| c.name.as_str()).collect();
    if let Some(s) = slot {
        let s = s.replace('/', "-");
        if !taken.contains(s.as_str()) {
            return s;
        }
    }
    let base: String = Path::new(repo)
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let base = if base.is_empty() {
        "session".to_string()
    } else {
        base
    };
    (1..)
        .map(|n| format!("{base}-{n}"))
        .find(|n| !taken.contains(n.as_str()))
        .unwrap_or(base)
}

/// The live session that already uses `name`, if one does.
pub fn name_taken(root: &Path, me: &str, name: &str) -> Option<String> {
    let cards = read_cards(root);
    let id = live_others(&cards, me)
        .find(|c| c.name == name)
        .map(|c| c.id.clone());
    id
}

/// One card for its own session (`belay card` with no flags).
pub fn render_card(c: &Card, now: u64, idle_gap_ms: u64) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} ({}) · {}",
        c.name,
        c.id,
        c.status_at(now, idle_gap_ms).name()
    );
    if let Some(t) = &c.said.task {
        let _ = writeln!(out, "task: {t}");
    }
    if let Some(n) = &c.said.note {
        let _ = writeln!(out, "note: {n}");
    }
    if !c.said.offers.is_empty() {
        let _ = writeln!(out, "offers: {}", c.said.offers.join(", "));
    }
    if !c.said.topics.is_empty() {
        let _ = writeln!(out, "topics: {}", c.said.topics.join(", "));
    }
    if let Some(h) = &c.said.handoff {
        let _ = writeln!(out, "handoff: {h}");
    }
    let _ = writeln!(out, "repo: {}", c.repo);
    out
}

/// `belay sessions`: the board. Live cards grouped by repo; with `all`, also sessions
/// that ended or are gone in the last day.
pub fn board(
    cards: &[(Card, bool)],
    now: u64,
    idle_gap_ms: u64,
    all: bool,
    me: Option<&str>,
) -> String {
    let shown: Vec<&(Card, bool)> = cards
        .iter()
        .filter(|(c, live)| {
            *live || (all && now.saturating_sub(c.last_activity_ms) < ENDED_SHOWN_MS)
        })
        .collect();
    let mut out = String::new();
    let live = shown.iter().filter(|(_, l)| *l).count();
    if shown.is_empty() {
        out.push_str("No live belay sessions.\n");
        return out;
    }
    let _ = writeln!(
        out,
        "{live} live belay session{}{}",
        if live == 1 { "" } else { "s" },
        if all && shown.len() > live {
            format!(", {} ended in the last day", shown.len() - live)
        } else {
            String::new()
        }
    );
    let mut group: Option<&str> = None;
    for (c, live) in shown {
        if group != Some(c.repo.as_str()) {
            group = Some(c.repo.as_str());
            let _ = writeln!(out, "\n{}", c.repo);
        }
        let status = if *live {
            c.status_at(now, idle_gap_ms).name()
        } else if c.is_ended() {
            "ended"
        } else {
            "gone"
        };
        let you = if me == Some(c.id.as_str()) {
            "  (you)"
        } else {
            ""
        };
        let mut head = format!("  {}  {status}", c.name);
        if let Some(t) = &c.said.task {
            let _ = write!(head, "  \"{t}\"");
        }
        let _ = writeln!(out, "{head}{you}");
        let mut facts = Vec::new();
        if let Some(t) = &c.last_test {
            let how = match t.passed {
                Some(true) => "passed".to_string(),
                Some(false) if t.failures > 0 => format!("failed ({})", t.failures),
                Some(false) => "failed".to_string(),
                None => "ran".to_string(),
            };
            facts.push(format!(
                "last test `{}` {how} {}",
                t.command,
                ago(now.saturating_sub(t.at_ms))
            ));
        }
        if !c.files.is_empty() {
            let names: Vec<&str> = c.files.iter().take(3).map(|(p, _)| p.as_str()).collect();
            let more = c.files.len().saturating_sub(3);
            facts.push(format!(
                "changed {}{}",
                names.join(", "),
                if more > 0 {
                    format!(" +{more}")
                } else {
                    String::new()
                }
            ));
        }
        if c.loops > 0 {
            facts.push(format!(
                "{} loop{}",
                c.loops,
                if c.loops == 1 { "" } else { "s" }
            ));
        }
        if c.blocks > 0 {
            facts.push(format!(
                "{} blocked edit{}",
                c.blocks,
                if c.blocks == 1 { "" } else { "s" }
            ));
        }
        if !c.said.offers.is_empty() {
            facts.push(format!("offers {}", c.said.offers.join(", ")));
        }
        let unread = unread(&inbox_path(&paths::sessions_dir().join(&c.id)));
        if *live && unread > 0 {
            facts.push(format!("{unread} unread"));
        }
        if !c.said.topics.is_empty() {
            let t: Vec<String> = c.said.topics.iter().map(|t| format!("topic:{t}")).collect();
            facts.push(format!("in {}", t.join(", ")));
        }
        facts.push(format!(
            "active {}",
            ago(now.saturating_sub(c.last_activity_ms))
        ));
        let mut who = format!("{} · {}", c.agent, c.mode);
        if let Some(s) = &c.slot {
            let _ = write!(who, " · fleet slot {s}");
        }
        if c.top != c.repo {
            let _ = write!(who, " · {}", c.top);
        }
        let _ = writeln!(out, "      {}", facts.join(" · "));
        let _ = writeln!(out, "      {who} · {}", c.id);
        if let Some(n) = &c.said.note {
            let _ = writeln!(out, "      note: {n}");
        }
        if let Some(h) = c.said.handoff.as_ref().filter(|_| !*live) {
            let _ = writeln!(out, "      left: {h}");
        }
    }
    out
}

pub(crate) fn ago(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s} s ago"),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=172_799 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86_400),
    }
}

// --- Handoff (C4) --------------------------------------------------------------------

/// The card the next session in `project` starts from: the newest one there that ended
/// (or whose daemon is gone) in the last week, is not a fleet session's, and has
/// something to hand over.
pub fn latest_handoff(
    cards: &[(Card, bool)],
    me: &str,
    project: &str,
    now: u64,
) -> Option<belay_core::commons::Handoff> {
    cards
        .iter()
        .filter(|(c, live)| !*live && c.id != me && c.repo == project && c.fleet.is_none())
        .filter(|(c, _)| {
            now.saturating_sub(c.last_activity_ms) < belay_core::commons::HANDOFF_MAX_AGE_MS
        })
        .map(|(c, _)| belay_core::commons::Handoff::from_card(c))
        .filter(|h| !h.is_empty())
        .max_by(|a, b| {
            a.ended_ms
                .cmp(&b.ended_ms)
                .then_with(|| b.from.cmp(&a.from))
        })
}

// --- Messages (C2) -------------------------------------------------------------------

/// A session's commons inbox.
pub fn inbox_path(session_dir: &Path) -> PathBuf {
    session_dir.join("inbox.jsonl")
}

/// The user's commons inbox (messages to `user` from any session).
pub fn user_inbox() -> PathBuf {
    paths::home().join("inbox.jsonl")
}

/// The cards `to` names, other than `me`: a card name, `topic:<t>`, or `repo` (every
/// live session in `my_repo`). `user` is not a card: the caller handles it.
pub fn resolve<'a>(
    cards: &'a [(Card, bool)],
    me: &'a str,
    my_repo: &str,
    to: &str,
) -> Vec<&'a Card> {
    let others = live_others(cards, me);
    match (to.strip_prefix("topic:"), to) {
        (Some(t), _) => others
            .filter(|c| c.said.topics.iter().any(|x| x == t))
            .collect(),
        (None, "repo") => others.filter(|c| c.repo == my_repo).collect(),
        (None, name) => others.filter(|c| c.name == name).collect(),
    }
}

/// Append one message line to `inbox`, under an flock on `<inbox>.lock`, so senders
/// writing at once lose no line.
pub fn append_message(inbox: &Path, line: &serde_json::Value) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;
    let lock = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{}.lock", inbox.display()))?;
    // SAFETY: flock on an open file; released when `lock` is dropped.
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(inbox)?;
    let mut text = serde_json::to_string(line).map_err(std::io::Error::other)?;
    text.push('\n');
    f.write_all(text.as_bytes())
}

/// A message line as an inbox holds it.
pub fn message_line(
    from: &str,
    from_session: &str,
    repo: &str,
    kind: &str,
    text: &str,
    now: u64,
) -> serde_json::Value {
    serde_json::json!({
        "ts_ms": now,
        "from": from,
        "from_session": from_session,
        "repo": repo,
        "kind": kind,
        "text": text,
        "channel": "commons",
    })
}

/// Lines of `inbox` not yet read: past the `.seen` byte offset (a session's daemon
/// keeps it) or the `.read` line count (the user's inbox).
pub fn unread(inbox: &Path) -> usize {
    let Ok(bytes) = fs::read(inbox) else { return 0 };
    let lines = |b: &[u8]| b.iter().filter(|&&c| c == b'\n').count();
    if let Ok(read) = fs::read_to_string(format!("{}.read", inbox.display())) {
        return lines(&bytes).saturating_sub(read.trim().parse().unwrap_or(0));
    }
    let seen: usize = fs::read_to_string(format!("{}.seen", inbox.display()))
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0);
    lines(&bytes[seen.min(bytes.len())..])
}

/// The user's inbox line count now: mark this many read once they are shown.
pub fn user_lines() -> usize {
    fs::read(user_inbox())
        .map(|b| b.iter().filter(|&&c| c == b'\n').count())
        .unwrap_or(0)
}

/// Mark the first `lines` lines of the user's inbox read: a line that arrives while
/// the inbox is shown stays unread.
pub fn mark_user_read(lines: usize) {
    let _ = fs::write(
        format!("{}.read", user_inbox().display()),
        lines.to_string(),
    );
}

/// `belay msg` from the user's terminal (no belay session): to a card name, a topic, or
/// `repo` (of the current directory). Written straight to the inboxes.
pub fn send_as_user(to: &str, kind: Option<&str>, text: &str) -> anyhow::Result<String> {
    // Only the user's own terminal speaks as the user; any other shell is labelled.
    let from = if paths::users_terminal() {
        "user"
    } else {
        "shell"
    };
    let text = text.trim();
    if text.is_empty() {
        anyhow::bail!("a message needs text");
    }
    let kind = kind.unwrap_or(if text.ends_with('?') {
        "question"
    } else {
        "notice"
    });
    if !belay_core::commons::KINDS.contains(&kind) {
        anyhow::bail!(
            "`{kind}` is not a message kind: use {}",
            belay_core::commons::KINDS.join(", ")
        );
    }
    let root = paths::sessions_dir();
    let cards = read_cards(&root);
    let here = std::env::current_dir().unwrap_or_default();
    let my_repo = crate::learnstore::project_of(&here);
    let to_cards = resolve(&cards, "", &my_repo, to);
    if to_cards.is_empty() {
        return Ok(format!(
            "belay: refused. No live session is `{to}` (`belay sessions` lists them)."
        ));
    }
    let text = belay_core::text::truncate_chars(
        &belay_core::text::redact(text),
        belay_core::commons::MAX_MESSAGE,
    );
    for c in &to_cards {
        let line = message_line(from, "", "", kind, &text, paths::now_ms());
        append_message(&inbox_path(&root.join(&c.id)), &line)?;
    }
    Ok(sent_to(to, &to_cards))
}

/// "Sent to api." / "Sent to 3 sessions in topic review."
pub fn sent_to(to: &str, cards: &[&Card]) -> String {
    let n = cards.len();
    let s = if n == 1 { "" } else { "s" };
    match (cards, to.strip_prefix("topic:")) {
        ([one], None) if to != "repo" => format!("Sent to {}.", one.name),
        (_, None) if to != "repo" => format!("Sent to {n} sessions named {to}."),
        (_, Some(t)) => format!("Sent to {n} session{s} in topic {t}."),
        (_, None) => format!("Sent to {n} session{s} in this repo."),
    }
}

/// `belay sessions` with the default sessions dir.
pub fn board_default(all: bool, json: bool) -> String {
    let cards = read_cards(&paths::sessions_dir());
    let me = std::env::var("BELAY_SESSION_DIR").ok().and_then(|d| {
        Path::new(&d)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
    });
    let gap = crate::config::load().idle_gap_minutes * 60_000;
    let now = paths::now_ms();
    if json {
        let shown: Vec<serde_json::Value> = cards
            .iter()
            .filter(|(c, live)| {
                *live || (all && now.saturating_sub(c.last_activity_ms) < ENDED_SHOWN_MS)
            })
            .map(|(c, live)| {
                let mut v = serde_json::to_value(c).unwrap_or_default();
                v["live"] = (*live).into();
                v["status"] = if *live {
                    c.status_at(now, gap).name()
                } else if c.is_ended() {
                    "ended"
                } else {
                    "gone"
                }
                .into();
                v
            })
            .collect();
        return serde_json::to_string_pretty(&shown).unwrap_or_default() + "\n";
    }
    let mut out = board(&cards, now, gap, all, me.as_deref());
    let mine = unread(&user_inbox());
    if me.is_none() && mine > 0 {
        out.push_str(&format!("\n{mine} unread for you (`belay inbox`).\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use belay_core::commons::CardStatus;
    use belay_core::commons::{AgentCard, LastTest, CARD_FORMAT};

    fn card(id: &str, name: &str, repo: &str) -> Card {
        Card {
            card: CARD_FORMAT,
            id: id.into(),
            name: name.into(),
            agent: "claude-code".into(),
            mode: "optimize".into(),
            started_ms: 1_000,
            repo: repo.into(),
            top: repo.into(),
            fleet: None,
            slot: None,
            said: AgentCard::default(),
            status: CardStatus::Working,
            last_test: None,
            files: vec![],
            tool_calls: 0,
            loops: 0,
            blocks: 0,
            last_activity_ms: 1_000,
            ended: false,
        }
    }

    #[test]
    fn default_names_skip_live_ones_and_prefer_the_slot() {
        let cards = vec![
            (card("a", "atlas-1", "/r/atlas"), true),
            (card("b", "atlas-2", "/r/atlas"), false),
            (card("c", "worker-1", "/r/x"), true),
        ];
        assert_eq!(
            default_name(&cards, "me", "/r/atlas", None),
            "atlas-2",
            "b is not live"
        );
        assert_eq!(
            default_name(&cards, "a", "/r/atlas", None),
            "atlas-1",
            "its own name"
        );
        assert_eq!(
            default_name(&cards, "me", "/r/x", Some("worker-1")),
            "x-1",
            "slot taken"
        );
        assert_eq!(
            default_name(&cards, "me", "/r/x", Some("sub-1/worker-2")),
            "sub-1-worker-2"
        );
        assert_eq!(default_name(&cards, "me", "/r/My Repo", None), "my-repo-1");
    }

    #[test]
    fn a_card_round_trips_and_writes_atomically() {
        let d = std::env::temp_dir().join(format!("belay-card-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let mut c = card("s1", "shop-1", "/r/shop");
        c.said.task = Some("fix the parser".into());
        write_card(&d, &c).unwrap();
        assert_eq!(read_card(&d), Some(c.clone()));
        assert!(fs::read_dir(&d).unwrap().count() == 1, "no temp file left");
        // Only activity changed: the same shape.
        let mut later = c.clone();
        later.last_activity_ms = 9_999;
        assert_eq!(shape_of(&c), shape_of(&later));
        mark_ended(&d);
        let ended = read_card(&d).unwrap();
        assert!(ended.is_ended());
        assert_eq!(
            ended.status,
            CardStatus::Working,
            "the status stays, for a handoff"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_board_groups_by_repo_and_shows_what_each_does() {
        let mut a = card("1", "atlas-1", "/r/atlas");
        a.said.task = Some("refactor the Physics connector".into());
        a.files = vec![("atlas/connectors/physics.py".into(), 2_000)];
        let mut b = card("2", "api", "/r/belay");
        b.last_test = Some(LastTest {
            command: "cargo test".into(),
            passed: Some(false),
            failures: 2,
            at_ms: 1_500,
        });
        let gone = card("3", "old", "/r/belay");
        let cards = vec![(a, true), (b, true), (gone, false)];
        let out = board(&cards, 61_000, 600_000, false, Some("2"));
        assert!(out.starts_with("2 live belay sessions"), "{out}");
        assert!(
            out.contains("/r/atlas\n  atlas-1  working  \"refactor the Physics connector\""),
            "{out}"
        );
        assert!(out.contains("changed atlas/connectors/physics.py"), "{out}");
        assert!(out.contains("api  working  (you)"), "{out}");
        assert!(out.contains("last test `cargo test` failed (2)"), "{out}");
        assert!(!out.contains("old"), "not live: {out}");
        let all = board(&cards, 61_000, 600_000, true, None);
        assert!(all.contains("old  gone"), "{all}");
        // Quiet for longer than the idle gap: idle.
        assert!(board(&cards, 2_000_000, 600_000, false, None).contains("atlas-1  idle"));
    }

    #[test]
    fn the_handoff_is_the_newest_ended_card_here_with_something_in_it() {
        let day = 24 * 3_600_000;
        let now = 30 * day;
        let mk = |id: &str, repo: &str, at: u64, handoff: Option<&str>| {
            let mut c = card(id, id, repo);
            c.last_activity_ms = at;
            c.said.handoff = handoff.map(String::from);
            c
        };
        let mut fleet = mk("fleet", "/r/x", now - 1_000, Some("worker"));
        fleet.fleet = Some("f".into());
        let cards = vec![
            (mk("old", "/r/x", now - 8 * day, Some("too old")), false),
            (
                mk("mid", "/r/x", now - day, Some("open: the parser")),
                false,
            ),
            (mk("empty", "/r/x", now - 2_000, None), false),
            (fleet, false),
            (mk("live", "/r/x", now - 500, Some("still here")), true),
            (
                mk("elsewhere", "/r/y", now - 100, Some("other repo")),
                false,
            ),
        ];
        let h = latest_handoff(&cards, "me", "/r/x", now).unwrap();
        assert_eq!(
            (h.from.as_str(), h.text.as_deref()),
            ("mid", Some("open: the parser"))
        );
        assert!(latest_handoff(&cards, "me", "/r/z", now).is_none());
    }

    #[test]
    fn addresses_resolve_to_live_cards_other_than_me() {
        let mut a = card("1", "api", "/r/belay");
        a.said.topics = vec!["review".into()];
        let b = card("2", "atlas-1", "/r/atlas");
        let mut c = card("3", "web", "/r/belay");
        c.said.topics = vec!["review".into()];
        let gone = card("4", "old", "/r/belay");
        let cards = vec![(a, true), (b, true), (c, true), (gone, false)];
        let names = |v: Vec<&Card>| v.iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(resolve(&cards, "2", "/r/atlas", "api")), ["api"]);
        assert_eq!(
            names(resolve(&cards, "1", "/r/belay", "topic:review")),
            ["web"],
            "not itself"
        );
        assert_eq!(
            names(resolve(&cards, "2", "/r/belay", "repo")),
            ["api", "web"],
            "not the gone one"
        );
        assert!(resolve(&cards, "2", "/r/belay", "old").is_empty());
        assert!(
            resolve(&cards, "1", "/r/belay", "api").is_empty(),
            "not to itself"
        );
    }

    #[test]
    fn concurrent_senders_lose_no_line_and_unread_counts_them() {
        let d = std::env::temp_dir().join(format!("belay-inbox-append-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let inbox = inbox_path(&d);
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let inbox = inbox.clone();
                std::thread::spawn(move || {
                    for j in 0..25 {
                        let l = message_line("api", "1", "/r", "notice", &format!("{i}-{j}"), 1);
                        append_message(&inbox, &l).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let text = fs::read_to_string(&inbox).unwrap();
        let ok = text
            .lines()
            .filter(|l| serde_json::from_str::<serde_json::Value>(l).is_ok())
            .count();
        assert_eq!(ok, 200);
        assert_eq!(unread(&inbox), 200);
        let _ = fs::remove_dir_all(&d);
    }
}
