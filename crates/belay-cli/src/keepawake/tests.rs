use super::*;

fn fixture(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/keepawake")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

// ---- WSL

#[test]
fn powercfg_lid_action_is_read_from_a_capture() {
    // Captured on a WSL2 laptop: Windows sleeps on lid close on both AC and battery.
    let text = fixture("powercfg_qh_lidaction_wsl.txt");
    assert_eq!(lid_action(&text), Some((1, 1)));
}

#[test]
fn powercfg_without_the_lid_setting_reads_as_none() {
    // `/q` (without h) leaves the hidden LIDACTION out on that machine.
    assert_eq!(
        lid_action(&fixture("powercfg_q_lidaction_hidden.txt")),
        None
    );
    assert_eq!(lid_action(""), None);
}

#[test]
fn powercfg_indices_after_lidaction_only() {
    // Another setting's indices before LIDACTION must not be taken.
    let text = "Power Setting GUID: a706 (Start menu power button)\r\n  GUID Alias: UIBUTTON_ACTION\r\n  Current AC Power Setting Index: 0x00000002\r\n  Current DC Power Setting Index: 0x00000002\r\nPower Setting GUID: 5ca8 (Lid close action)\r\n  GUID Alias: LIDACTION\r\n  Current AC Power Setting Index: 0x00000000\r\n  Current DC Power Setting Index: 0x00000001\r\n";
    assert_eq!(lid_action(text), Some((0, 1)));
}

#[test]
fn wsl_lid_do_nothing_is_held() {
    let a = assess_wsl(true, Some((0, 0)));
    assert!(a.all_held(), "{a:?}");
    assert_eq!(a.fix, None);
}

#[test]
fn wsl_lid_sleep_is_not_held_with_the_powercfg_fix() {
    let a = assess_wsl(true, Some((0, 1)));
    assert_eq!(a.idle_sleep, Guarantee::Held);
    assert_eq!(a.lid_closed, Guarantee::NotHeld);
    assert!(
        a.note
            .contains("do nothing on AC power and sleep on battery"),
        "{}",
        a.note
    );
    let fix = a.fix.unwrap();
    assert!(fix.contains("/setacvalueindex SCHEME_CURRENT SUB_BUTTONS LIDACTION 0"));
    assert!(fix.contains("/setdcvalueindex"));
    assert!(fix.contains("/setactive"));
    assert!(!fix.contains("sudo"));
}

#[test]
fn wsl_unknown_lid_and_no_interop() {
    assert_eq!(assess_wsl(true, None).lid_closed, Guarantee::Unknown);
    let a = assess_wsl(false, Some((0, 0)));
    assert_eq!(a.mechanism, "none");
    assert_eq!(a.idle_sleep, Guarantee::NotHeld);
    assert_eq!(a.lid_closed, Guarantee::NotHeld);
}

// ---- macOS

#[test]
fn pmset_power_source() {
    assert_eq!(on_battery(&fixture("pmset_batt_battery.txt")), Some(true));
    assert_eq!(on_battery(&fixture("pmset_batt_ac.txt")), Some(false));
    assert_eq!(on_battery("No batteries"), None);
}

#[test]
fn mac_on_battery_lid_is_not_held_with_the_pmset_fix() {
    let a = assess_mac(true, Some(true));
    assert_eq!(a.idle_sleep, Guarantee::Held);
    assert_eq!(a.lid_closed, Guarantee::NotHeld);
    assert!(a.fix.unwrap().contains("sudo pmset -b disablesleep 1"));
}

#[test]
fn mac_on_ac_never_claims_the_lid() {
    let a = assess_mac(true, Some(false));
    assert_eq!(a.lid_closed, Guarantee::Unknown);
    assert!(!a.all_held());
    assert_eq!(assess_mac(true, None).lid_closed, Guarantee::Unknown);
    assert_eq!(
        assess_mac(false, Some(false)).idle_sleep,
        Guarantee::NotHeld
    );
}

// ---- Linux

#[test]
fn systemd_inhibit_failures_are_told_apart() {
    assert_eq!(inhibit_outcome(true, ""), Inhibit::Accepted);
    assert_eq!(
        inhibit_outcome(false, &fixture("systemd_inhibit_no_bus.txt")),
        Inhibit::NoLogind
    );
    assert_eq!(
        inhibit_outcome(false, &fixture("systemd_inhibit_polkit_denied.txt")),
        Inhibit::Refused("Failed to inhibit: Access denied".into())
    );
    assert_eq!(
        inhibit_outcome(false, ""),
        Inhibit::Refused("no reason given".into())
    );
}

#[test]
fn logind_accepting_the_lid_switch_holds_both() {
    let a = assess_linux(&Inhibit::Accepted, &Inhibit::Missing);
    assert!(a.all_held(), "{a:?}");
    assert_eq!(a.mechanism, "systemd-inhibit");
}

#[test]
fn polkit_refusing_the_lid_switch_falls_back_to_idle() {
    let a = assess_linux(
        &Inhibit::Refused("Failed to inhibit: Access denied".into()),
        &Inhibit::Accepted,
    );
    assert_eq!(a.idle_sleep, Guarantee::Held);
    assert_eq!(a.lid_closed, Guarantee::NotHeld);
    assert!(a.note.contains("Access denied"), "{}", a.note);
    assert!(a.fix.unwrap().contains("HandleLidSwitch=ignore"));
}

#[test]
fn no_logind_or_no_tool_holds_nothing() {
    for (full, idle) in [
        (Inhibit::NoLogind, Inhibit::Missing),
        (Inhibit::Missing, Inhibit::Missing),
        (Inhibit::Refused("x".into()), Inhibit::Refused("x".into())),
    ] {
        let a = assess_linux(&full, &idle);
        assert_eq!(a.mechanism, "none", "{full:?}");
        assert_eq!(a.idle_sleep, Guarantee::NotHeld);
        assert_eq!(a.lid_closed, Guarantee::NotHeld);
    }
}

// ---- The rest

#[test]
fn summary_says_both_guarantees() {
    let s = assess_wsl(true, Some((1, 1))).summary();
    assert!(
        s.starts_with("Idle sleep held off, closed lid not held off (SetThreadExecutionState"),
        "{s}"
    );
}

#[test]
fn durations() {
    assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
    assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
    assert_eq!(parse_duration("45m").unwrap(), Duration::from_secs(2700));
    assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_secs(5400));
    assert_eq!(parse_duration("2d").unwrap(), Duration::from_secs(172_800));
    for bad in ["", "h", "5x", "1h30", "0s", "-5m"] {
        assert!(parse_duration(bad).is_err(), "{bad}");
    }
}

#[test]
fn output_within_kills_a_stuck_command() {
    let start = Instant::now();
    let r = output_within(Command::new("sleep").arg("10"), Duration::from_millis(200));
    assert_eq!(r.unwrap_err().kind(), io::ErrorKind::TimedOut);
    assert!(start.elapsed() < Duration::from_secs(3));
}

/// Holds and releases on this machine, for real. Skipped where belay has no mechanism.
#[test]
fn holds_while_the_pid_lives_and_releases() {
    let a = assess();
    if a.idle_sleep != Guarantee::Held {
        eprintln!("skipped: {}", a.summary());
        return;
    }
    let mut target = Command::new("sleep").arg("30").spawn().unwrap();
    let mut guard = hold("belay test", target.id()).unwrap();
    thread::sleep(Duration::from_millis(1500));
    assert!(
        guard.alive(),
        "the holder stopped early ({})",
        guard.mechanism
    );

    // The holder ends by itself once the held process ends.
    target.kill().unwrap();
    target.wait().unwrap();
    let start = Instant::now();
    while guard.alive() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the holder outlived its pid"
        );
        thread::sleep(Duration::from_millis(100));
    }

    // Dropping a live guard stops its holder.
    let me = std::process::id();
    let mut guard = hold("belay test", me).unwrap();
    thread::sleep(Duration::from_millis(500));
    assert!(guard.alive());
    let pids: Vec<u32> = guard.children.iter().map(Child::id).collect();
    drop(guard);
    for p in pids {
        assert!(!Path::new(&format!("/proc/{p}")).exists() || cfg!(target_os = "macos"));
    }
}
