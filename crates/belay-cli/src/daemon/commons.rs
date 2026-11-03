//! Commons requests (`belay msg`, `share`, `ask`, `card`, `handoff`) sent to the daemon.

use super::*;

pub(super) const COMMONS_OFF: &str = "belay: refused. `commons = off` in belay's config: this session keeps no card, and sends and shares nothing.";

/// A commons request (`belay msg`, `share`, `ask`, `card`, `handoff`): the lock only
/// around the controller; cards, inboxes, findings, and hubs outside it.
pub(super) fn commons_request(state: &Mutex<State>, req: Request) -> Result<String> {
    if CARD_DIR.get().is_none() {
        return Ok(COMMONS_OFF.into());
    }
    // Requests carry who sends them (identity, not authentication): a shell of another
    // session that points BELAY_SOCKET here would act as this session.
    let from = match &req {
        Request::Send { from, .. }
        | Request::Share { from, .. }
        | Request::Ask { from, .. }
        | Request::Card { from, .. } => from.clone(),
        _ => None,
    };
    if let Some(from) = from {
        let me = lock(state).session_id.clone();
        if from != me {
            return Ok(format!(
                "belay: refused. This socket is session {me}'s, and the request came from session {from}. Run belay from that session's own shell."
            ));
        }
    }
    match req {
        Request::Send { to, kind, text, .. } => send_commons(state, &to, kind, &text),
        Request::Share {
            kind,
            text,
            tags,
            files,
            code,
            ..
        } => {
            if text.trim().is_empty() {
                return Ok("belay: refused. A finding needs text.".into());
            }
            let now = now_ms();
            let (me, project) = my_card_and_project(state, now);
            let f = crate::findings::add_in(&crate::findings::dir(), |id| {
                belay_core::commons::clean_finding(
                    id,
                    kind.as_deref(),
                    &text,
                    &tags,
                    &files,
                    code.as_deref(),
                    &project,
                    &me.name,
                    &me.id,
                    now,
                )
            })?;
            {
                let mut st = lock(state);
                let d = st.controller.shared(&f, now);
                st.store.persist(&d)?;
            }
            Ok(format!("Shared {}. Others find it with `belay ask`.", f.id))
        }
        Request::Ask {
            query, here, json, ..
        } => {
            let now = now_ms();
            let (me, project) = my_card_and_project(state, now);
            let hits =
                belay_core::commons::search(&query, &crate::findings::searchable(), &project, here);
            {
                let mut st = lock(state);
                let ids = hits.iter().map(|h| h.finding.id.clone()).collect();
                let d = st.controller.asked(&query, ids, now);
                st.store.persist(&d)?;
            }
            if json {
                return Ok(serde_json::to_string_pretty(&hits)? + "\n");
            }
            let cards = crate::commons::read_cards(&crate::paths::sessions_dir());
            let know = crate::findings::might_know(&cards, &me.id, &query);
            Ok(crate::findings::render_hits(&query, &hits, &know, now))
        }
        Request::Card {
            name,
            task,
            note,
            offers,
            join,
            leave,
            handoff,
            ..
        } => {
            if let Some(n) = name
                .as_deref()
                .filter(|n| !belay_core::commons::valid_name(n))
            {
                return Ok(format!("belay: refused. `{n}` is not a card name: use lowercase letters, digits, `-`, `_`, `.` (up to 40), and not `user` or `repo`."));
            }
            if let Some(t) = join
                .iter()
                .flatten()
                .find(|t| !belay_core::commons::valid_name(t))
            {
                return Ok(format!(
                    "belay: refused. `{t}` is not a topic name: use lowercase letters, digits, `-`, `_`, `.`."
                ));
            }
            if handoff.as_deref().is_some_and(|h| h.trim().is_empty()) {
                return Ok("belay: refused. A handoff needs text.".into());
            }
            let me = lock(state).session_id.clone();
            if let Some(n) = &name {
                if let Some(id) = crate::commons::name_taken(&crate::paths::sessions_dir(), &me, n)
                {
                    return Ok(format!(
                        "belay: refused. The name `{n}` is taken by live session {id}."
                    ));
                }
            }
            let mut st = lock(state);
            let topics = (join.is_some() || leave.is_some()).then(|| {
                let mut t = st.controller.card(now_ms()).said.topics;
                t.extend(join.unwrap_or_default());
                let leave = leave.unwrap_or_default();
                t.retain(|x| !leave.contains(x));
                t
            });
            let ev = belay_core::Event::CardSet {
                name,
                task,
                note,
                offers,
                topics,
                handoff,
            };
            let d = st.controller.handle(ev, now_ms());
            st.store.persist(&d)?;
            let gap = st.controller.config().idle_gap_minutes * 60_000;
            Ok(crate::commons::render_card(
                &st.controller.card(now_ms()),
                now_ms(),
                gap,
            ))
        }
        _ => unreachable!("only commons requests"),
    }
}

/// This session's card and project, under the lock for a moment.
pub(super) fn my_card_and_project(
    state: &Mutex<State>,
    now: u64,
) -> (belay_core::commons::Card, String) {
    let st = lock(state);
    let me = st.controller.card(now);
    let project = st
        .project
        .clone()
        .unwrap_or_else(|| crate::learnstore::project_of(Path::new(&me.top)));
    (me, project)
}

/// A commons message from this session (C2): resolve `to` against live cards, let the
/// controller decide and record it, then append it to each recipient's inbox, and
/// with `commons = on`, wake an idle fleet stream session (rate-limited by its hub).
pub(super) fn send_commons(
    state: &Mutex<State>,
    to: &str,
    kind: Option<String>,
    text: &str,
) -> Result<String> {
    let now = now_ms();
    let (me, on, gap) = {
        let st = lock(state);
        let cfg = st.controller.config();
        (
            st.controller.card(now),
            cfg.commons,
            cfg.idle_gap_minutes * 60_000,
        )
    };
    let root = crate::paths::sessions_dir();
    let cards = crate::commons::read_cards(&root);
    let to_user = to == "user";
    let targets: Vec<&belay_core::commons::Card> = if to_user {
        Vec::new()
    } else {
        crate::commons::resolve(&cards, &me.id, &me.repo, to)
    };
    let names: Vec<String> = if to_user {
        vec!["user".into()]
    } else {
        targets.iter().map(|c| c.name.clone()).collect()
    };
    let verdict = {
        let mut st = lock(state);
        let (d, verdict) = st.controller.send(to, names, kind, text, now);
        st.store.persist(&d)?;
        verdict
    };
    let (kind, text) = match verdict {
        Ok(v) => v,
        Err(why) => return Ok(format!("belay: refused. {}", capitalize(&why))),
    };
    let line = crate::commons::message_line(&me.name, &me.id, &me.repo, &kind, &text, now);
    if to_user {
        crate::commons::append_message(&crate::commons::user_inbox(), &line)?;
        let a = attention::Alert {
            kind: attention::Kind::Message,
            text: format!("{kind} from {}: {text}", me.name),
            action: None,
        };
        alert(&mut lock(state), vec![a]);
        return Ok("Sent to the user (`belay inbox`).".into());
    }
    // Best effort: one inbox that cannot be written does not undo the others.
    let mut failed = Vec::new();
    for c in &targets {
        if crate::commons::append_message(&crate::commons::inbox_path(&root.join(&c.id)), &line)
            .is_err()
        {
            failed.push(c.name.clone());
            continue;
        }
        // An idle fleet stream session reads it only when a turn starts: ask its hub.
        // Not under `look`: the message would only be recorded, the turn wasted.
        if let (true, Some(fleet), Some(slot)) = (on, &c.fleet, &c.slot) {
            if c.status_at(now, gap) == belay_core::commons::CardStatus::Waiting {
                let _ = crate::hub::wake(fleet, slot, &me.name);
            }
        }
    }
    let mut said = crate::commons::sent_to(to, &targets);
    if !failed.is_empty() {
        said.push_str(&format!(
            " Not delivered to {}: its inbox could not be written.",
            failed.join(", ")
        ));
    }
    Ok(said)
}

pub(super) fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}
