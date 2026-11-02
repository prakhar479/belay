use super::*;
use belay_core::brief::{Brief, Check, CheckStatus, LookAt};

const STREAM: &str = include_str!("../../../../fixtures/claude_code/headless_stream.jsonl");
const LIMIT: &str = include_str!("../../../../fixtures/fleet/result_limit_resets_11pm.json");
/// 2026-11-02 01:00 UTC.
const NOW: u64 = 1_793_581_200_000;

fn meta(budget: Budget, until_ms: Option<u64>) -> Meta {
    Meta {
        id: "bg-1".into(),
        prompt: "fix it".into(),
        cwd: "/r".into(),
        place: Place::Here,
        resume: None,
        model: None,
        budget,
        until_ms,
        keep_awake: false,
        by: "user".into(),
        created_ms: NOW,
    }
}

fn claude() -> Box<dyn Connector> {
    connector::for_agent(connector::DEFAULT_AGENT).unwrap()
}

#[test]
fn budgets_are_tokens_or_dollars() {
    assert_eq!(parse_budget("2M").unwrap().tokens, Some(2_000_000));
    assert_eq!(parse_budget("500k").unwrap().tokens, Some(500_000));
    assert_eq!(parse_budget("$5").unwrap().usd, Some(5.0));
    assert_eq!(parse_budget("2.5$").unwrap().usd, Some(2.5));
    for bad in ["", "0", "$0", "$-1", "lots", "$x"] {
        assert!(parse_budget(bad).is_err(), "{bad}");
    }
}

#[test]
fn until_is_the_next_local_time_or_a_duration() {
    let start = NOW / MINUTE_MS * MINUTE_MS;
    // 17:00 local now: 18:30 is in 90 minutes; 08:00 tomorrow, in 15 hours.
    assert_eq!(
        parse_until("18:30", NOW, Some(17 * 60)).unwrap(),
        start + 90 * MINUTE_MS
    );
    assert_eq!(
        parse_until("08:00", NOW, Some(17 * 60)).unwrap(),
        start + 15 * 60 * MINUTE_MS
    );
    // The minute it is now: a day ahead, not now.
    assert_eq!(
        parse_until("17:00", NOW, Some(17 * 60)).unwrap(),
        start + DAY_MINUTES * MINUTE_MS
    );
    assert_eq!(parse_until("45m", NOW, None).unwrap(), NOW + 45 * MINUTE_MS);
    assert_eq!(
        parse_until("1h30m", NOW, None).unwrap(),
        NOW + 90 * MINUTE_MS
    );
    for bad in ["25:00", "12:60", "soon", "18:30x"] {
        assert!(parse_until(bad, NOW, Some(0)).is_err(), "{bad}");
    }
    assert!(parse_until("18:30", NOW, None).is_err(), "no local time");
}

#[test]
fn a_run_is_read_for_tokens_the_conversation_and_its_end() {
    let c = claude();
    let m = meta(Budget::default(), None);
    let mut t = Tracker::new(&*c, &m, &State::default());
    for l in STREAM.lines() {
        assert_eq!(t.line(l, NOW), None, "{l}");
    }
    // msg_01A once (2 + 4 + 8600), msg_01B (40 + 12 + 100).
    assert_eq!(t.tokens, 8606 + 152);
    assert_eq!(
        t.conversation.as_deref(),
        Some("fbdeec93-30e3-4e40-b8d6-5c081a2148ee")
    );
    assert_eq!(t.result.as_deref(), Some("Done: a.txt says b."));
    assert_eq!(t.cost_usd, Some(0.0018));
    assert_eq!(t.limit, None);
    assert_eq!(t.finish(NOW), None);
}

#[test]
fn a_budget_stops_at_the_next_tool_result_not_mid_step() {
    let c = claude();
    let m = meta(
        Budget {
            tokens: Some(1000),
            usd: None,
        },
        None,
    );
    // Earlier runs count: 100 tokens before this one.
    let st = State {
        tokens: 100,
        ..State::default()
    };
    let mut t = Tracker::new(&*c, &m, &st);
    let lines: Vec<&str> = STREAM.lines().collect();
    // init, then msg_01A's text and its tool call: over budget, but the tool runs.
    for l in &lines[..3] {
        assert_eq!(t.line(l, NOW), None);
    }
    let stop = t.line(lines[3], NOW).expect("stops at the tool result");
    assert_eq!(
        stop,
        Stop::Budget("it used 8706 tokens, past the budget of 1K".into())
    );
    assert_eq!(t.total_tokens(), 8706);
    assert_eq!(t.finish(NOW), Some(stop));
}

#[test]
fn a_dollar_budget_and_a_deadline_stop_too() {
    let c = claude();
    let m = meta(
        Budget {
            tokens: None,
            usd: Some(0.001),
        },
        None,
    );
    let mut t = Tracker::new(&*c, &m, &State::default());
    for l in STREAM.lines() {
        t.line(l, NOW);
    }
    // The cost comes with the end: the run is over, and it says why it stops.
    assert!(matches!(t.finish(NOW), Some(Stop::Budget(r)) if r.contains("$0.00")));

    let m = meta(Budget::default(), Some(NOW - 1));
    let mut t = Tracker::new(&*c, &m, &State::default());
    let lines: Vec<&str> = STREAM.lines().collect();
    assert_eq!(t.line(lines[0], NOW), None);
    assert!(matches!(t.line(lines[3], NOW), Some(Stop::Deadline(_))));
    // A deadline that passed with no output at all.
    let mut t = Tracker::new(&*c, &m, &State::default());
    assert!(matches!(t.finish(NOW), Some(Stop::Deadline(_))));
}

#[test]
fn a_user_stop_waits_for_the_step_and_the_first_reason_wins() {
    let c = claude();
    let m = meta(Budget::default(), None);
    let mut t = Tracker::new(&*c, &m, &State::default());
    t.ask(Stop::User("the user attached".into()));
    t.ask(Stop::Budget("later".into()));
    let lines: Vec<&str> = STREAM.lines().collect();
    assert_eq!(t.line(lines[2], NOW), None);
    assert_eq!(
        t.line(lines[3], NOW),
        Some(Stop::User("the user attached".into()))
    );
}

#[test]
fn a_usage_limit_ends_the_run_with_its_reset_time() {
    let c = claude();
    let m = meta(Budget::default(), None);
    let mut t = Tracker::new(&*c, &m, &State::default());
    t.line(STREAM.lines().next().unwrap(), NOW);
    t.line(LIMIT.trim(), NOW);
    let midnight = NOW / 86_400_000 * 86_400_000;
    assert_eq!(t.limit, Some(Some(midnight + 23 * 60 * MINUTE_MS)));
    // A limit is not a stop: the supervisor waits and goes on.
    t.ask(Stop::User("x".into()));
    assert_eq!(t.finish(NOW), None);
}

#[test]
fn the_checkout_gets_auto_permissions_and_the_sandbox_bypass() {
    let c = claude();
    let here = argv(&*c, &Place::Here, "go", Some("conv-1"), Some("m")).unwrap();
    let mode =
        |a: &[String]| a[a.iter().position(|x| x == "--permission-mode").unwrap() + 1].clone();
    assert_eq!(mode(&here), "auto");
    assert!(here
        .windows(3)
        .any(|w| w == ["--resume", "conv-1", "--fork-session"]));
    assert_eq!(here.last().unwrap(), "go");
    let sb = argv(&*c, &Place::Sandbox { id: "s".into() }, "go", None, None).unwrap();
    assert_eq!(mode(&sb), "bypassPermissions");
}

fn look(kind: &str, text: &str) -> LookAt {
    LookAt {
        kind: kind.into(),
        text: text.into(),
        evidence: "a-3".into(),
    }
}

fn check(status: CheckStatus) -> Check {
    Check {
        command: "pytest".into(),
        status,
        runs: 1,
        output: "o-1".into(),
    }
}

#[test]
fn needs_you_is_an_unallowed_block_then_a_check_not_passing() {
    let mut b = Brief::default();
    assert_eq!(needs_you(&b), None);
    b.checked = vec![check(CheckStatus::PassedAfterLastEdit)];
    b.look_at = vec![look(
        "guard",
        "the test guard blocked an edit to t.py; the user allowed it",
    )];
    assert_eq!(needs_you(&b), None);
    b.checked = vec![check(CheckStatus::NotRunSinceLastEdit)];
    assert_eq!(
        needs_you(&b).as_deref(),
        Some("`pytest` not run since the last edit")
    );
    b.look_at.push(look(
        "guard",
        "the test guard blocked an edit to t.py; not allowed",
    ));
    assert_eq!(
        needs_you(&b).as_deref(),
        Some("the test guard blocked an edit to t.py; not allowed [a-3]")
    );
}

#[test]
fn phases_read_as_the_user_sees_them() {
    assert_eq!(phase_line(&Phase::Working), "working");
    assert_eq!(phase_line(&Phase::Done), "done");
    assert_eq!(
        phase_line(&Phase::NeedsYou {
            reason: "`pytest` failing".into()
        }),
        "needs you: `pytest` failing"
    );
    let w = phase_line(&Phase::Waiting { until_ms: NOW });
    assert!(
        w.starts_with("waiting for the usage limit to reset, until "),
        "{w}"
    );
    // The state file keeps the phase flat, next to the counts.
    let st = State {
        phase: Phase::Waiting { until_ms: 5 },
        tokens: 7,
        ..State::default()
    };
    let v: serde_json::Value = serde_json::to_value(&st).unwrap();
    assert_eq!(v["phase"], "waiting");
    assert_eq!(v["until_ms"], 5);
    let back: State = serde_json::from_value(v).unwrap();
    assert_eq!(back.phase, Phase::Waiting { until_ms: 5 });
}

#[test]
fn the_lid_line_says_held_or_how_to_change_it() {
    let held = keepawake::assess_wsl(true, Some((0, 0)));
    assert!(
        lid_line(&held).starts_with("lid closed: held"),
        "{}",
        lid_line(&held)
    );
    let not = keepawake::assess_wsl(true, Some((1, 1)));
    let l = lid_line(&not);
    assert!(l.starts_with("lid closed: not held."), "{l}");
    assert!(l.contains("To change: ") && l.contains("LIDACTION"), "{l}");
}
