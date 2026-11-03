use super::*;

#[test]
fn a_read_count_past_a_shorter_inbox_is_not_zero_forever() {
    let dir = std::env::temp_dir().join(format!("belay-unread-short-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("inbox.jsonl"), "{}\n{}\n").unwrap();
    fs::write(dir.join("inbox.read"), "5").unwrap();
    assert_eq!(unread_for_user(&dir), 2);
    fs::write(dir.join("inbox.read"), "1").unwrap();
    assert_eq!(unread_for_user(&dir), 1);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_non_utf8_byte_in_the_ledger_hides_only_its_line() {
    let dir = std::env::temp_dir().join(format!("belay-ledger-utf8-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let good = r#"{"seq":1,"ts_ms":5,"body":{"kind":"event","event":{"type":"usage","slot":"w-1","tokens":3,"cost_usd":0.1}}}"#;
    let mut bytes = format!("{good}\n").into_bytes();
    bytes.extend_from_slice(b"{\"seq\":2,\xff\xfe\n");
    bytes.extend_from_slice(format!("{good}\n").as_bytes());
    fs::write(dir.join("ledger.jsonl"), bytes).unwrap();
    let n = read_ledger(&dir).len();
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(n, 2);
}

#[test]
fn usage_and_the_usage_limit_come_from_result_lines() {
    let line = r#"{"type":"result","subtype":"success","total_cost_usd":0.0035,"usage":{"input_tokens":2,"cache_creation_input_tokens":242,"cache_read_input_tokens":22345,"output_tokens":5},"result":"ok"}"#;
    assert_eq!(
        usage_of(line, "w-1"),
        Some(FleetEvent::Usage {
            slot: "w-1".into(),
            tokens: 22594,
            cost_usd: 0.0035
        })
    );
    assert!(limit_of(line, "w-1", 0).is_none());
    let hit =
        r#"{"type":"result","is_error":true,"result":"Claude AI usage limit reached|1792630000"}"#;
    assert!(matches!(
        limit_of(hit, "w-1", 0),
        Some(FleetEvent::UsageLimit { .. })
    ));
    // An agent's long answer that mentions usage limits is not one.
    let talk = format!(
        r#"{{"type":"result","result":"{}"}}"#,
        "the API has a usage limit; ".repeat(20)
    );
    assert!(limit_of(&talk, "w-1", 0).is_none());
    // A short answer that mentions a limit is not one either (dogfood review).
    let short = r#"{"type":"result","result":"Done. Note: the API has a usage limit of 100/min."}"#;
    assert!(limit_of(short, "w-1", 0).is_none());
    let reached = r#"{"type":"result","result":"You've hit your session limit · resets 4pm"}"#;
    assert!(limit_of(reached, "w-1", 0).is_some());
    // Claude Code 2.1.316 names the window (dogfood 0.2: three attempts each were
    // lost to it as failures).
    let five_hour = include_str!("../../../../fixtures/fleet/result_five_hour_limit.json");
    assert!(limit_of(five_hour.trim(), "w-1", 0).is_some());
    let weekly = r#"{"type":"result","result":"You've hit your weekly limit · resets Mon"}"#;
    assert!(limit_of(weekly, "w-1", 0).is_some());
    let unrelated =
        r#"{"type":"result","result":"You've hit your stride; the limit test passes."}"#;
    assert!(limit_of(unrelated, "w-1", 0).is_none());
}

#[test]
fn an_exact_fleet_id_wins_over_longer_ids_it_starts() {
    let dirs = vec![PathBuf::from("/f/100-1"), PathBuf::from("/f/100-12")];
    assert_eq!(pick(&dirs, "100-1").unwrap(), PathBuf::from("/f/100-1"));
    assert!(pick(&dirs, "100-").is_err());
}

#[test]
fn a_check_that_leaves_a_background_process_still_finishes() {
    let d = std::env::temp_dir();
    let t = std::time::Instant::now();
    let (code, out) = run_with_timeout(&d, "sleep 30 & echo hi", &[], Duration::from_secs(60));
    assert_eq!(code, Some(0), "{out}");
    assert!(
        t.elapsed() < Duration::from_secs(10),
        "waited for the leftover"
    );
}

#[test]
fn a_check_with_a_lot_of_output_still_finishes() {
    let d = std::env::temp_dir();
    let (code, out) = run_with_timeout(
        &d,
        "head -c 6000000 /dev/zero | tr '\\0' x; echo done",
        &[],
        Duration::from_secs(20),
    );
    assert_eq!(code, Some(0), "{}", out.len());
}

#[test]
fn output_that_is_not_utf8_does_not_stop_the_reader() {
    let d = std::env::temp_dir().join(format!("belay-readout-{}", std::process::id()));
    fs::create_dir_all(&d).unwrap();
    let log = File::create(d.join("o.jsonl")).unwrap();
    let (tx, rx) = mpsc::channel();
    let mut input = b"\xff\xfe junk\n".to_vec();
    input.extend(br#"{"type":"result","result":"ok"}"#);
    input.push(b'\n');
    read_output(&input[..], log, "w-1", &tx);
    assert!(rx.try_iter().any(|m| matches!(m, Msg::TurnEnded(_))));
}

#[test]
fn a_sessions_unread_messages_are_those_its_ledger_has_not_seen() {
    let d = std::env::temp_dir().join(format!("belay-unread-{}", std::process::id()));
    fs::create_dir_all(&d).unwrap();
    let inbox = d.join("in.jsonl");
    fs::write(&inbox, "{}\n{}\n{}\n").unwrap();
    let seen = belay_core::Entry {
        seq: 1,
        ts_ms: 0,
        causes: vec![],
        body: belay_core::Body::Event {
            event: belay_core::Event::Message {
                from: "lead-1".into(),
                kind: "answer".into(),
                text: "x".into(),
                channel: None,
                from_session: None,
                repo: None,
            },
        },
    };
    assert_eq!(unread_for_session(&inbox, &[seen]), 2);
    assert_eq!(unread_for_session(&d.join("missing"), &[]), 0);
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn the_tick_wait_is_what_is_left_of_a_tick_and_never_more() {
    assert_eq!(tick_wait(Duration::ZERO), TICK);
    assert_eq!(tick_wait(TICK / 2), TICK / 2);
    assert_eq!(tick_wait(TICK * 5), Duration::ZERO);
}

#[test]
fn a_torn_ledger_line_does_not_swallow_the_next_record() {
    let d = std::env::temp_dir().join(format!("belay-torn-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    let p = d.join("ledger.jsonl");
    fs::write(&p, "{\"torn\":").unwrap();
    let f = OpenOptions::new().append(true).open(&p).unwrap();
    end_torn_line(&p, &f);
    assert_eq!(fs::read_to_string(&p).unwrap().lines().count(), 1);
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn uuids_are_version_4() {
    let u = uuid_v4().unwrap();
    assert_eq!(u.len(), 36);
    assert_eq!(&u[14..15], "4");
    assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"), "{u}");
    assert_ne!(u, uuid_v4().unwrap());
}

/// A down fleet "F1" over a temp repo: `belay/F1/merged` is in HEAD,
/// `belay/F1/open` has one commit HEAD lacks, `belay/sandbox/sb-1` is in HEAD too.
fn down_fleet(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("belay-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let repo = root.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(ok.status.success(), "git {args:?}: {ok:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);
    let base = git_out(&repo, &["rev-parse", "HEAD"]).unwrap();
    for b in ["belay/F1/merged", "belay/sandbox/sb-1"] {
        git(&["checkout", "-qb", b, "main"]);
        fs::write(repo.join(format!("{}.txt", b.replace('/', "-"))), "x\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", b]);
        git(&["checkout", "-q", "main"]);
        git(&["merge", "-q", "--no-ff", "-m", "merge", b]);
    }
    git(&["checkout", "-qb", "belay/F1/open", "main"]);
    fs::write(repo.join("open.txt"), "o\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "open"]);
    git(&["checkout", "-q", "main"]);
    let fleet = root.join("fleets").join("F1");
    fs::create_dir_all(&fleet).unwrap();
    let file: SpecFile = serde_json::from_value(serde_json::json!({"preset": "pool"})).unwrap();
    let meta = Meta {
        id: "F1".into(),
        repo: Some(repo.display().to_string()),
        base: Some(base),
        cwd: repo.display().to_string(),
        mode: None,
        created_ms: 1,
        spec: spec::resolve(&file).unwrap(),
    };
    fs::write(
        fleet.join("meta.json"),
        serde_json::to_string(&meta).unwrap(),
    )
    .unwrap();
    (root, repo, fleet)
}

#[test]
fn prune_branches_deletes_merged_keeps_the_rest() {
    let (root, repo, fleet) = down_fleet("prune-branches");
    let branch = |b: &str| git_out(&repo, &["rev-parse", "--verify", "-q", b]).is_some();

    let dry = prune_dirs(std::slice::from_ref(&fleet), false, true);
    assert!(
        dry.contains("would delete branch belay/F1/merged (merged)"),
        "{dry}"
    );
    assert!(
        dry.contains("keep   branch belay/F1/open (1 commit not in HEAD)"),
        "{dry}"
    );
    assert!(!dry.contains("sandbox"), "{dry}");
    assert!(dry.contains("--branches --yes"), "{dry}");
    assert!(branch("belay/F1/merged"), "a listing deletes nothing");

    let plain = prune_dirs(std::slice::from_ref(&fleet), true, false);
    assert!(!plain.contains("branch belay/"), "{plain}");
    assert!(
        branch("belay/F1/merged"),
        "no --branches, no branch deleted"
    );

    let done = prune_dirs(std::slice::from_ref(&fleet), true, true);
    assert!(done.contains("deleted branch belay/F1/merged"), "{done}");
    assert!(done.contains("Deleted 1 merged branch, kept 1."), "{done}");
    assert!(!branch("belay/F1/merged"));
    assert!(branch("belay/F1/open"), "unmerged work is kept");
    assert!(
        branch("belay/sandbox/sb-1"),
        "sandbox branches are not a fleet's"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn down_keeps_a_worktree_whose_task_is_not_done() {
    let (root, repo, fleet) = down_fleet("prune-hold");
    for w in ["worker-1", "worker-2"] {
        let p = fleet.join("worktrees").join(w);
        assert!(git_ok(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                p.to_str().unwrap(),
                "main"
            ]
        ));
    }
    let hold = |name: &str| (name == "worker-1").then(|| "task t-1 is not done".to_string());
    let mut out = String::new();
    let mut counts = (0, 0);
    prune_fleet(
        &fleet,
        &read_meta(&fleet).unwrap(),
        true,
        &hold,
        &mut out,
        &mut counts,
    );
    assert_eq!(counts, (1, 1), "{out}");
    assert!(out.contains("worker-1 (task t-1 is not done)"), "{out}");
    assert!(fleet.join("worktrees/worker-1").exists());
    assert!(!fleet.join("worktrees/worker-2").exists(), "{out}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn disk_use_warns_past_10_gb() {
    let small = crate::disk::Usage {
        parts: vec![("fleets", 1_000_000)],
        ..Default::default()
    };
    assert!(disk_line(&small).starts_with("belay data 1.0 MB"));
    let big = crate::disk::Usage {
        parts: vec![("fleets", 46_000_000_000)],
        ..Default::default()
    };
    let line = disk_line(&big);
    assert!(line.starts_with("warn: belay data 46.0 GB"), "{line}");
    assert!(line.contains("belay fleet prune --yes"), "{line}");
}

#[test]
fn a_down_pruned_fleet_is_listed_only_with_all() {
    let (root, _repo, fleet) = down_fleet("list-quiet");
    let dirs = std::slice::from_ref(&fleet);
    assert_eq!(list_dirs(dirs, false), "");
    assert!(list_dirs(dirs, true).starts_with("fleet F1 · down"));
    // A worktree left to prune keeps it on the board.
    fs::create_dir_all(fleet.join("worktrees").join("worker-1")).unwrap();
    assert!(list_dirs(dirs, false).starts_with("fleet F1 · down"));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_restarted_hub_takes_back_its_work_records() {
    let file: SpecFile = serde_json::from_value(serde_json::json!({"preset": "lead"})).unwrap();
    let mut m = Model::new(spec::resolve(&file).unwrap());
    let snap = m.record_work(
        Work::Snapshot {
            task: "t-1".into(),
            tree: "abc".into(),
        },
        "t-1 is done.".into(),
        vec![],
        1,
    );
    let brought = m.record_work(
        Work::Brought {
            task: "t-1".into(),
            slot: "worker-2".into(),
            conflicts: vec![],
        },
        "t-3 depends on t-1.".into(),
        vec![snap.seq],
        2,
    );
    let (mut trees, mut integrated) = (BTreeMap::new(), std::collections::BTreeSet::new());
    for r in [&snap, &brought] {
        restore_work(r, &mut trees, &mut integrated);
    }
    assert_eq!(trees["t-1"], ("abc".to_string(), snap.seq));
    assert!(integrated.contains(&("worker-2".to_string(), "t-1".to_string())));
}

#[test]
fn a_silent_client_does_not_hold_up_the_next_one() {
    let d = std::env::temp_dir().join(format!("belay-hub-listen-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    let sock = socket_path(&d);
    let listener = UnixListener::bind(&sock).unwrap();
    let (tx, _rx) = mpsc::channel();
    thread::spawn(move || listen(listener, tx));
    // Connects and never writes: before, the next client waited the 2 s read timeout.
    let _silent = UnixStream::connect(&sock).unwrap();
    let t = std::time::Instant::now();
    assert!(is_up(&d));
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn a_missing_hub_is_not_running_and_a_dead_socket_too() {
    let d = std::env::temp_dir().join(format!("belay-hub-ask-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    let e = send_cli(&d, &HubRequest::Ping).unwrap_err().to_string();
    assert_eq!(e, "the fleet's hub is not running");
    // A socket nobody listens on any more (a hub that crashed).
    crate::ipc::dead_socket(&socket_path(&d));
    let e = send_cli(&d, &HubRequest::Ping).unwrap_err().to_string();
    assert_eq!(e, "the fleet's hub is not running");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn fleet_files_parse_from_toml() {
    let d = std::env::temp_dir().join(format!("belay-hub-toml-{}", std::process::id()));
    fs::create_dir_all(&d).unwrap();
    let f = d.join("fleet.toml");
    fs::write(
        &f,
        r#"
preset = "lead"
check  = "make check"

[role.lead]
model = "opus"

[role.worker]
model         = "sonnet"
replicas      = "1..3"
may_not_write = ["tests/**"]
budget        = { tokens = "2M", wall = "45m", attempts = 3 }

[limits]
sessions = 5
tokens   = "20M"
"#,
    )
    .unwrap();
    let (spec, _) = load_spec(Some(&f), None).unwrap();
    assert_eq!(spec.roles["worker"].max, 3);
    assert_eq!(spec.limits.sessions, 5);
    fs::write(
        &f,
        "preset = \"lead\"\n[role.worker]\nreplicas = \"9..1\"\n",
    )
    .unwrap();
    let e = load_spec(Some(&f), None).unwrap_err().to_string();
    assert!(
        e.contains("1 problem") && e.contains("9..1 is empty"),
        "{e}"
    );
    fs::write(&f, "preset = \"lead\"\nrolez = 3\n").unwrap();
    let e = load_spec(Some(&f), None).unwrap_err().to_string();
    assert!(e.contains("rolez"), "{e}");
    assert!(load_spec(None, None).is_err(), "nothing to run");
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn a_worktree_whose_changes_are_in_head_is_taken() {
    let d = std::env::temp_dir().join(format!("belay-taken-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    let repo = d.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |at: &Path, args: &[&str]| {
        assert!(Command::new("git")
            .arg("-C")
            .arg(at)
            .args([
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false"
            ])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());
    };
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git_out(&repo, &["rev-parse", "HEAD"]).unwrap();
    git(&repo, &["worktree", "add", "-q", "../wt"]);
    let wt = d.join("wt");
    fs::write(wt.join("a.txt"), "a\nb\n").unwrap();
    fs::write(wt.join("new.txt"), "n\n").unwrap();
    assert!(!changes_in_head(&repo, &wt, &base), "not taken yet");
    // The user takes it as a patch, and commits.
    fs::write(repo.join("a.txt"), "a\nb\n").unwrap();
    fs::write(repo.join("new.txt"), "n\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "taken"]);
    assert!(changes_in_head(&repo, &wt, &base));
    // HEAD moves on past the taken change: the files differ, but a merge adds nothing.
    fs::write(repo.join("a.txt"), "first\na\nb\n").unwrap();
    git(&repo, &["commit", "-qam", "later"]);
    assert!(!changes_in_head(&repo, &wt, &base));
    assert!(merges_to_nothing(&repo, &wt));
    // One more change in the worktree: not taken.
    fs::write(wt.join("new.txt"), "n\nmore\n").unwrap();
    assert!(!merges_to_nothing(&repo, &wt));
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn a_check_killed_by_a_signal_says_so() {
    let d = std::env::temp_dir();
    let (code, out) =
        run_with_timeout(&d, "echo partial; kill -9 $$", &[], Duration::from_secs(20));
    assert_eq!(code, None);
    assert!(out.contains("killed by signal 9"), "{out}");
}

#[test]
fn new_files_with_quoted_names_are_counted_and_listed() {
    let d = std::env::temp_dir().join(format!("belay-quoted-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&d)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.email=t@t",
        "-c",
        "user.name=t",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "base",
    ]);
    fs::write(d.join("a\"b.txt"), "1\n2\n3\n").unwrap();
    fs::write(d.join(" lead.txt"), "1\n").unwrap();
    assert_eq!(diff_size(&d, None), Some(4));
    let mut names = untracked(&d);
    names.sort();
    assert_eq!(names, vec![" lead.txt".to_string(), "a\"b.txt".to_string()]);
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn changes_only_staged_in_the_repo_are_not_in_head() {
    let d = std::env::temp_dir().join(format!("belay-staged-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    let repo = d.join("repo");
    fs::create_dir_all(&repo).unwrap();
    let git = |at: &Path, args: &[&str]| {
        assert!(Command::new("git")
            .arg("-C")
            .arg(at)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());
    };
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git_out(&repo, &["rev-parse", "HEAD"]).unwrap();
    git(&repo, &["worktree", "add", "-q", "../wt"]);
    let wt = d.join("wt");
    fs::write(wt.join("a.txt"), "a\nb\n").unwrap();
    // The user staged the same change but did not commit it: HEAD lacks it.
    fs::write(repo.join("a.txt"), "a\nb\n").unwrap();
    git(&repo, &["add", "."]);
    assert!(!changes_in_head(&repo, &wt, &base), "staged is not in HEAD");
    git(&repo, &["commit", "-qm", "taken"]);
    assert!(changes_in_head(&repo, &wt, &base));
    let _ = fs::remove_dir_all(&d);
}

#[test]
fn changed_files_lists_non_ascii_names_unquoted() {
    let d = std::env::temp_dir().join(format!("belay-cf-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .arg("-C")
            .arg(&d)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "-q"]);
    fs::write(d.join("é.txt"), "a\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "base"]);
    let base = git_out(&d, &["rev-parse", "HEAD"]).unwrap();
    fs::write(d.join("é.txt"), "b\n").unwrap();
    fs::write(d.join("ü new.txt"), "n\n").unwrap();
    assert_eq!(changed_files(&d, &base), vec!["é.txt", "ü new.txt"]);
    let _ = fs::remove_dir_all(&d);
}
