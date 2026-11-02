use std::time::{Duration, Instant};

use belay_core::guard::GuardDecision;
use belay_core::{ActionKind, Body, Decision, Entry, Event, Notice, NoticeLevel, SteerTrigger};

use super::*;

fn decision(bodies: Vec<Body>) -> Vec<Decision> {
    vec![Decision {
        entries: bodies
            .into_iter()
            .enumerate()
            .map(|(i, body)| Entry {
                seq: i as u64,
                ts_ms: 0,
                causes: vec![],
                body,
            })
            .collect(),
        ..Default::default()
    }]
}

fn event(event: Event) -> Body {
    Body::Event { event }
}

fn action(id: &str, action: ActionKind) -> Body {
    Body::Action {
        id: id.into(),
        action,
        reason: "Why.".into(),
        applied: true,
        unsupported: false,
    }
}

fn block() -> Body {
    action(
        "a-1",
        ActionKind::Guard {
            path: "tests/test_a.py".into(),
            decision: GuardDecision::Deny,
        },
    )
}

fn steer() -> Body {
    action(
        "a-2",
        ActionKind::Steer {
            trigger: SteerTrigger::Cycle {
                path: "src/a.py".into(),
            },
            text: "belay: the same failure again.".into(),
        },
    )
}

fn waiting(reason: &str) -> Body {
    event(Event::AgentWaiting {
        reason: reason.into(),
        text: "Claude needs your permission to use Bash".into(),
    })
}

/// observe + admit, as the daemon runs them: the kinds sent at `now`.
fn sent(a: &mut Attention, bodies: Vec<Body>, unmet: bool, now: u64) -> Vec<Kind> {
    a.observe(&decision(bodies), unmet, now)
        .into_iter()
        .filter(|x| a.admit(x.kind, now, Some(12 * 60)))
        .map(|x| x.kind)
        .collect()
}

#[test]
fn each_kind_triggers_once_and_is_rate_limited() {
    let mut a = Attention::new(Notify::Attention, None);
    let ended = || event(Event::TurnEnded);
    assert_eq!(sent(&mut a, vec![block()], false, 0), [Kind::Blocked]);
    assert_eq!(sent(&mut a, vec![steer()], false, 0), [Kind::Loop]);
    assert_eq!(sent(&mut a, vec![ended()], true, 0), [Kind::VerifyUnmet]);
    assert_eq!(
        sent(&mut a, vec![waiting("permission_prompt")], false, 0),
        [Kind::Waiting]
    );
    // Within two minutes, each kind again is held back; other kinds still go.
    for t in [1_000, RATE_MS - 1] {
        assert!(sent(&mut a, vec![block()], false, t).is_empty());
        assert!(sent(&mut a, vec![steer()], false, t).is_empty());
        assert!(sent(&mut a, vec![ended()], true, t).is_empty());
        assert!(sent(&mut a, vec![waiting("elicitation_dialog")], false, t).is_empty());
    }
    assert_eq!(
        sent(&mut a, vec![block(), steer()], false, RATE_MS),
        [Kind::Blocked, Kind::Loop]
    );

    // "done": a turn that ended and stays idle for IDLE_MS, once.
    let mut a = Attention::new(Notify::Attention, None);
    assert!(sent(&mut a, vec![ended()], false, 0).is_empty());
    assert_eq!(a.idle_due(IDLE_MS - 1), None);
    let done = a.idle_due(IDLE_MS).expect("done");
    assert_eq!(done.kind, Kind::Done);
    assert!(a.admit(Kind::Done, IDLE_MS, None));
    assert_eq!(a.idle_due(IDLE_MS * 3), None, "once per turn");
    // The next turn ends soon after: rate-limited.
    sent(
        &mut a,
        vec![event(Event::PromptSubmitted { chars: 3 }), ended()],
        false,
        IDLE_MS + 10,
    );
    let again = a.idle_due(IDLE_MS * 2 + 10).expect("due");
    assert!(!a.admit(again.kind, IDLE_MS * 2 + 10, None));
}

#[test]
fn what_is_not_a_moment() {
    let mut a = Attention::new(Notify::Attention, None);
    // A prompt or a tool call after the turn's end: not idle.
    sent(&mut a, vec![event(Event::TurnEnded)], false, 0);
    sent(
        &mut a,
        vec![event(Event::PromptSubmitted { chars: 1 })],
        false,
        1,
    );
    assert_eq!(a.idle_due(IDLE_MS * 2), None);
    // belay kept the turn going: neither done nor unmet.
    let verify = action(
        "a-3",
        ActionKind::Verify {
            command: "pytest".into(),
            text: "Run it.".into(),
        },
    );
    assert!(sent(&mut a, vec![event(Event::TurnEnded), verify], true, 2).is_empty());
    assert_eq!(a.idle_due(IDLE_MS * 2), None);
    // Claude Code's own idle reminder and a login are not waits.
    assert!(sent(
        &mut a,
        vec![waiting("idle_prompt"), waiting("auth_success")],
        false,
        3
    )
    .is_empty());
    // An unapplied block, a guard warning: the agent was not stopped.
    let mut unapplied = block();
    if let Body::Action { applied, .. } = &mut unapplied {
        *applied = false;
    }
    let warn = action(
        "a-4",
        ActionKind::Guard {
            path: "tests/t.py".into(),
            decision: GuardDecision::Warn,
        },
    );
    let notice = Body::Notice {
        notice: Notice {
            level: NoticeLevel::Warn,
            text: "A warning.".into(),
            action: None,
        },
        shown: false,
    };
    assert!(sent(&mut a, vec![unapplied, warn, notice.clone()], false, 4).is_empty());
    // `all` hears warnings too; `off` hears nothing.
    let mut all = Attention::new(Notify::All, None);
    assert_eq!(sent(&mut all, vec![notice], false, 0), [Kind::Warning]);
    let mut off = Attention::new(Notify::Off, None);
    assert!(sent(&mut off, vec![block(), steer()], false, 0).is_empty());
    sent(&mut off, vec![event(Event::TurnEnded)], false, 0);
    assert_eq!(off.idle_due(IDLE_MS), None);
}

#[test]
fn quiet_window_holds_notifications_and_wraps_midnight() {
    let mut a = Attention::new(Notify::Attention, Some((22 * 60, 7 * 60)));
    for m in [22 * 60, 23 * 60 + 59, 0, 6 * 60 + 59] {
        assert!(!a.admit(Kind::Blocked, 0, Some(m)), "quiet at {m}");
    }
    // A held-back one does not count against the rate limit.
    assert!(a.admit(Kind::Blocked, 0, Some(7 * 60)));
    let mut day = Attention::new(Notify::Attention, Some((12 * 60, 13 * 60)));
    assert!(!day.admit(Kind::Loop, 0, Some(12 * 60 + 30)));
    assert!(day.admit(Kind::Loop, 0, Some(13 * 60)));
    // No local time known: the window cannot apply.
    assert!(a.admit(Kind::Loop, 0, None));
}

#[test]
fn notify_command_reads_the_event_as_json() {
    let dir = std::env::temp_dir().join(format!("belay-attn-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("got.json");
    let c = Channels {
        session: "s-1".into(),
        repo: "/r".into(),
        ..Default::default()
    };
    let alert = Alert::new(Kind::Blocked, "Blocked an edit.", Some("a-1".into()));
    let cmd = format!("cat > {}", crate::run::sh_quote(&out.to_string_lossy()));
    assert!(run_command(&cmd, &payload(&c, &alert, 7), COMMAND_TIMEOUT));
    let got: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(
        got,
        serde_json::json!({
            "session": "s-1", "repo": "/r", "kind": "blocked",
            "text": "Blocked an edit.", "action": "a-1", "ts_ms": 7,
        })
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_hung_command_is_killed_and_delivery_does_not_wait() {
    assert_eq!(COMMAND_TIMEOUT, Duration::from_secs(5));
    let c = Channels {
        command: Some("sleep 30".into()),
        ..Default::default()
    };
    let alert = Alert::new(Kind::Done, "Done.", None);
    // The hook's side: deliver returns at once.
    let t = Instant::now();
    deliver(c.clone(), vec![alert.clone()], 0);
    assert!(
        t.elapsed() < Duration::from_millis(100),
        "{:?}",
        t.elapsed()
    );
    // The thread's side: killed at the timeout (short here), with what it started.
    let t = Instant::now();
    assert!(!run_command(
        "sleep 30; echo late",
        &payload(&c, &alert, 0),
        Duration::from_millis(300)
    ));
    let took = t.elapsed();
    assert!(
        took >= Duration::from_millis(300) && took < Duration::from_secs(3),
        "{took:?}"
    );
}
