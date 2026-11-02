//! The hub as a supervisor that outlives its terminal (BG0): a detached start, and the
//! reset time a usage-limit message gives.

use std::fs::OpenOptions;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

/// Start `belay fleet hub` for `dir`, detached from the caller: a double fork and
/// `setsid`, so it leads no session and has no controlling terminal. The caller's
/// shell exiting, a SIGHUP to its group, or its terminal closing do not reach it.
/// stdin is /dev/null, stdout and stderr go to hub.log; cwd and umask are kept.
pub(super) fn start_hub(dir: &Path, resume: bool) -> Result<()> {
    let exe = std::env::current_exe().context("find belay executable")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("hub.log"))?;
    let mut cmd = Command::new(exe);
    cmd.args(["fleet", "hub"]);
    if resume {
        cmd.arg("--resume");
    }
    cmd.arg("--dir")
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: between fork and exec the closure calls only setsid, fork and _exit, which
    // are async-signal-safe, and allocates nothing. The first child leaves at once; its
    // child (not a session leader, so it can never take a terminal) goes on to exec.
    // spawn still sees an exec failure: the grandchild holds the error pipe until exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            match libc::fork() {
                -1 => Err(std::io::Error::last_os_error()),
                0 => Ok(()),
                _ => libc::_exit(0),
            }
        });
    }
    let mut first = cmd.spawn().context("start the fleet hub")?;
    // Reap the first child; the hub itself is init's (or a subreaper's) now.
    let _ = first.wait();
    Ok(())
}

const DAY_MS: u64 = 86_400_000;
/// A reset time of day this far in the past is today's (the message was a little late),
/// not tomorrow's.
const LATE_MS: u64 = 10 * 60_000;

/// When a usage-limit message says the limit resets, as Unix ms, read at `now`.
/// Forms: "resets 2:30am (UTC)", "resets 11pm (UTC)", "resets 14:00 UTC",
/// "resets Nov 3, 5pm (UTC)", and the older "…limit reached|<unix seconds>". None for
/// anything else, a zone other than UTC (belay has no zone data), or no time at all:
/// the user resumes then.
pub(super) fn reset_at(text: &str, now: u64) -> Option<u64> {
    let lower = text.to_lowercase();
    if let Some((_, secs)) = lower.split_once("limit reached|") {
        let digits: String = secs.chars().take_while(char::is_ascii_digit).collect();
        return digits.parse::<u64>().ok().map(|s| s * 1000);
    }
    let rest = lower.split("resets").nth(1)?;
    let mut words = rest
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty())
        .peekable();
    if words.peek().is_some_and(|w| *w == "at" || *w == "on") {
        words.next();
    }
    let date = match words.peek().and_then(|w| month(w)) {
        Some(m) => {
            words.next();
            let day: String = words
                .next()?
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if words.peek() == Some(&"at") {
                words.next();
            }
            Some((m, day.parse::<u32>().ok().filter(|d| (1..=31).contains(d))?))
        }
        None => None,
    };
    let mut time = words.next()?.to_string();
    if words.peek().is_some_and(|w| *w == "am" || *w == "pm") {
        time.push_str(words.next()?);
    }
    let zone = words
        .next()?
        .trim_matches(|c| c == '(' || c == ')' || c == '.');
    if !matches!(zone, "utc" | "gmt" | "etc/utc" | "z") {
        return None;
    }
    let of_day = time_of_day(&time)?;
    Some(match date {
        None => {
            let today = now / DAY_MS * DAY_MS + of_day;
            if today + LATE_MS < now {
                today + DAY_MS
            } else {
                today
            }
        }
        Some((m, d)) => {
            let (year, _, _) = civil(now / DAY_MS);
            let at = |y: i64| days(y, m, d).map(|n| n * DAY_MS + of_day);
            let this = at(year)?;
            if this + DAY_MS < now {
                at(year + 1)?
            } else {
                this
            }
        }
    })
}

fn month(w: &str) -> Option<u32> {
    const NAMES: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let w = w.trim_end_matches('.');
    if w.len() < 3 {
        return None;
    }
    NAMES
        .iter()
        .position(|n| w.starts_with(n))
        .map(|i| i as u32 + 1)
}

/// "2:30am", "11pm", "14:00" → ms after midnight.
fn time_of_day(t: &str) -> Option<u64> {
    let (clock, half) = match t.strip_suffix("am").or_else(|| t.strip_suffix("a.m.")) {
        Some(c) => (c, Some(false)),
        None => match t.strip_suffix("pm").or_else(|| t.strip_suffix("p.m.")) {
            Some(c) => (c, Some(true)),
            None => (t, None),
        },
    };
    let (h, m) = match clock.split_once(':') {
        Some((h, m)) => (h.parse::<u64>().ok()?, m.parse::<u64>().ok()?),
        // A bare hour needs am or pm: "resets 3" says nothing.
        None if half.is_some() => (clock.parse::<u64>().ok()?, 0),
        None => return None,
    };
    if m > 59 {
        return None;
    }
    let h = match half {
        Some(pm) if (1..=12).contains(&h) => h % 12 + if pm { 12 } else { 0 },
        Some(_) => return None,
        None if h < 24 => h,
        None => return None,
    };
    Some((h * 60 + m) * 60_000)
}

/// Days since 1970-01-01 → (year, month, day) (Howard Hinnant's civil_from_days).
fn civil(z: u64) -> (i64, u32, u32) {
    let z = z as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// (year, month, day) → days since 1970-01-01; None before it.
fn days(y: i64, m: u32, d: u32) -> Option<u64> {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    u64::try_from(era * 146_097 + doe - 719_468).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-02 01:00 UTC.
    const NOW: u64 = 1_793_581_200_000;

    #[test]
    fn the_calendar_round_trips() {
        assert_eq!(civil(NOW / DAY_MS), (2026, 11, 2));
        assert_eq!(days(2026, 11, 2), Some(NOW / DAY_MS));
        assert_eq!(days(1970, 1, 1), Some(0));
        assert_eq!(civil(days(2028, 2, 29).unwrap()), (2028, 2, 29));
    }

    #[test]
    fn reset_times_are_read_from_the_limit_messages() {
        let midnight = NOW / DAY_MS * DAY_MS;
        let h = |h: u64, m: u64| (h * 60 + m) * 60_000;
        let fixture = include_str!("../../../../fixtures/fleet/result_five_hour_limit.json");
        let v: serde_json::Value = serde_json::from_str(fixture).unwrap();
        let text = v["result"].as_str().unwrap();
        assert_eq!(reset_at(text, NOW), Some(midnight + h(2, 30)));
        let fixture = include_str!("../../../../fixtures/fleet/result_limit_resets_11pm.json");
        let v: serde_json::Value = serde_json::from_str(fixture).unwrap();
        let text = v["result"].as_str().unwrap();
        assert_eq!(reset_at(text, NOW), Some(midnight + h(23, 0)));
        // Past today: tomorrow's. A few minutes past: today's (the message was late).
        assert_eq!(
            reset_at("resets 12:30am (UTC)", NOW),
            Some(midnight + DAY_MS + h(0, 30))
        );
        assert_eq!(
            reset_at("resets 12:55am (UTC)", NOW),
            Some(midnight + h(0, 55))
        );
        assert_eq!(
            reset_at("resets 12pm (UTC)", NOW),
            Some(midnight + h(12, 0))
        );
        assert_eq!(
            reset_at("resets at 14:05 UTC", NOW),
            Some(midnight + h(14, 5))
        );
        assert_eq!(
            reset_at(
                "You've hit your weekly limit · resets Nov 5, 5pm (UTC)",
                NOW
            ),
            Some(days(2026, 11, 5).unwrap() * DAY_MS + h(17, 0))
        );
        assert_eq!(
            reset_at("resets Jan 2 at 9am (UTC)", NOW),
            Some(days(2027, 1, 2).unwrap() * DAY_MS + h(9, 0))
        );
        assert_eq!(
            reset_at("Claude AI usage limit reached|1792630000", NOW),
            Some(1_792_630_000_000)
        );
        // Unreadable, or another zone: no time, and the user resumes.
        for text in [
            "You've hit your session limit · resets 4pm",
            "resets 2:30am (Europe/Berlin)",
            "You've hit your weekly limit · resets Mon",
            "resets 13pm (UTC)",
            "resets 3 (UTC)",
            "resets 2:75am (UTC)",
            "usage limit reached",
        ] {
            assert_eq!(reset_at(text, NOW), None, "{text}");
        }
    }
}
