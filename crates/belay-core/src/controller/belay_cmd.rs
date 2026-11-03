//! Recognizing the agent running belay's user-only commands (rule 12).

use super::*;

/// A simple command of `command` runs `belay allow` (any path to belay). Text inside
/// quotes, such as a commit message, does not count.
pub(super) fn runs_belay_allow(command: &str) -> bool {
    runs_belay(command, "allow")
}

/// The words after `belay` in each simple command, once per way to read leading global
/// flags: `--flag`, `--flag=value` and `--flag value` are skipped, and a bare `--flag`
/// may or may not take a value, so both readings are offered (rule 12: a flag before the
/// verb must not hide it).
pub(super) fn belay_args(command: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for seg in &shell::parse(command).segments {
        let w = shell::program_words(seg);
        if w.len() < 2 || shell::program_name(&w[0]) != "belay" {
            continue;
        }
        let mut starts = vec![1usize];
        let mut i = 1;
        while i < w.len() && w[i].starts_with('-') {
            let takes_value =
                !w[i].contains('=') && w.get(i + 1).is_some_and(|n| !n.starts_with('-'));
            if takes_value {
                starts.push(i + 2);
            }
            i += 1;
            starts.push(i);
        }
        starts.sort_unstable();
        starts.dedup();
        out.extend(
            starts
                .into_iter()
                .filter(|&s| s < w.len())
                .map(|s| w[s..].to_vec()),
        );
    }
    out
}

/// A simple command runs `belay <verb>`.
pub(super) fn runs_belay(command: &str, verb: &str) -> bool {
    belay_args(command).iter().any(|w| w[0] == verb)
}

/// A simple command runs `belay sandbox rm` (rule 12).
pub(super) fn runs_belay_sandbox_rm(command: &str) -> bool {
    belay_args(command)
        .iter()
        .any(|w| w.len() >= 2 && w[0] == "sandbox" && w[1] == "rm")
}

/// A simple command runs `belay bg stop` or `belay bg attach`: a background session is
/// the user's to end or take over (rule 12).
pub(super) fn runs_belay_bg_user(command: &str) -> bool {
    belay_args(command)
        .iter()
        .any(|w| w.len() >= 2 && w[0] == "bg" && (w[1] == "stop" || w[1] == "attach"))
}

/// A simple command runs `belay fleet prune --yes`: it deletes worktrees and branches
/// (rule 12). The listing without `--yes` is the agent's to run.
pub(super) fn runs_belay_fleet_prune(command: &str) -> bool {
    belay_args(command).iter().any(|w| {
        w.len() >= 2 && w[0] == "fleet" && w[1] == "prune" && w[2..].iter().any(|a| a == "--yes")
    })
}

/// A simple command runs `belay notes approve|drop`, `belay memory approve|drop`, or
/// `belay fixes forget`: the user's
/// review of what sessions share (F2, L4).
pub(super) fn runs_belay_notes_review(command: &str) -> bool {
    belay_args(command).iter().any(|w| {
        w.len() >= 2
            && (w[0] == "notes" && matches!(w[1].as_str(), "approve" | "drop")
                || w[0] == "fixes" && w[1] == "forget"
                || w[0] == "findings" && w[1] == "drop"
                || w[0] == "memory" && matches!(w[1].as_str(), "approve" | "drop")
                || w[0] == "skill" && matches!(w[1].as_str(), "approve" | "reject" | "remove"))
    })
}
