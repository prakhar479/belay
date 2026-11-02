use std::collections::BTreeMap;

use serde_json::json;

use super::*;
use crate::{Capabilities, Config, Controller, ToolKind};

/// A session scripted through the real controller: its ledger and blobs.
struct Script {
    c: Controller,
    entries: Vec<Entry>,
    blobs: BTreeMap<String, String>,
    now: u64,
}

impl Script {
    fn new() -> Self {
        Script {
            c: Controller::new(Config::default(), Capabilities::CLAUDE_CODE),
            entries: Vec::new(),
            blobs: BTreeMap::new(),
            now: 1_000,
        }
    }

    fn keep(&mut self, d: crate::Decision) {
        self.entries.extend(d.entries);
        for b in d.blobs {
            self.blobs.insert(b.hash, b.content);
        }
    }

    fn event(&mut self, ev: Event) {
        self.now += 60_000;
        let d = self.c.handle(ev, self.now);
        self.keep(d);
    }

    fn run(&mut self, cmd: &str, output: &str, failed: bool) {
        let id = format!("r{}", self.now);
        self.event(Event::ToolCompleted {
            tool_use_id: id,
            tool: "Bash".into(),
            kind: ToolKind::Shell,
            input: json!({ "command": cmd }),
            output: output.into(),
            failed,
        });
    }

    fn pass(&mut self, cmd: &str) {
        self.run(cmd, "===== 3 passed in 0.10s =====", false);
    }

    fn fail(&mut self, cmd: &str) {
        let out = "FAILED tests/test_a.py::test_x - assert 1 == 2\n===== 1 failed in 0.10s =====";
        self.run(cmd, out, true);
    }

    fn edit(&mut self, path: &str) {
        let id = format!("e{}", self.now);
        self.event(Event::FileEdited {
            tool_use_id: id,
            path: path.into(),
            after_hash: Some(format!("h{}", self.now)),
        });
    }

    fn shell_write(&mut self, path: &str, old: &str, new: &str) {
        let id = format!("w{}", self.now);
        self.event(Event::FileChanged {
            tool_use_id: id,
            path: path.into(),
            old_text: old.into(),
            new_text: new.into(),
        });
    }

    fn brief(&self, git: Option<Vec<GitChange>>) -> Brief {
        let blobs = |h: &str| self.blobs.get(h).cloned();
        build(&Input {
            session: "s1",
            entries: &self.entries,
            blobs: &blobs,
            root: Some("/repo"),
            git,
        })
    }
}

fn statuses(b: &Brief) -> Vec<(&str, CheckStatus)> {
    b.checked
        .iter()
        .map(|c| (c.command.as_str(), c.status))
        .collect()
}

fn kinds(b: &Brief) -> Vec<&str> {
    b.look_at.iter().map(|l| l.kind.as_str()).collect()
}

#[test]
fn checks_say_whether_they_ran_after_the_last_edit() {
    use CheckStatus::*;
    // (script, the status of `pytest`)
    type Step = fn(&mut Script);
    let table: [(&str, Vec<Step>, CheckStatus); 5] = [
        (
            "edit then pass",
            vec![|s| s.edit("/repo/src/app.py"), |s| s.pass("pytest")],
            PassedAfterLastEdit,
        ),
        (
            "pass then edit",
            vec![|s| s.edit("/repo/src/app.py"), |s| s.pass("pytest"), |s| {
                s.edit("/repo/src/app.py")
            }],
            NotRunSinceLastEdit,
        ),
        (
            "a docs edit after the pass",
            vec![|s| s.pass("pytest"), |s| s.edit("/repo/README.md")],
            PassedAfterLastEdit,
        ),
        (
            "failing after a pass",
            vec![|s| s.pass("pytest"), |s| s.edit("/repo/src/app.py"), |s| {
                s.fail("pytest")
            }],
            Failing,
        ),
        (
            "passing after a failure",
            vec![|s| s.fail("pytest"), |s| s.edit("/repo/src/app.py"), |s| {
                s.pass("pytest")
            }],
            PassedAfterLastEdit,
        ),
    ];
    for (name, steps, want) in table {
        let mut s = Script::new();
        for step in steps {
            step(&mut s);
        }
        let b = s.brief(None);
        assert_eq!(statuses(&b), vec![("pytest", want)], "{name}");
        assert!(b.checked[0].output.starts_with("o-"), "{name}");
    }
}

#[test]
fn each_command_is_judged_on_its_own() {
    let mut s = Script::new();
    s.pass("cargo test");
    s.edit("/repo/src/lib.rs");
    s.pass("pytest");
    let b = s.brief(None);
    assert_eq!(
        statuses(&b),
        vec![
            ("cargo test", CheckStatus::NotRunSinceLastEdit),
            ("pytest", CheckStatus::PassedAfterLastEdit)
        ]
    );
    assert_eq!(
        b.headline()[1],
        "Checked: ⚠ cargo test (not run since the last edit) · ✓ pytest"
    );
    // More than three: counted by state, worst first.
    s.fail("npm test");
    s.pass("go test ./...");
    assert_eq!(
        s.brief(None).headline()[1],
        "Checked: 4 commands: ✗ 1 failing · ⚠ 1 not run since the last edit · ✓ 2 passed after the last edit"
    );
}

#[test]
fn files_changed_after_the_last_pass_are_named_with_their_entry() {
    let mut s = Script::new();
    s.pass("pytest");
    s.edit("/repo/src/app.py");
    let b = s.brief(None);
    let last = b.look_at.last().expect("an item");
    assert_eq!(last.kind, "changed_after_pass");
    assert_eq!(last.text, "src/app.py changed after the last passing run");
    assert_eq!(b.changed[0].path, "src/app.py");
    assert_eq!(last.evidence, b.changed[0].last_edit.unwrap().to_string());
    // No run at all: that is flagged instead.
    let mut s = Script::new();
    s.edit("/repo/src/app.py");
    assert_eq!(kinds(&s.brief(None)), vec!["unchecked"]);
    assert_eq!(s.brief(None).headline()[1], "Checked: no test or check ran");
}

#[test]
fn a_guard_block_the_user_allowed_comes_first_with_its_action() {
    let mut s = Script::new();
    s.pass("pytest");
    s.edit("/repo/src/app.py");
    s.event(Event::EditProposed {
        tool_use_id: "x".into(),
        path: "tests/test_api.py".into(),
        old_text: "assert a == 1\nassert b == 2\n".into(),
        new_text: "assert a == 1\n".into(),
    });
    let d = s.c.allow(None, s.now + 1).expect("allow");
    s.keep(d);
    // The retry the user allowed: its guard note is not a second item.
    s.event(Event::EditProposed {
        tool_use_id: "y".into(),
        path: "tests/test_api.py".into(),
        old_text: "assert a == 1\nassert b == 2\n".into(),
        new_text: "assert a == 1\n".into(),
    });
    let b = s.brief(None);
    assert_eq!(kinds(&b), vec!["guard", "changed_after_pass"]);
    let g = &b.look_at[0];
    assert!(g.evidence.starts_with("a-"), "{g:?}");
    assert_eq!(
        g.text,
        "the test guard blocked an edit to tests/test_api.py; the user allowed it"
    );
    assert_eq!(b.belay.guard_blocks, 1);
}

#[test]
fn ci_and_dependency_changes_from_git_are_flagged_with_added_names() {
    let mut s = Script::new();
    s.edit("/repo/Cargo.toml");
    s.pass("cargo test");
    let git = vec![
        GitChange {
            change: FileChange {
                path: "Cargo.toml".into(),
                old: Some("[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\n".into()),
                new: Some(
                    "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\nleftpad = \"0.1\"\n\n[dev-dependencies.proptest]\nversion = \"1\"\n"
                        .into(),
                ),
            },
            added: 4,
            removed: 0,
        },
        GitChange {
            change: FileChange {
                path: ".github/workflows/ci.yml".into(),
                old: Some("on: push\n".into()),
                new: Some("on: push\n# skip\n".into()),
            },
            added: 1,
            removed: 0,
        },
        GitChange {
            change: FileChange {
                path: "src/app.py".into(),
                old: Some("import os\n".into()),
                new: Some("import os  # noqa\n".into()),
            },
            added: 1,
            removed: 1,
        },
    ];
    let b = s.brief(Some(git));
    assert_eq!(
        kinds(&b),
        vec!["check_config", "dependencies", "suppression"]
    );
    assert_eq!(
        b.look_at[1].text,
        "dependency manifest changed: Cargo.toml (added leftpad, proptest)"
    );
    assert_eq!(
        b.look_at[0].evidence, "git",
        "the ledger never saw the CI file"
    );
    assert_ne!(b.look_at[1].evidence, "git", "the ledger's edit");
    assert_eq!(b.headline()[0], "Changed: 3 files (+6 −1)");
    assert_eq!(b.headline()[2], "Look at: 3 items");
    assert_eq!(
        statuses(&b),
        vec![("cargo test", CheckStatus::PassedAfterLastEdit)]
    );
}

#[test]
fn without_git_a_shell_write_is_audited_from_the_ledger() {
    let mut s = Script::new();
    s.shell_write(
        "/repo/requirements.txt",
        "requests==2\n",
        "requests==2\nleft-pad>=1\n",
    );
    let b = s.brief(None);
    assert!(
        b.look_at
            .iter()
            .any(|l| l.text == "dependency manifest changed: requirements.txt (added left-pad)"),
        "{:?}",
        b.look_at
    );
    assert_eq!(b.headline()[0], "Changed: 1 file");
}

#[test]
fn verify_actions_and_the_belay_line() {
    let mut s = Script::new();
    s.pass("pytest");
    s.edit("/repo/src/app.py");
    s.event(Event::TurnEnded);
    // A summary that reached the agent, and one whose reply came too late (D9).
    for (id, seq) in [("a-90", 900), ("a-91", 901)] {
        s.entries.push(Entry {
            seq,
            ts_ms: s.now,
            causes: vec![],
            body: Body::Action {
                id: id.into(),
                action: ActionKind::Shape {
                    output_id: "o-9".into(),
                    from_tokens: 5000,
                    to_tokens: 300,
                    from_lines: 400,
                    to_lines: 20,
                },
                reason: "Long output.".into(),
                applied: true,
                unsupported: false,
            },
        });
    }
    s.entries.push(Entry {
        seq: 902,
        ts_ms: s.now,
        causes: vec![],
        body: Body::Undelivered {
            actions: vec!["a-91".into()],
            reason: "late".into(),
        },
    });
    let b = s.brief(None);
    assert_eq!(kinds(&b), vec!["verify", "changed_after_pass"]);
    assert!(b.look_at[0].evidence.starts_with("a-"));
    assert_eq!(b.belay.summaries, 1);
    assert_eq!(b.belay.tokens_kept_out, 4700);
    assert_eq!(b.duration_ms, Some(2 * 60_000), "first entry to last");
    // Stable JSON keys.
    let v = serde_json::to_value(&b).unwrap();
    for k in [
        "session",
        "duration_ms",
        "changed",
        "checked",
        "look_at",
        "belay",
    ] {
        assert!(v.get(k).is_some(), "{k}");
    }
    assert_eq!(v["checked"][0]["status"], "not_run_since_last_edit");
}

#[test]
fn dependency_names_per_manifest() {
    let names = |p: &str, t: &str| -> Vec<String> { dependency_names(p, t).into_iter().collect() };
    assert_eq!(
        names(
            "web/package.json",
            r#"{"dependencies":{"react":"1"},"devDependencies":{"jest":"2"}}"#
        ),
        ["jest", "react"]
    );
    assert_eq!(
        names(
            "pyproject.toml",
            "[project]\nname = \"x\"\ndependencies = [\n  \"Requests>=2\",\n  'click',\n]\n\n[tool.poetry.dependencies]\npython = \"^3.11\"\nrich = \"*\"\n"
        ),
        ["click", "requests", "rich"]
    );
    assert_eq!(
        names(
            "go.mod",
            "module x\n\nrequire github.com/a/b v1.0.0\nrequire (\n\tgolang.org/x/y v0.1.0\n)\n"
        ),
        ["github.com/a/b", "golang.org/x/y"]
    );
    assert_eq!(
        names("Gemfile", "source 'x'\ngem 'rails', '7'\n"),
        ["rails"]
    );
    assert_eq!(
        names("requirements-dev.txt", "# c\n-r base.txt\npytest>=8\n"),
        ["pytest"]
    );
    assert!(names("Cargo.lock", "[[package]]\nname = \"x\"\n").is_empty());
}
