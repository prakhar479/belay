//! What reaches the session from outside: fleet claims, inbox messages, and learned fixes.

use super::*;

/// Read the fleet's claims again if the file changed (F5c). A missing or bad file
/// leaves the last ones: fail open.
pub(super) fn refresh_claims(st: &mut State) {
    let Some((path, seen)) = &mut st.claims else {
        return;
    };
    let Ok(modified) = fs::metadata(&*path).and_then(|m| m.modified()) else {
        return;
    };
    if *seen == Some(modified) {
        return;
    }
    *seen = Some(modified);
    if let Ok(claims) = fs::read_to_string(&*path).map_err(|_| ()).and_then(|t| {
        serde_json::from_str::<Vec<belay_core::fleet::scope::ClaimFile>>(&t).map_err(|_| ())
    }) {
        st.controller.set_fleet_claims(&claims);
    }
}

/// New messages in this session's fleet inbox become events, so this hook's reply can
/// carry them (F4). A missing file means none yet.
pub(super) fn read_inbox(st: &mut State) -> Result<()> {
    let mut lines = Vec::new();
    let mut read_to = Vec::new();
    for (i, (path, offset, commons)) in st.inbox.iter().enumerate() {
        let (new, next) = new_inbox_lines(path, offset);
        lines.extend(new.into_iter().map(|v| (v, *commons)));
        read_to.push((i, next));
    }
    for (v, commons) in lines {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let opt = |k: &str| Some(s(k)).filter(|x| !x.is_empty());
        let ev = belay_core::Event::Message {
            from: s("from"),
            kind: s("kind"),
            text: s("text"),
            channel: commons.then(|| "commons".to_string()),
            from_session: if commons { opt("from_session") } else { None },
            repo: if commons { opt("repo") } else { None },
        };
        let d = st.controller.handle(ev, now_ms());
        st.store.persist(&d)?;
    }
    // Only once the messages are in the ledger: a failed write delivers them again.
    for (i, next) in read_to {
        let (path, seen, _) = &mut st.inbox[i];
        // Only a new offset or file is written down; a new stamp alone stays in memory.
        if (seen.offset, &seen.head) != (next.offset, &next.head) {
            mark_inbox_seen(path, &next);
        }
        *seen = next;
    }
    Ok(())
}

/// Fixes other sessions learned since the last hook (F2).
pub(super) fn refresh_fixes(st: &mut State) {
    if let Some(seen) = &mut st.fixes_seen {
        let new = crate::learnstore::new_fixes(seen, &st.session_id);
        if !new.is_empty() {
            st.controller.add_known_fixes(new);
        }
    }
}

/// How far an inbox was read, and a fingerprint of the file it was (a hash of its
/// first bytes): an inbox replaced by another file is read from its start, whatever
/// its length. `stamp` (size and mtime, not kept on disk) keeps the hook path at one
/// `stat` while nothing changes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Seen {
    pub(super) offset: u64,
    pub(super) head: Option<String>,
    pub(super) stamp: Option<(u64, i64, i64)>,
}

/// Bytes the fingerprint covers: the start of the first message.
pub(super) const HEAD_BYTES: u64 = 64;

/// Where an earlier daemon of this slot stopped reading its inbox (a restart, a
/// rejoin): old messages are not delivered twice. `.seen` holds "<offset> <head>",
/// or the offset alone (older daemons).
pub(super) fn inbox_seen(path: &Path) -> Seen {
    let text = fs::read_to_string(format!("{}.seen", path.display())).unwrap_or_default();
    let mut w = text.split_whitespace();
    Seen {
        offset: w.next().and_then(|x| x.parse().ok()).unwrap_or(0),
        head: w.next().map(String::from),
        stamp: None,
    }
}

/// The fingerprint of the file's first `n` bytes: what was read, up to `HEAD_BYTES`.
pub(super) fn head_of(f: &mut fs::File, n: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut buf = Vec::new();
    f.seek(SeekFrom::Start(0)).ok()?;
    std::io::Read::by_ref(f)
        .take(n.min(HEAD_BYTES))
        .read_to_end(&mut buf)
        .ok()?;
    Some(belay_core::text::sha256_hex(&String::from_utf8_lossy(&buf))[..16].to_string())
}

/// Whole lines of the inbox past `seen`, and how far they go. The caller keeps it,
/// with `mark_inbox_seen`, once the lines are recorded.
pub(super) fn new_inbox_lines(path: &Path, seen: &Seen) -> (Vec<serde_json::Value>, Seen) {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::MetadataExt;
    // On the hook path: a `stat` when nothing changed, and only the new bytes (plus the
    // first few) when something did.
    let Ok(mut f) = fs::File::open(path) else {
        return (Vec::new(), seen.clone());
    };
    let Ok(meta) = f.metadata() else {
        return (Vec::new(), seen.clone());
    };
    let stamp = Some((meta.len(), meta.mtime(), meta.mtime_nsec()));
    if seen.stamp.is_some() && seen.stamp == stamp {
        return (Vec::new(), seen.clone());
    }
    let len = meta.len();
    // Another file now, or shorter than what was read: it was rewritten. Read it again,
    // or nothing more would ever arrive.
    let replaced =
        seen.head.is_some() && seen.offset <= len && seen.head != head_of(&mut f, seen.offset);
    let offset = if replaced || seen.offset > len {
        0
    } else {
        seen.offset
    };
    let mut buf = Vec::new();
    if offset < len
        && (f.seek(SeekFrom::Start(offset)).is_err() || f.read_to_end(&mut buf).is_err())
    {
        return (Vec::new(), seen.clone());
    }
    let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    // Bytes, then text: one line that is not UTF-8 is skipped, not the rest.
    let lines = buf[..end]
        .split(|&b| b == b'\n')
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect();
    let offset = offset + end as u64;
    let head = (offset > 0).then(|| head_of(&mut f, offset)).flatten();
    (
        lines,
        Seen {
            offset,
            head,
            stamp,
        },
    )
}

/// Keep where this session stopped reading `path`, for the next daemon of the slot.
pub(super) fn mark_inbox_seen(path: &Path, seen: &Seen) {
    let text = match &seen.head {
        Some(h) => format!("{} {h}", seen.offset),
        None => seen.offset.to_string(),
    };
    let _ = fs::write(format!("{}.seen", path.display()), text);
}
