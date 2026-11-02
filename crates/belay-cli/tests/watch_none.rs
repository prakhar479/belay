//! `belay watch` with no live session says so and exits instead of waiting forever.

use std::process::Command;

#[test]
fn watch_with_no_live_session_says_so_and_exits() {
    let home = std::env::temp_dir().join(format!("belay-watch-none-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_belay"))
        .arg("watch")
        .env("BELAY_HOME", &home)
        .env("HOME", &home)
        .env_remove("BELAY_SESSION_DIR")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("No live belay session to watch. Start one with `belay claude`."),
        "{text}"
    );
}
