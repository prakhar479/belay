use super::*;

/// A machine where everything is in order.
fn good() -> Facts {
    Facts {
        version: "0.2.0".into(),
        exe: Some("/bin/belay".into()),
        on_path: Some("/bin/belay".into()),
        same_binary: true,
        claude_bin: "claude".into(),
        claude_version: Some("2.1.316".into()),
        conformant: vec!["2.1.308".into(), "2.1.316".into()],
        home: "/h/.local/share/belay".into(),
        home_state: HomeState::Mode(0o40700),
        config_path: "/h/.config/belay/config.toml".into(),
        config_text: None,
        plugin: Plugin::Installed(Some("0.2.0".into())),
        alias_place: "/h/.bashrc".into(),
        alias_line: "alias claude='belay claude'".into(),
        alias: true,
        statusline: Some(("belay status".into(), true)),
        sandbox: SandboxFacts {
            linux: true,
            bwrap: true,
            userns_error: None,
            wsl: false,
            interop_hidden: true,
            systemd_scope: true,
        },
        data: DataFacts {
            home_bytes: 2_000_000,
            sessions: 3,
            live: 0,
            oldest_days: Some(4),
            keep_days: 30,
        },
        ..Default::default()
    }
}

fn of<'a>(all: &'a [Check], area: &str) -> Vec<&'a Check> {
    all.iter().filter(|c| c.area == area).collect()
}

fn one(f: &Facts, area: &str) -> Check {
    let all = checks(f);
    let v = of(&all, area);
    assert_eq!(v.len(), 1, "{area}: {v:?}");
    v[0].clone()
}

#[test]
fn a_good_machine_is_all_ok() {
    let all = checks(&good());
    let bad: Vec<_> = all.iter().filter(|c| c.status != Status::Ok).collect();
    assert!(bad.is_empty(), "{bad:?}");
    assert!(all.iter().all(|c| c.fix.is_none()));
}

#[test]
fn every_warn_or_fail_says_the_fix_except_no_sandbox_os() {
    let mut f = good();
    f.on_path = None;
    f.claude_version = Some("9.9.9".into());
    f.plugin = Plugin::Absent;
    f.alias = false;
    f.statusline = None;
    f.sandbox.bwrap = false;
    f.data.oldest_days = Some(99);
    for c in checks(&f) {
        if c.status != Status::Ok {
            assert!(c.fix.is_some(), "{c:?}");
        }
    }
}

#[test]
fn claude_version_against_the_conformance_list() {
    let mut f = good();
    assert_eq!(one(&f, "claude").status, Status::Ok);
    f.claude_version = Some("2.1.400".into());
    let c = one(&f, "claude");
    assert_eq!(c.status, Status::Warn);
    assert!(c.text.contains("2.1.400"));
    assert!(c.fix.unwrap().contains("scripts/conformance.py"));
    f.claude_version = None;
    assert_eq!(one(&f, "claude").status, Status::Fail);
}

#[test]
fn the_conformance_list_is_baked_in_from_docs() {
    let v = conformant();
    assert!(!v.is_empty());
    assert!(v.windows(2).all(|w| w[0] <= w[1]), "sorted: {v:?}");
    assert!(v.iter().all(|x| !x.ends_with(".md")));
}

#[test]
fn binary_on_path_mismatch_warns() {
    let mut f = good();
    f.on_path = Some("/usr/local/bin/belay".into());
    f.same_binary = false;
    let c = one(&f, "belay");
    assert_eq!(c.status, Status::Warn);
    assert!(c.text.contains("/usr/local/bin/belay"));
}

#[test]
fn data_dir_states() {
    let mut f = good();
    f.home_state = HomeState::Symlink;
    let all = checks(&f);
    assert!(of(&all, "home").iter().any(|c| c.status == Status::Fail));
    f.home_state = HomeState::Mode(0o40755);
    let all = checks(&f);
    assert!(of(&all, "home")
        .iter()
        .any(|c| c.status == Status::Warn && c.text.contains("755")));
}

#[test]
fn plugin_installed_and_its_version() {
    let text =
        r#"{"version": 2, "plugins": {"belay@belay": [{"scope": "user", "version": "0.1.0"}]}}"#;
    assert_eq!(plugin_of(text), Plugin::Installed(Some("0.1.0".into())));
    assert_eq!(
        plugin_of(r#"{"plugins": {"belay@belay": [{"scope": "user"}]}}"#),
        Plugin::Installed(None)
    );
    assert_eq!(plugin_of(r#"{"plugins": {"other@x": []}}"#), Plugin::Absent);
    assert_eq!(plugin_of("not json"), Plugin::Absent);

    let mut f = good();
    f.plugin = plugin_of(text);
    let c = one(&f, "plugin");
    assert_eq!(c.status, Status::Warn);
    assert!(
        c.text.contains("0.1.0") && c.text.contains("0.2.0"),
        "{c:?}"
    );
    f.plugin = Plugin::Absent;
    assert_eq!(one(&f, "plugin").status, Status::Warn);
}

#[test]
fn alias_is_found_and_optional_with_the_plugin() {
    assert!(has_alias("export X=1\nalias claude='belay claude'\n"));
    assert!(has_alias(
        "function claude --wraps='belay claude'\n  belay claude $argv\nend\n"
    ));
    assert!(!has_alias("# alias claude='belay claude'\n"));
    let mut f = good();
    f.alias = false;
    assert_eq!(one(&f, "alias").status, Status::Ok);
    f.plugin = Plugin::Absent;
    let c = one(&f, "alias");
    assert_eq!(c.status, Status::Warn);
    assert!(c.fix.unwrap().contains("alias claude="));
}

#[test]
fn statusline_not_belays_warns() {
    let mut f = good();
    f.statusline = Some(("my-line".into(), false));
    let c = one(&f, "statusline");
    assert_eq!(c.status, Status::Warn);
    assert!(c.text.contains("my-line"));
    f.statusline = None;
    assert_eq!(one(&f, "statusline").status, Status::Warn);
}

#[test]
fn sandbox_readiness() {
    let mut f = good();
    f.sandbox.bwrap = false;
    let all = checks(&f);
    let s = of(&all, "sandbox");
    assert!(s
        .iter()
        .any(|c| c.status == Status::Warn && c.text.contains("bubblewrap")));
    assert!(s.iter().all(|c| c.status != Status::Fail));

    let mut f = good();
    f.sandbox.userns_error = Some("setting up uid map: Permission denied".into());
    let all = checks(&f);
    assert!(of(&all, "sandbox")
        .iter()
        .any(|c| c.status == Status::Warn && c.text.contains("uid map")));

    let mut f = good();
    f.sandbox.wsl = true;
    let all = checks(&f);
    assert!(of(&all, "sandbox")
        .iter()
        .any(|c| c.status == Status::Ok && c.text.contains("sandbox doctor")));
    f.sandbox.interop_hidden = false;
    let all = checks(&f);
    assert!(of(&all, "sandbox").iter().any(|c| c.status == Status::Fail));

    let mut f = good();
    f.sandbox.systemd_scope = false;
    let all = checks(&f);
    assert!(of(&all, "sandbox")
        .iter()
        .any(|c| c.status == Status::Ok && c.text.contains("watchdog alone")));

    f.sandbox.linux = false;
    let all = checks(&f);
    assert_eq!(of(&all, "sandbox").len(), 1);
}

#[test]
fn the_real_layout_hides_wsl_interop() {
    assert!(interop_hidden());
}

#[test]
fn skills_drift_and_pending_warn() {
    let mut f = good();
    assert_eq!(one(&f, "skills").status, Status::Ok);
    f.skills = vec![
        ("ok", "2 skill(s) match their lock".into()),
        (
            "warn",
            "skill x (user) differs from what you approved; see `belay skill list`".into(),
        ),
        ("info", "1 skill(s) wait for `belay skill approve`".into()),
    ];
    let all = checks(&f);
    let s = of(&all, "skills");
    assert_eq!(s.len(), 3);
    assert_eq!(s[0].status, Status::Ok);
    assert_eq!(s[1].status, Status::Warn);
    assert_eq!(s[2].status, Status::Warn);
    assert!(s[2].fix.as_deref().unwrap().contains("approve"));
}

#[test]
fn old_config_keys_point_to_the_fix() {
    assert_eq!(
        old_keys(
            "fleet = look\nlearn = off # was\nmemory = look\nloop_threshold = 3\n# learn = on\n"
        ),
        vec!["fleet", "learn", "memory = look"]
    );
    let mut f = good();
    f.config_text = Some("plugin_mode = optimize\n".into());
    f.config_warnings =
        vec!["line 1: `plugin_mode` is ignored: belay has no modes now (see `belay pause`)".into()];
    let c = one(&f, "config");
    assert_eq!(c.status, Status::Warn);
    assert!(c.text.contains("plugin_mode"));
    assert!(c.fix.as_deref().unwrap().contains("guide"));
    f.has_config_migrate = true;
    assert!(one(&f, "config")
        .fix
        .unwrap()
        .contains("belay config migrate"));

    // A bad line still gets its own warning.
    f.config_text = Some("loop_treshold = 4\n".into());
    f.config_warnings = vec!["line 1: unknown key `loop_treshold`".into()];
    assert!(one(&f, "config").text.contains("loop_treshold"));

    f.config_text = Some("loop_threshold = 4\n".into());
    f.config_warnings.clear();
    f.config_overrides = 1;
    assert_eq!(one(&f, "config").status, Status::Ok);
}

#[test]
fn old_sessions_past_keep_days_warn() {
    let mut f = good();
    let c = one(&f, "data");
    assert!(
        c.text.contains("2.0 MB") && c.text.contains("3 sessions"),
        "{c:?}"
    );
    f.data.oldest_days = Some(45);
    let c = one(&f, "data");
    assert_eq!(c.status, Status::Warn);
    assert!(c.fix.unwrap().contains("--older-than 30"));
    f.data.keep_days = 0;
    let c = one(&f, "data");
    assert_eq!(c.status, Status::Ok);
    assert!(!c.text.contains("deletes"));
}

#[test]
fn json_shape_and_exit_code() {
    let mut f = good();
    f.claude_version = None;
    let all = checks(&f);
    let v: Value = serde_json::from_str(&to_json(&all)).unwrap();
    assert_eq!(v["ok"], false);
    let list = v["checks"].as_array().unwrap();
    assert_eq!(list.len(), all.len());
    let c = list.iter().find(|c| c["area"] == "claude").unwrap();
    assert_eq!(c["status"], "fail");
    assert!(c["text"].is_string() && c["fix"].is_string());
    let ok = list.iter().find(|c| c["area"] == "belay").unwrap();
    assert_eq!(ok["status"], "ok");
    assert!(ok.get("fix").is_none());

    let v: Value = serde_json::from_str(&to_json(&checks(&good()))).unwrap();
    assert_eq!(v["ok"], true);
}

#[test]
fn text_lines_have_status_area_and_fix() {
    let mut f = good();
    f.statusline = None;
    let out = render(&checks(&f));
    let line = out.lines().find(|l| l.contains("statusline")).unwrap();
    assert!(line.starts_with("warn "), "{line}");
    assert!(
        line.contains(" Fix: `belay install --statusline`."),
        "{line}"
    );
    assert!(out
        .lines()
        .all(|l| ["ok ", "warn ", "fail "].iter().any(|s| l.starts_with(s))));
}

#[test]
fn disk_use_is_one_line_and_warns_past_10_gb() {
    // A fake belay home: a fleet worktree with a build dir, a sandbox, a cache.
    let h = std::env::temp_dir().join(format!("belay-doctor-disk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&h);
    for (p, n) in [
        ("fleets/f1/worktrees/worker-1/target/x", 3_000_000),
        ("sandboxes/sb-1/work/a", 1_000_000),
        ("sessions/s1/ledger.jsonl", 10_000),
        ("cache/repo-1/worker-1/target/x", 2_000_000),
    ] {
        let p = h.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, vec![0u8; n]).unwrap();
    }
    let f = Facts {
        disk: disk::usage(&h),
        ..good()
    };
    let c = one(&f, "disk");
    assert_eq!(c.status, Status::Ok, "{c:?}");
    assert!(c.text.contains("belay data 6.0 MB"), "{}", c.text);
    assert!(c.text.contains("fleets 3.0 MB"), "{}", c.text);
    assert!(c.text.contains("cache 2.0 MB"), "{}", c.text);
    assert!(
        c.text.contains(&format!(
            "largest: {} (3.0 MB)",
            h.join("fleets/f1/worktrees/worker-1").display()
        )),
        "{}",
        c.text
    );
    let _ = std::fs::remove_dir_all(&h);

    let big = disk::Usage {
        parts: vec![("fleets", 46_000_000_000), ("cache", 2_000_000_000)],
        largest: Some(("/h/fleets/f/worktrees/worker-1".into(), 3_100_000_000)),
        rest: 0,
    };
    let c = one(
        &Facts {
            disk: big,
            ..good()
        },
        "disk",
    );
    assert_eq!(c.status, Status::Warn);
    assert!(c.text.contains("belay data 48.0 GB"), "{}", c.text);
    assert!(c.fix.unwrap().contains("belay fleet prune --yes"));
}
