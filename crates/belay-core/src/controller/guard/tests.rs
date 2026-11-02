//! Guard path tests: the precision corpus (fixtures/guard/real), project paths, moves.

use serde_json::{json, Value};

use super::super::*;
use crate::capability::Capabilities;

macro_rules! corpus {
    ($($name:literal),* $(,)?) => {
        [$(($name, include_str!(concat!("../../../../../fixtures/guard/real/", $name, ".json")))),*]
    };
}

/// Every file in fixtures/guard/real. A new case goes in this list too.
const CORPUS: [(&str, &str); 27] = corpus![
    "assert_removed",
    "conditional_skip_except",
    "conditional_skip_if_missing",
    "conditional_skip_module_level",
    "expected_to_actual",
    "focus_added_js",
    "heredoc_rewrites_existing",
    "moved_python_tests",
    "moved_tests_one_command",
    "moved_tests_one_turn_add_first",
    "moved_tests_one_turn_remove_first",
    "new_file_heredoc",
    "new_file_python_write",
    "new_file_write",
    "no_focus_syntax_in_python",
    "or_true_on_test_command",
    "outside_project_draft",
    "relative_tolerance_vacuous",
    "skipif_decorator",
    "test_deleted_not_moved",
    "tolerance_1000x_no_comment",
    "tolerance_justified_line_above",
    "tolerance_justified_same_line",
    "tolerance_small_tweak",
    "unconditional_skip_existing",
    "unconditional_skip_rust",
    "unittest_skip_unless",
];

fn ctl() -> Controller {
    Controller::new(Config::default(), Capabilities::CLAUDE_CODE)
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

fn shell_done(id: &str) -> Event {
    Event::ToolCompleted {
        tool_use_id: id.into(),
        tool: "Bash".into(),
        kind: ToolKind::Shell,
        input: json!({ "command": "true" }),
        output: String::new(),
        failed: false,
    }
}

/// Run one case's steps in one turn. Returns the guard reasons that stand: guard
/// actions no `guard_withdrawn` judgment points back to.
fn run(case: &Value) -> Vec<String> {
    let mut c = ctl();
    let mut entries = Vec::new();
    let mut feed = |c: &mut Controller, ev: Event| {
        let t = entries.len() as u64;
        entries.extend(c.handle(ev, t).entries);
    };
    feed(
        &mut c,
        Event::Workspace {
            top: s(case, "top"),
            common: format!("{}/.git", s(case, "top")),
        },
    );
    feed(&mut c, Event::PromptSubmitted { chars: 10 });
    let steps = case["steps"].as_array().cloned().unwrap_or_default();
    let mut open_command: Option<String> = None;
    for (i, step) in steps.iter().enumerate() {
        let via = s(step, "via");
        // A shell command ends when the next step is not part of it.
        if let Some(id) = open_command.take() {
            if via != "shell" || s(step, "tool_use_id") != id {
                feed(&mut c, shell_done(&id));
            } else {
                open_command = Some(id);
            }
        }
        match via.as_str() {
            "shell" => {
                let id = s(step, "tool_use_id");
                feed(
                    &mut c,
                    Event::FileChanged {
                        tool_use_id: id.clone(),
                        path: s(step, "path"),
                        old_text: s(step, "old"),
                        new_text: s(step, "new"),
                    },
                );
                open_command = Some(id);
            }
            "edit" => feed(
                &mut c,
                Event::EditProposed {
                    tool_use_id: format!("e{i}"),
                    path: s(step, "path"),
                    old_text: s(step, "old"),
                    new_text: s(step, "new"),
                },
            ),
            "run" => feed(
                &mut c,
                Event::ToolCompleted {
                    tool_use_id: format!("r{i}"),
                    tool: "Bash".into(),
                    kind: ToolKind::Shell,
                    input: json!({ "command": s(step, "command") }),
                    output: s(step, "output"),
                    failed: step["failed"].as_bool().unwrap_or(false),
                },
            ),
            other => panic!("unknown step {other}"),
        }
    }
    if let Some(id) = open_command {
        feed(&mut c, shell_done(&id));
    }
    feed(&mut c, Event::TurnEnded);
    let withdrawn: Vec<u64> = entries
        .iter()
        .filter(
            |e| matches!(&e.body, Body::Judgment { question, .. } if question == "guard_withdrawn"),
        )
        .flat_map(|e| e.causes.clone())
        .collect();
    entries
        .iter()
        .filter(|e| !withdrawn.contains(&e.seq))
        .filter_map(|e| match &e.body {
            Body::Action {
                action: ActionKind::Guard { .. },
                reason,
                ..
            } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

/// P0.1: the guard's precision and recall over real patterns (synthetic, minimal).
#[test]
fn precision_corpus() {
    let (mut tp, mut fp, mut fn_, mut tn) = (0, 0, 0, 0);
    let mut wrong = Vec::new();
    for (name, text) in CORPUS {
        let case: Value = serde_json::from_str(text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let flags = run(&case);
        let want = s(&case, "expect") == "flag";
        let rule = s(&case, "rule");
        match (want, flags.is_empty()) {
            (true, false) if flags.iter().any(|r| r.contains(&rule)) => tp += 1,
            (true, false) => {
                fn_ += 1;
                wrong.push(format!("{name}: flagged without `{rule}`: {flags:?}"));
            }
            (true, true) => {
                fn_ += 1;
                wrong.push(format!("{name}: missed ({})", s(&case, "pattern")));
            }
            (false, false) => {
                fp += 1;
                wrong.push(format!("{name}: false alarm: {flags:?}"));
            }
            (false, true) => tn += 1,
        }
    }
    let ratio = |a: usize, b: usize| {
        if a + b == 0 {
            1.0
        } else {
            a as f64 / (a + b) as f64
        }
    };
    println!(
        "guard corpus: {} cases, precision {:.2}, recall {:.2} (tp {tp}, fp {fp}, fn {fn_}, tn {tn})",
        CORPUS.len(),
        ratio(tp, fp),
        ratio(tp, fn_)
    );
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

#[test]
fn removal_that_may_be_a_move_warns_then_is_withdrawn() {
    let old = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn a() {\n        assert_eq!(f(), 1);\n    }\n}\n";
    let mut c = ctl();
    c.handle(Event::PromptSubmitted { chars: 1 }, 0);
    let d = c.handle(
        Event::EditProposed {
            tool_use_id: "e1".into(),
            path: "/p/src/lib.rs".into(),
            old_text: old.into(),
            new_text: "#[cfg(test)]\nmod tests;\n".into(),
        },
        1,
    );
    // Not blocked: the tests may have moved.
    assert!(d.response.permission.is_none(), "{:?}", d.response);
    assert!(d.response.add_context[0].contains("tests removed"));
    let d = c.handle(
        Event::EditProposed {
            tool_use_id: "e2".into(),
            path: "/p/src/lib/tests.rs".into(),
            old_text: String::new(),
            new_text: "use super::*;\n\n#[test]\nfn a() {\n    assert_eq!(f(), 1);\n}\n".into(),
        },
        2,
    );
    assert!(d.entries.iter().any(|e| matches!(
        &e.body,
        Body::Judgment { question, subject, .. } if question == "guard_withdrawn" && subject == "/p/src/lib.rs"
    )));
    // A removal in the next turn is not proved a move by this one.
    c.handle(Event::TurnEnded, 3);
    c.handle(Event::PromptSubmitted { chars: 1 }, 4);
    let d = c.handle(
        Event::EditProposed {
            tool_use_id: "e3".into(),
            path: "/p/src/lib/tests.rs".into(),
            old_text: "#[test]\nfn a() {\n    assert_eq!(f(), 1);\n}\n".into(),
            new_text: String::new(),
        },
        5,
    );
    assert!(!d.response.add_context.is_empty());
}

#[test]
fn a_removed_assertion_in_a_kept_test_still_blocks() {
    let mut c = ctl();
    let d = c.handle(
        Event::EditProposed {
            tool_use_id: "e1".into(),
            path: "tests/test_a.py".into(),
            old_text: "def test_a():\n    assert f() == 1\n    assert g() == 2\n".into(),
            new_text: "def test_a():\n    assert f() == 1\n".into(),
        },
        0,
    );
    assert!(matches!(d.response.permission, Some(Permission::Deny(_))));
}

#[test]
fn files_outside_the_project_are_not_judged() {
    let mut c = ctl();
    c.handle(
        Event::Workspace {
            top: "/work/proj".into(),
            common: "/work/proj/.git".into(),
        },
        0,
    );
    let edit = |path: &str| Event::EditProposed {
        tool_use_id: "e".into(),
        path: path.into(),
        old_text: "assert a\nassert b\n".into(),
        new_text: "assert a\n".into(),
    };
    for path in ["/tmp/x/tests/test_a.py", "/work/project2/tests/test_a.py"] {
        let d = c.handle(edit(path), 1);
        assert!(
            !d.entries
                .iter()
                .any(|e| matches!(e.body, Body::Judgment { .. })),
            "{path}"
        );
    }
    // The test-path rules read the path inside the project: a `tests/` above the
    // top does not make a source file a test.
    let mut c = ctl();
    c.handle(
        Event::Workspace {
            top: "/home/u/tests/app".into(),
            common: "/home/u/tests/app/.git".into(),
        },
        0,
    );
    let d = c.handle(edit("/home/u/tests/app/src/main.py"), 1);
    assert!(d.response.permission.is_none());
    let d = c.handle(edit("/home/u/tests/app/tests/test_main.py"), 2);
    assert!(d.response.permission.is_some());
}
