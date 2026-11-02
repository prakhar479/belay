use super::*;
use belay_core::perception::OutputKind;
use serde_json::json;
use std::io::Write as _;

fn src() -> Box<dyn TranscriptSource> {
    crate::connector::for_agent("claude-code")
        .unwrap()
        .transcript()
        .unwrap()
}

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/claude_code/usage_limit_transcript.jsonl"
);

/// A fresh transcript root: `<root>/<project>/<session>.jsonl` files.
fn root(name: &str) -> PathBuf {
    let r = std::env::temp_dir().join(format!("belay-usage-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&r);
    fs::create_dir_all(r.join("-home-dev-shop")).unwrap();
    r
}

fn put(root: &Path, session: &str, text: &str) -> PathBuf {
    let p = root.join("-home-dev-shop").join(format!("{session}.jsonl"));
    fs::write(&p, text).unwrap();
    p
}

fn ms(s: &str) -> u64 {
    // The fixture's dates are all on 2026-10-20 (UTC).
    let (h, m) = s.split_once(':').unwrap();
    1_792_454_400_000 + (h.parse::<u64>().unwrap() * 60 + m.parse::<u64>().unwrap()) * 60_000
}

fn assistant(at_ms: u64, id: &str, model: &str, input: u64, output: u64) -> String {
    let secs = at_ms / 1000;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    let days = secs / 86_400 - 20_746; // 2026-10-20
    json!({"type": "assistant", "timestamp": format!("2026-10-{:02}T{h:02}:{m:02}:{s:02}.000Z", 20 + days),
        "message": {"id": id, "model": model, "content": [],
            "usage": {"input_tokens": input, "output_tokens": output}}})
    .to_string()
        + "\n"
}

fn limit_line(at_ms: u64) -> String {
    let secs = at_ms / 1000;
    let (h, m) = ((secs / 3600) % 24, (secs / 60) % 60);
    let days = secs / 86_400 - 20_746;
    json!({"type": "assistant", "isApiErrorMessage": true,
        "timestamp": format!("2026-10-{:02}T{h:02}:{m:02}:00.000Z", 20 + days),
        "message": {"id": format!("lim-{at_ms}"), "model": "<synthetic>",
            "content": [{"type": "text", "text": "You've hit your 5-hour limit · resets 3pm"}],
            "usage": {"input_tokens": 0, "output_tokens": 0}}})
    .to_string()
        + "\n"
}

#[test]
fn ms_helper_matches_the_transcript_parser() {
    assert_eq!(ms("00:00"), 1_792_454_400_000);
    let log = src().usage_log(Path::new(FIXTURE), 0);
    assert_eq!(log.messages[0].at_ms, ms("10:00") + 5000);
}

/// One message over two lines counts once with its last usage; a `<synthetic>` line
/// is a limit hit and no message; a quoted limit and a weekly limit are not hits.
#[test]
fn transcript_usage_and_limit_hits() {
    let log = src().usage_log(Path::new(FIXTURE), 0);
    let ids: Vec<&str> = log.messages.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["msg_u1", "msg_u2"]);
    assert_eq!(log.messages[0].usage.cache_read, 10_000);
    assert_eq!(log.limit_hits, [ms("10:05")]);
    assert_eq!(log.cwd.as_deref(), Some("/home/dev/shop"));
    assert_eq!(log.end, fs::metadata(FIXTURE).unwrap().len());
}

/// The headless stream's result line (fixtures/fleet) is a hit too.
#[test]
fn result_line_is_a_hit() {
    let r = root("result");
    let text = fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/fleet/result_five_hour_limit.json"
    ))
    .unwrap();
    let mut v: Value = serde_json::from_str(&text).unwrap();
    v["timestamp"] = json!("2026-10-20T10:00:00.000Z");
    let p = put(&r, "s", &(v.to_string() + "\n"));
    assert_eq!(src().usage_log(&p, 0).limit_hits, [ms("10:00")]);
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn totals_on_the_fixture() {
    let r = root("totals");
    let text = fs::read_to_string(FIXTURE).unwrap();
    put(&r, "s-usage", &text);
    // A resumed conversation's file repeats a message: it counts once.
    put(&r, "s-resumed", text.lines().nth(2).unwrap());
    let prices = Prices::new("");
    let (t, by) = totals(src().as_ref(), &r, 0, By::Project, &prices);
    assert_eq!(
        (t.input, t.output, t.cache_read, t.cache_write, t.messages),
        (1010, 1100, 10_000, 2000, 2)
    );
    assert_eq!(t.tokens(), 14_110);
    // Sonnet 5.5: 10×2 + 100×10 + 10k×0.2 + 2k×2.5; Haiku 5.5: 1k×0.1 + 1k×0.5.
    assert!((t.cost_est - 0.00862).abs() < 1e-9, "{}", t.cost_est);
    assert_eq!(by.keys().collect::<Vec<_>>(), ["/home/dev/shop"]);
    let (_, by) = totals(src().as_ref(), &r, 0, By::Session, &prices);
    assert_eq!(by["s-usage"].messages, 2);
    // Nothing after the period starts.
    let (t, _) = totals(src().as_ref(), &r, ms("11:00"), By::Day, &prices);
    assert_eq!(t.messages, 0);
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn prices_match_the_longest_key_and_take_overrides() {
    let p = Prices::new("");
    assert_eq!(p.for_model("claude-opus-4-1-20250805").input, 15.0);
    assert_eq!(p.for_model("claude-opus-4-5").input, 5.0);
    assert_eq!(p.for_model("us.anthropic.claude-opus-5-5").input, 4.0);
    assert_eq!(p.for_model("some-new-model").key, "sonnet");
    let p = Prices::new("opus 1/2, my-model 3/4/0.5/6");
    assert_eq!(p.for_model("claude-opus-9").output, 2.0);
    assert_eq!(p.for_model("claude-opus-9").cache_read, 0.1);
    let m = p.for_model("my-model");
    assert_eq!((m.cache_read, m.cache_write), (0.5, 6.0));
    for bad in ["opus", "opus 1", "opus 1/2/3/4/5", "opus a/b", "opus -1/2"] {
        assert!(parse_prices(bad).is_err(), "{bad}");
    }
    assert!(parse_prices("").unwrap().is_empty());
}

fn entry(seq: u64, ts_ms: u64, causes: Vec<u64>, body: Body) -> Entry {
    Entry {
        seq,
        ts_ms,
        causes,
        body,
    }
}

fn completed(kind: ToolKind, input: Value, failed: bool) -> Body {
    Body::Event {
        event: Event::ToolCompleted {
            tool_use_id: "t".into(),
            tool: if kind == ToolKind::Read {
                "Read"
            } else {
                "Bash"
            }
            .into(),
            kind,
            input,
            output: String::new(),
            failed,
        },
    }
}

fn requested() -> Body {
    Body::Event {
        event: Event::ToolRequested {
            tool_use_id: "t".into(),
            tool: "Bash".into(),
            kind: ToolKind::Shell,
            input: json!({}),
            rewrite_ok: false,
        },
    }
}

fn output(blob: &str) -> Body {
    Body::Output {
        id: "o-1".into(),
        blob: blob.into(),
        output_kind: OutputKind::Other,
        lines: 1,
        tokens: 10,
    }
}

fn msg(at_ms: u64, n: u64) -> MessageUsage {
    MessageUsage {
        at_ms,
        id: format!("m{at_ms}"),
        model: "claude-sonnet-5-5".into(),
        usage: Usage {
            input: n,
            messages: 1,
            ..Usage::default()
        },
    }
}

/// A scripted ledger: a failing run at 1:00, the next call at 2:00; a loop from 3:00
/// to its steer at 5:00 (with a failing run inside); a read twice of the same content,
/// the second at 6:00, then a call at 7:00; a summary that kept 900 tokens out.
#[test]
fn episodes_join_messages_by_time() {
    let min = 60_000;
    let loop_steer = Body::Action {
        id: "a-1".into(),
        action: ActionKind::Steer {
            trigger: SteerTrigger::Loop {
                signatures: vec!["sig".into()],
                duration_ms: Some(2 * min),
            },
            text: "stop".into(),
        },
        reason: "loop".into(),
        applied: true,
        unsupported: false,
    };
    let shape = Body::Action {
        id: "a-2".into(),
        action: ActionKind::Shape {
            from_tokens: 1000,
            to_tokens: 100,
            output_id: "o-1".into(),
            from_lines: 100,
            to_lines: 10,
        },
        reason: "summary".into(),
        applied: true,
        unsupported: false,
    };
    let read = json!({"file_path": "/x.py"});
    let entries = vec![
        entry(1, min, vec![], completed(ToolKind::Shell, json!({}), true)),
        entry(2, 2 * min, vec![], requested()),
        entry(
            3,
            4 * min,
            vec![],
            completed(ToolKind::Shell, json!({}), true),
        ),
        entry(4, 5 * min, vec![], loop_steer),
        entry(
            5,
            5 * min + 1,
            vec![],
            completed(ToolKind::Read, read.clone(), false),
        ),
        entry(6, 5 * min + 2, vec![5], output("h1")),
        entry(7, 6 * min, vec![], completed(ToolKind::Read, read, false)),
        entry(8, 6 * min + 1, vec![7], output("h1")),
        entry(9, 7 * min, vec![], requested()),
        entry(10, 8 * min, vec![], shape),
    ];
    let msgs = vec![
        msg(30_000, 1),              // before anything
        msg(min + 30_000, 10),       // after the failing run
        msg(2 * min + 30_000, 2),    // after the next call: not the run's
        msg(3 * min + 30_000, 100),  // in the loop
        msg(4 * min + 30_000, 100),  // in the loop, after its failing run: the loop's
        msg(6 * min + 30_000, 1000), // after the repeated read
        msg(7 * min + 30_000, 5),
    ];
    let w = attribute(&entries, &msgs);
    assert_eq!(
        w,
        Where {
            sessions: 1,
            total: 1218,
            loops: 200,
            failing_runs: 10,
            repeated_reads: 1000,
            kept_out: 900,
        }
    );
    assert_eq!(
        where_line(&w),
        "of 1.2k tokens: 200 in loops, 10 after failing runs, 1.0k after repeated reads; 900 kept out by summaries (est.)"
    );
}

/// The estimate needs 2 hits; with 3 it is the median of the windows before them.
#[test]
fn limit_estimate_with_0_1_3_hits() {
    let r = root("limit");
    let prices = Prices::new("");
    let h = 3600 * 1000;
    // Haiku 5.5 at 1M output tokens: $0.50 a message.
    let half = |at: u64, id: &str| assistant(at, id, "claude-haiku-5-5", 0, 1_000_000);
    let day = 24 * h;
    let t0 = ms("00:00");
    // Day d: 2(d+1) messages from 1:00, then a hit at 2:00. Time moves on as it would.
    let msgs = |d: u64| -> String {
        (0..2 * (d + 1))
            .map(|i| half(t0 + d * day + h + i * 60_000, &format!("m{d}-{i}")))
            .collect()
    };
    let p = put(&r, "s", &msgs(0));
    let now = t0 + h + 30 * 60_000;
    let c = refresh(Cache::default(), src().as_ref(), &r, &prices, "k", now);
    assert!(c.hits.is_empty());
    assert_eq!(budget_of(&c, now).usual_est, None);
    assert_eq!(status_segment(&budget_of(&c, now)), "");

    // One hit: still no estimate. A second line of the same limit (another session,
    // a retry) within the window is the same hit.
    let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
    f.write_all(limit_line(t0 + 2 * h).as_bytes()).unwrap();
    f.write_all(limit_line(t0 + 2 * h + 60_000).as_bytes())
        .unwrap();
    let now = t0 + 2 * h + 2 * 60_000;
    let c = refresh(c, src().as_ref(), &r, &prices, "k", now);
    assert_eq!(c.hits.len(), 1);
    assert!((c.hits[0].cost_est - 1.0).abs() < 1e-9);
    assert_eq!(budget_of(&c, now).usual_est, None);

    // Two more days, read incrementally: windows of $2 and $3, median $2.
    for d in [1, 2] {
        f.write_all(msgs(d).as_bytes()).unwrap();
        f.write_all(limit_line(t0 + d * day + 2 * h).as_bytes())
            .unwrap();
    }
    let now = t0 + 3 * day;
    let c = refresh(c, src().as_ref(), &r, &prices, "k", now);
    let costs: Vec<f64> = c.hits.iter().map(|h| h.cost_est).collect();
    assert_eq!(costs, [1.0, 2.0, 3.0]);
    assert_eq!(estimate(&c.hits), Some(2.0));
    // A cold scan agrees.
    let cold = refresh(Cache::default(), src().as_ref(), &r, &prices, "k", now);
    assert_eq!(cold.hits, c.hits);

    // Now $1 in the last 5 hours: half the usual budget.
    f.write_all(half(now - h, "late-1").as_bytes()).unwrap();
    f.write_all(half(now - h + 1, "late-2").as_bytes()).unwrap();
    let c = refresh(c, src().as_ref(), &r, &prices, "k", now);
    let b = budget_of(&c, now);
    assert_eq!(b.percent(), Some(50));
    assert_eq!(status_segment(&b), " · ~50% of usual 5-hour budget");
    let _ = fs::remove_dir_all(&r);
}

/// A warm cache reads only what a transcript gained; a half-written line waits.
#[test]
fn refresh_reads_only_new_lines() {
    let r = root("incr");
    let prices = Prices::new("");
    let now = ms("12:00");
    let p = put(
        &r,
        "s",
        &assistant(ms("11:00"), "a", "claude-haiku-5-5", 0, 1_000_000),
    );
    let c = refresh(Cache::default(), src().as_ref(), &r, &prices, "k", now);
    assert_eq!(c.recent.len(), 1);
    let line = assistant(ms("11:30"), "b", "claude-haiku-5-5", 0, 1_000_000);
    let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
    f.write_all(&line.as_bytes()[..20]).unwrap();
    let c = refresh(c, src().as_ref(), &r, &prices, "k", now);
    assert_eq!(c.recent.len(), 1);
    f.write_all(&line.as_bytes()[20..]).unwrap();
    let c = refresh(c, src().as_ref(), &r, &prices, "k", now);
    assert_eq!(c.recent.len(), 2);
    assert!((window_cost(&c.recent, now) - 1.0).abs() < 1e-9);
    // Other prices: a new scan, at the new prices.
    let c = refresh(c, src().as_ref(), &r, &Prices::new("haiku 1/1"), "k2", now);
    assert!((window_cost(&c.recent, now) - 2.0).abs() < 1e-9);
    let _ = fs::remove_dir_all(&r);
}

/// The status line runs `refresh` on every redraw: with a warm cache over many
/// transcripts it stays well under 50 ms (in a debug build).
#[test]
fn warm_cache_status_is_fast() {
    let r = root("speed");
    let prices = Prices::new("");
    let now = ms("12:00");
    for s in 0..300 {
        let mut text = String::new();
        for i in 0..50 {
            text += &assistant(
                ms("06:00") + i * 60_000,
                &format!("{s}-{i}"),
                "opus",
                10,
                10,
            );
        }
        if s % 100 == 0 {
            text += &limit_line(ms("07:00") + s);
        }
        put(&r, &format!("s{s}"), &text);
    }
    let c = refresh(Cache::default(), src().as_ref(), &r, &prices, "k", now);
    let bytes = serde_json::to_vec(&c).unwrap();
    let start = std::time::Instant::now();
    let warm: Cache = serde_json::from_slice(&bytes).unwrap();
    let warm = refresh(warm, src().as_ref(), &r, &prices, "k", now);
    let _ = status_segment(&budget_of(&warm, now));
    let took = start.elapsed();
    assert_eq!(warm, c);
    assert!(took.as_millis() < 50, "warm refresh took {took:?}");
    let _ = fs::remove_dir_all(&r);
}

#[test]
fn dates_parse_to_local_midnight() {
    let d = parse_date("2026-11-01").unwrap();
    assert_eq!(local_day(d), "2026-11-01");
    assert_eq!(local_midnight(d + 5 * 3600 * 1000), d);
    for bad in ["2026-13-01", "yesterday", "2026-11", "2026-11-01-02"] {
        assert!(parse_date(bad).is_err(), "{bad}");
    }
}

#[test]
fn render_says_est_and_no_estimate_yet() {
    let t = Tally {
        input: 1000,
        output: 10,
        messages: 1,
        cost_est: 1.5,
        ..Tally::default()
    };
    let mut groups = BTreeMap::new();
    groups.insert("2026-11-01".to_string(), t.clone());
    let b = Budget {
        hits: 1,
        usual_est: None,
        last_5h_est: 0.25,
    };
    let out = render(0, By::Day, &t, &groups, &Where::default(), &b);
    assert!(
        out.contains("Cost: ~$1.50 (est., prices of 2026-10-06"),
        "{out}"
    );
    assert!(out.contains("no estimate yet"), "{out}");
    assert!(out.contains("By day:\n"), "{out}");
}
