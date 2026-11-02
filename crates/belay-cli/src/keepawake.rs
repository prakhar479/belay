//! Keep the machine awake while a session works (BG2), and say honestly what that holds.
//!
//! belay never runs sudo and never changes a system setting: when a setting stands in the
//! way (a lid that suspends), the assessment names the fix for the user to run.
//!
//! `assess` and `hold` do the I/O. The parsers and the `assess_*` functions are pure, so
//! each platform is tested with injected command outputs.

use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use std::{env, io, path::Path, thread};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::doctor::find_on_path;

#[cfg(test)]
mod tests;

/// Whether belay can keep one kind of sleep off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Guarantee {
    Held,
    NotHeld,
    #[default]
    Unknown,
}

impl Guarantee {
    fn words(self) -> &'static str {
        match self {
            Guarantee::Held => "held off",
            Guarantee::NotHeld => "not held off",
            Guarantee::Unknown => "unknown",
        }
    }
}

/// What belay can guarantee on this machine.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Assessment {
    /// How belay holds sleep off here (`none` when it cannot).
    pub mechanism: String,
    /// Sleep after the machine sits idle.
    pub idle_sleep: Guarantee,
    /// Sleep when a laptop's lid closes.
    pub lid_closed: Guarantee,
    /// One or two sentences: why the guarantees are what they are.
    pub note: String,
    /// What the user can run to hold more. belay never runs it.
    pub fix: Option<String>,
}

impl Assessment {
    pub fn all_held(&self) -> bool {
        self.idle_sleep == Guarantee::Held && self.lid_closed == Guarantee::Held
    }

    /// One line: `Idle sleep held off, closed lid not held off (caffeinate). <note>`.
    pub fn summary(&self) -> String {
        format!(
            "Idle sleep {}, closed lid {} ({}). {}",
            self.idle_sleep.words(),
            self.lid_closed.words(),
            self.mechanism,
            self.note
        )
    }
}

fn none(note: &str) -> Assessment {
    Assessment {
        mechanism: "none".into(),
        idle_sleep: Guarantee::NotHeld,
        lid_closed: Guarantee::NotHeld,
        note: note.into(),
        fix: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Linux,
    Mac,
    Wsl,
    Other,
}

pub fn platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::Mac
    } else if cfg!(target_os = "linux") {
        if crate::sandbox::probe::wsl() || env::var_os("WSL_DISTRO_NAME").is_some() {
            Platform::Wsl
        } else {
            Platform::Linux
        }
    } else {
        Platform::Other
    }
}

// ---- Linux: logind inhibitors.

const WHAT_ALL: &str = "sleep:idle:handle-lid-switch";
const WHAT_IDLE: &str = "sleep:idle";

/// How `systemd-inhibit` answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inhibit {
    Accepted,
    /// logind or polkit said no; the first line of its message.
    Refused(String),
    /// No logind to ask (no system bus, or not booted with systemd).
    NoLogind,
    /// No `systemd-inhibit` on PATH.
    Missing,
}

/// Classify a finished `systemd-inhibit` run.
pub fn inhibit_outcome(success: bool, stderr: &str) -> Inhibit {
    if success {
        return Inhibit::Accepted;
    }
    let s = stderr.trim();
    if s.contains("Failed to connect to bus")
        || s.contains("not been booted with systemd")
        || s.contains("org.freedesktop.login1 was not provided")
    {
        return Inhibit::NoLogind;
    }
    let first = s.lines().next().unwrap_or("").trim();
    Inhibit::Refused(if first.is_empty() {
        "no reason given".into()
    } else {
        first.trim_end_matches('.').into()
    })
}

const LOGIND_LID_FIX: &str = "set `HandleLidSwitch=ignore` and `HandleLidSwitchExternalPower=ignore` in /etc/systemd/logind.conf, then `sudo systemctl kill -s HUP systemd-logind` (remove the lines to undo); or let your user take the `org.freedesktop.login1.inhibit-handle-lid-switch` polkit action.";

/// `full` is the probe with the lid switch; `idle` without it (only asked when `full` was
/// refused).
pub fn assess_linux(full: &Inhibit, idle: &Inhibit) -> Assessment {
    match (full, idle) {
        (Inhibit::Missing, _) => {
            none("systemd-inhibit is not installed, so belay has no way to hold sleep off here.")
        }
        (Inhibit::NoLogind, _) => {
            none("logind is not running here, so systemd-inhibit cannot hold sleep off.")
        }
        (Inhibit::Accepted, _) => Assessment {
            mechanism: "systemd-inhibit".into(),
            idle_sleep: Guarantee::Held,
            lid_closed: Guarantee::Held,
            note: "logind accepts belay's inhibitor for sleep, idle, and the lid switch.".into(),
            fix: None,
        },
        (Inhibit::Refused(why), Inhibit::Accepted) => Assessment {
            mechanism: "systemd-inhibit".into(),
            idle_sleep: Guarantee::Held,
            lid_closed: Guarantee::NotHeld,
            note: format!(
                "logind refused the lid-switch inhibitor ({why}), so closing the lid may suspend."
            ),
            fix: Some(LOGIND_LID_FIX.into()),
        },
        (Inhibit::Refused(why), _) => none(&format!(
            "logind refused belay's sleep inhibitor ({why}), so nothing is held off."
        )),
    }
}

// ---- macOS: caffeinate.

/// From `pmset -g batt`: Some(true) on battery, Some(false) on AC, None when unclear.
pub fn on_battery(pmset_batt: &str) -> Option<bool> {
    if pmset_batt.contains("'Battery Power'") {
        Some(true)
    } else if pmset_batt.contains("'AC Power'") {
        Some(false)
    } else {
        None
    }
}

// [Verify] macOS (not yet run on a Mac): `caffeinate -ims -w <pid>` holds idle sleep off,
// and a closed lid sleeps on battery whatever caffeinate holds.
pub fn assess_mac(caffeinate: bool, battery: Option<bool>) -> Assessment {
    if !caffeinate {
        return none("caffeinate is not on PATH, so belay has no way to hold sleep off.");
    }
    let (lid_closed, note, fix) = match battery {
        Some(true) => (
            Guarantee::NotHeld,
            "On battery, a Mac sleeps when its lid closes, whatever caffeinate holds.",
            "`sudo pmset -b disablesleep 1` (and `sudo pmset -b disablesleep 0` to undo).",
        ),
        Some(false) => (
            Guarantee::Unknown,
            "On AC power, a closed lid keeps a Mac awake only with an external display attached.",
            "attach an external display, or `sudo pmset -a disablesleep 1` (and `sudo pmset -a disablesleep 0` to undo).",
        ),
        None => (
            Guarantee::Unknown,
            "pmset did not say whether this Mac runs on battery or AC, so a closed lid may sleep.",
            "`sudo pmset -a disablesleep 1` (and `sudo pmset -a disablesleep 0` to undo).",
        ),
    };
    Assessment {
        mechanism: "caffeinate".into(),
        idle_sleep: Guarantee::Held,
        lid_closed,
        note: note.into(),
        fix: Some(fix.into()),
    }
}

// ---- WSL: Windows' power manager through powershell.exe.

/// The lid action's AC and DC indices from `powercfg /qh SCHEME_CURRENT SUB_BUTTONS
/// LIDACTION` (0 do nothing, 1 sleep, 2 hibernate, 3 shut down).
pub fn lid_action(powercfg: &str) -> Option<(u32, u32)> {
    let after = &powercfg[powercfg.find("LIDACTION")?..];
    let index = |key: &str| -> Option<u32> {
        let line = after.lines().find(|l| l.contains(key))?;
        let hex = line.rsplit(':').next()?.trim();
        u32::from_str_radix(hex.trim_start_matches("0x"), 16).ok()
    };
    Some((
        index("Current AC Power Setting Index")?,
        index("Current DC Power Setting Index")?,
    ))
}

fn lid_word(i: u32) -> &'static str {
    match i {
        0 => "do nothing",
        1 => "sleep",
        2 => "hibernate",
        3 => "shut down",
        _ => "an unknown action",
    }
}

const WSL_MECHANISM: &str = "SetThreadExecutionState via powershell.exe";

const WSL_LID_FIX: &str = "in Windows, `powercfg /setacvalueindex SCHEME_CURRENT SUB_BUTTONS LIDACTION 0`, `powercfg /setdcvalueindex SCHEME_CURRENT SUB_BUTTONS LIDACTION 0`, then `powercfg /setactive SCHEME_CURRENT` (set 1 to put sleep back).";

pub fn assess_wsl(powershell: bool, lid: Option<(u32, u32)>) -> Assessment {
    if !powershell {
        return none(
            "WSL: powershell.exe is not on PATH (interop off?), so belay cannot reach Windows' power manager.",
        );
    }
    let (lid_closed, note, fix) = match lid {
        Some((0, 0)) => (
            Guarantee::Held,
            "WSL: Windows does nothing when the lid closes.".to_string(),
            None,
        ),
        Some((ac, dc)) => (
            Guarantee::NotHeld,
            format!(
                "WSL: Windows' lid action is {} on AC power and {} on battery.",
                lid_word(ac),
                lid_word(dc)
            ),
            Some(WSL_LID_FIX.to_string()),
        ),
        None => (
            Guarantee::Unknown,
            "WSL: powercfg did not report the lid action, so a closed lid may sleep.".to_string(),
            Some(WSL_LID_FIX.to_string()),
        ),
    };
    Assessment {
        mechanism: WSL_MECHANISM.into(),
        idle_sleep: Guarantee::Held,
        lid_closed,
        note,
        fix,
    }
}

/// Asks Windows to stay awake until its stdin closes. C# needs double quotes, which do not
/// survive the trip to a Windows command line, hence `[char]34`. Exit 3 when Windows refuses.
const WSL_HOLDER: &str = "$q=[char]34; $s=Add-Type -Name P -Namespace BelayKeepAwake -PassThru -MemberDefinition ('[DllImport(' + $q + 'kernel32.dll' + $q + ')] public static extern uint SetThreadExecutionState(uint f);'); if ($s::SetThreadExecutionState([uint32]2147483649) -eq 0) { exit 3 }; [void][Console]::In.ReadToEnd()";

// ---- I/O.

/// What belay can guarantee here. Runs a quick probe (at most a few seconds); changes
/// nothing.
pub fn assess() -> Assessment {
    match platform() {
        Platform::Linux => {
            if find_on_path("systemd-inhibit").is_none() {
                return assess_linux(&Inhibit::Missing, &Inhibit::Missing);
            }
            let full = probe_inhibit(WHAT_ALL);
            let idle = match full {
                Inhibit::Refused(_) => probe_inhibit(WHAT_IDLE),
                _ => Inhibit::Missing,
            };
            assess_linux(&full, &idle)
        }
        Platform::Mac => {
            let battery = output_within(Command::new("pmset").args(["-g", "batt"]), SHORT)
                .ok()
                .and_then(|o| on_battery(&String::from_utf8_lossy(&o.stdout)));
            assess_mac(find_on_path("caffeinate").is_some(), battery)
        }
        Platform::Wsl => {
            let powershell = find_on_path("powershell.exe").is_some();
            assess_wsl(powershell, powershell.then(read_lid_action).flatten())
        }
        Platform::Other => none("belay knows no way to hold sleep off on this OS."),
    }
}

const SHORT: Duration = Duration::from_secs(5);

fn probe_inhibit(what: &str) -> Inhibit {
    match output_within(
        Command::new("systemd-inhibit")
            .args(inhibit_args(what, "belay check"))
            .arg("true"),
        SHORT,
    ) {
        Ok(o) => inhibit_outcome(o.status.success(), &String::from_utf8_lossy(&o.stderr)),
        Err(e) => Inhibit::Refused(e.to_string()),
    }
}

fn inhibit_args(what: &str, why: &str) -> Vec<String> {
    vec![
        format!("--what={what}"),
        "--who=belay".into(),
        format!("--why={why}"),
        // No `--no-ask-password`: systemd 255 rejects it. With stdin not a terminal,
        // systemd opens no polkit prompt anyway.
        "--mode=block".into(),
    ]
}

// [Verify] WSL: on this machine `powercfg /q` leaves LIDACTION out (a hidden setting) and
// `/qh` shows it; `/q` stays as the fallback.
fn read_lid_action() -> Option<(u32, u32)> {
    ["/qh", "/q"].iter().find_map(|flag| {
        let o = output_within(
            Command::new("powercfg.exe").args([flag, "SCHEME_CURRENT", "SUB_BUTTONS", "LIDACTION"]),
            SHORT,
        )
        .ok()?;
        lid_action(&String::from_utf8_lossy(&o.stdout))
    })
}

/// Holds sleep off while it lives. Dropping it stops the holder.
pub struct Guard {
    children: Vec<Child>,
    pub mechanism: &'static str,
}

impl Guard {
    /// Whether the holder still runs. It ends by itself once the held pid exits.
    pub fn alive(&mut self) -> bool {
        self.children
            .iter_mut()
            .all(|c| matches!(c.try_wait(), Ok(None)))
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        for c in self.children.iter_mut().rev() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Keep the machine awake while `pid` lives, or until the Guard drops. It starts a child
/// process and returns at once (on Linux after at most a short wait for logind's answer),
/// so it never holds up a hook.
pub fn hold(reason: &str, pid: u32) -> Result<Guard> {
    match platform() {
        Platform::Linux => hold_linux(reason, pid),
        Platform::Mac => {
            let child = Command::new("caffeinate")
                .args(["-ims", "-w", &pid.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("start caffeinate")?;
            Ok(Guard {
                children: vec![child],
                mechanism: "caffeinate",
            })
        }
        Platform::Wsl => hold_wsl(pid),
        Platform::Other => bail!("belay knows no way to hold sleep off on this OS"),
    }
}

/// How long logind gets to refuse before the inhibitor counts as taken.
const ANSWER: Duration = Duration::from_millis(400);

fn hold_linux(reason: &str, pid: u32) -> Result<Guard> {
    let mut last = String::new();
    // When polkit refuses the lid switch, idle and sleep alone are still worth holding.
    for what in [WHAT_ALL, WHAT_IDLE] {
        let mut child = Command::new("systemd-inhibit")
            .args(inhibit_args(what, reason))
            .args(["tail", &format!("--pid={pid}"), "-f", "/dev/null"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("start systemd-inhibit")?;
        let start = Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break Some(s);
            }
            if start.elapsed() > ANSWER {
                break None;
            }
            thread::sleep(Duration::from_millis(20));
        };
        // Still running, or done because the pid already ended: taken.
        if status.is_none_or(|s| s.success()) {
            return Ok(Guard {
                children: vec![child],
                mechanism: "systemd-inhibit",
            });
        }
        let out = child.wait_with_output()?;
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        match inhibit_outcome(false, &err) {
            Inhibit::Refused(why) => last = why,
            _ => bail!("logind is not running here: {}", err.trim()),
        }
    }
    bail!("logind refused the sleep inhibitor: {last}")
}

// [Verify] WSL (observed on this machine): killing the Linux side of powershell.exe ends
// the Windows process too, and closing its stdin ends the script. Both are kept: `tail`
// closes the pipe when `pid` ends, and the Guard kills both on drop.
fn hold_wsl(pid: u32) -> Result<Guard> {
    let mut tail = Command::new("tail")
        .args([&format!("--pid={pid}"), "-f", "/dev/null"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("start tail")?;
    let pipe = tail.stdout.take().context("tail's stdout")?;
    let ps = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", WSL_HOLDER])
        .stdin(pipe)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let ps = match ps {
        Ok(ps) => ps,
        Err(e) => {
            let _ = tail.kill();
            let _ = tail.wait();
            return Err(e).context("start powershell.exe");
        }
    };
    Ok(Guard {
        children: vec![tail, ps],
        mechanism: WSL_MECHANISM,
    })
}

/// Run a command and collect its output, killing it past `limit`. Its output must be
/// small (a pipe's buffer), as it is read only at the end.
pub fn output_within(cmd: &mut Command, limit: Duration) -> io::Result<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let start = Instant::now();
    while child.try_wait()?.is_none() {
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no answer in {} s", limit.as_secs()),
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output()
}

// ---- `belay keep-awake`.

/// `90`, `90s`, `45m`, `2h`, `1h30m`.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if let Ok(n) = s.parse::<u64>() {
        return Ok(Duration::from_secs(n));
    }
    let mut total = 0u64;
    let mut num = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
            continue;
        }
        let unit = match ch {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => bail!("bad duration `{s}`: use 90s, 45m, 2h, or 1h30m"),
        };
        let n: u64 = num
            .parse()
            .with_context(|| format!("bad duration `{s}`: a number goes before `{ch}`"))?;
        total += n * unit;
        num.clear();
    }
    if !num.is_empty() || total == 0 {
        bail!("bad duration `{s}`: use 90s, 45m, 2h, or 1h30m");
    }
    Ok(Duration::from_secs(total))
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        // SAFETY: signal 0 only checks that the process exists.
        || unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
}

/// `belay keep-awake`: the user's command. It needs no session and records nothing.
pub fn run(for_: Option<&str>, pid: Option<u32>, cmd: &[String]) -> Result<i32> {
    let limit = for_.map(parse_duration).transpose()?;
    if let Some(p) = pid.filter(|&p| !pid_alive(p)) {
        bail!("no process {p}");
    }
    let a = assess();
    eprintln!("belay keep-awake: {}", a.summary());
    if let Some(fix) = a.fix.as_ref().filter(|_| !a.all_held()) {
        eprintln!("To hold more: {fix}");
    }

    if let Some((prog, args)) = cmd.split_first() {
        let mut child = Command::new(prog)
            .args(args)
            .spawn()
            .with_context(|| format!("start `{prog}`"))?;
        let _guard = hold_or_say(&format!("belay keep-awake: {}", cmd.join(" ")), child.id());
        let status = child.wait()?;
        return Ok(status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt;
            128 + status.signal().unwrap_or(0)
        }));
    }

    let target = pid.unwrap_or_else(std::process::id);
    let Some(mut guard) = hold_or_say("belay keep-awake", target) else {
        return Ok(1);
    };
    match (limit, pid) {
        (Some(d), _) => eprintln!("Holding for {} s; Ctrl-C stops.", d.as_secs()),
        (None, Some(p)) => eprintln!("Holding while process {p} runs; Ctrl-C stops."),
        (None, None) => eprintln!("Holding until Ctrl-C."),
    }
    let start = Instant::now();
    loop {
        if limit.is_some_and(|d| start.elapsed() >= d) || pid.is_some_and(|p| !pid_alive(p)) {
            return Ok(0);
        }
        if !guard.alive() {
            eprintln!(
                "belay keep-awake: the holder ({}) stopped.",
                guard.mechanism
            );
            return Ok(1);
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn hold_or_say(reason: &str, pid: u32) -> Option<Guard> {
    match hold(reason, pid) {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("belay keep-awake: cannot hold sleep off: {e:#}.");
            None
        }
    }
}
