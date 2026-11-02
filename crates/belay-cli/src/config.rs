//! User config: `~/.config/belay/config.toml` overrides `Config` defaults.
//! The file is flat TOML: `key = number` lines and `#` comments. No tables.
//! A flat format needs no TOML dependency.
//!
//! ```toml
//! shape_threshold_tokens = 2000
//! max_summary_ratio = 0.7
//! loop_threshold = 3
//! tighten_after = 5
//! idle_gap_minutes = 10
//! keep_days = 30            # belay claude deletes older sessions; 0 keeps all
//! experimental_audit = false  # audit what each session changed in tests and checks
//! notify = attention         # tell the user when they are needed: off, attention, all
//! notify_command = ~/bin/ping  # also run this with each notification as JSON on stdin
//! notify_quiet = 22:00-07:00  # no notifications in this window (local time)
//! guard = deny               # test guard: deny, warn, or off
//! loops = on                 # tell the agent when a failure loops
//! shape = on                 # summarize large test output
//! wrap_tests = false         # run tests through `belay run` (costs ~1-2 s each in auto mode)
//! verify = on                # ask the agent to run its tests before it finishes
//! memory = on                # handoffs, recall at start, learned fixes, project notes
//! recall_tokens = 600        # most tokens the start recall and the carry pack take
//! awareness = on             # tell the agent what other belay sessions here did
//! commons = off              # pass other sessions' messages to the agent
//! report = quiet             # how much belay tells the user: quiet, normal, verbose
//! sandbox_memory = 4G        # `belay sandbox`: most memory its processes use together
//! sandbox_procs = 512        # most processes in a sandbox
//! sandbox_disk = 10G         # most disk its worktree and private home use
//! sandbox_wall = 4h          # longest a sandboxed command runs
//! sandbox_agent = on         # agents may run `belay sandbox run` and `task` too
//! sandbox_tasks_per_hour = 4  # most `belay sandbox task` runs an agent starts per hour
//! sandbox_task_tokens = 2M   # a sandbox task is stopped past this many tokens
//! sandbox_ro = ~/.cargo, ~/.rustup   # toolchain dirs bound read-only (default: these,
//!                            # ~/.nvm, ~/.pyenv, ~/.local/bin, when they exist)
//! resume = continue          # a resumed conversation continues belay's state; fresh starts clean
//! build_cache = shared       # fleet builds: a cargo target dir per repo and slot; separate, off
//! sandbox_build_cache = off  # sandbox builds: shared binds one cache per repo in (see docs/sandbox.md)
//! ```
//!
//! 0.1 keys still load: `look` reads as `on`; `fleet` is `awareness`, `learn` is
//! `memory`; `plugin_mode` is ignored; `notify = true|false` reads as `all|off`. One line says so, at most once a day per file;
//! `belay config migrate` (config/migrate.rs) rewrites them.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Result};
use belay_core::{GuardSetting, ReportLevel};

use crate::disk::BuildCache;
use crate::sandbox::limits;

pub mod migrate;

fn on_off_word(b: bool) -> &'static str {
    if b {
        "on"
    } else {
        "off"
    }
}

/// The whole config file: the core's knobs, and those only the launcher, the plugin
/// and the daemon read. Derefs to the core's, so `cfg.loop_threshold` works.
#[derive(Debug, Clone)]
pub struct Config {
    pub core: belay_core::Config,
    /// `belay claude` deletes sessions with no activity for this many days (0: never).
    pub keep_days: u64,
    /// `belay claude` audits what the session changed in tests and checks (experimental).
    pub experimental_audit: bool,
    /// When belay tells the user they are needed (V3): a block, a loop, a stop with
    /// tests unmet, a finished turn, a prompt that waits.
    pub notify: Notify,
    /// A command run with each notification as JSON on its stdin (`sh -c`).
    pub notify_command: Option<String>,
    /// No notifications from the first minute of the day to the second (local time;
    /// it wraps past midnight when the start is later).
    pub notify_quiet: Option<(u32, u32)>,
    /// Sandbox limits (`belay sandbox`): memory and disk in bytes, wall time in seconds.
    pub sandbox_memory: u64,
    pub sandbox_procs: u64,
    pub sandbox_disk: u64,
    pub sandbox_wall: u64,
    /// Agents may run `belay sandbox run` and `belay sandbox task`, not only the user.
    pub sandbox_agent: bool,
    /// Most `belay sandbox task` runs one agent session starts in an hour.
    pub sandbox_tasks_per_hour: u64,
    /// A sandbox task is stopped once its agent used this many tokens.
    pub sandbox_task_tokens: u64,
    /// Toolchain dirs bound read-only into a sandbox, `~/` for the home.
    pub sandbox_ro: Vec<String>,
    /// A resumed conversation (`claude --resume`, `-c`, `/resume`) continues the belay
    /// state its earlier runs left (`continue`), or starts clean (`fresh`) (U8).
    pub resume: String,
    /// Where a fleet session's cargo builds go (P0.4): `shared` (a target dir per repo
    /// and slot under `<home>/cache`), `separate` (each worktree's own), or `off`.
    pub build_cache: BuildCache,
    /// A sandbox's cargo builds: `shared` binds one cache per repo in, read-write;
    /// `off` keeps them in the sandbox's clone.
    pub sandbox_build_cache: BuildCache,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            core: belay_core::Config::default(),
            keep_days: 30,
            experimental_audit: false,
            notify: Notify::Attention,
            notify_command: None,
            notify_quiet: None,
            sandbox_memory: 4 << 30,
            sandbox_procs: 512,
            sandbox_disk: 10 << 30,
            sandbox_wall: 4 * 3600,
            sandbox_agent: true,
            sandbox_tasks_per_hour: 4,
            sandbox_task_tokens: 2_000_000,
            sandbox_ro: [
                "~/.cargo",
                "~/.rustup",
                "~/.nvm",
                "~/.pyenv",
                "~/.local/bin",
            ]
            .map(String::from)
            .to_vec(),
            resume: "continue".into(),
            build_cache: BuildCache::Shared,
            // Off: a shared cache lets one sandbox leave artifacts another runs.
            sandbox_build_cache: BuildCache::Off,
        }
    }
}

impl std::ops::Deref for Config {
    type Target = belay_core::Config;
    fn deref(&self) -> &belay_core::Config {
        &self.core
    }
}

impl Config {
    fn own_fields(&self) -> BTreeMap<String, String> {
        [
            ("experimental_audit", self.experimental_audit.to_string()),
            ("keep_days", self.keep_days.to_string()),
            ("notify", self.notify.name().to_string()),
            // Only whether it is set: a command may hold a token, and this reaches the ledger.
            (
                "notify_command",
                if self.notify_command.is_some() {
                    "set"
                } else {
                    ""
                }
                .to_string(),
            ),
            (
                "notify_quiet",
                self.notify_quiet
                    .map(|(a, b)| format!("{}-{}", fmt_minute(a), fmt_minute(b)))
                    .unwrap_or_default(),
            ),
            ("sandbox_memory", limits::fmt_size(self.sandbox_memory)),
            ("sandbox_procs", self.sandbox_procs.to_string()),
            ("sandbox_disk", limits::fmt_size(self.sandbox_disk)),
            ("sandbox_wall", limits::fmt_duration(self.sandbox_wall)),
            ("sandbox_agent", on_off_word(self.sandbox_agent).to_string()),
            (
                "sandbox_tasks_per_hour",
                self.sandbox_tasks_per_hour.to_string(),
            ),
            (
                "sandbox_task_tokens",
                limits::fmt_count(self.sandbox_task_tokens),
            ),
            ("sandbox_ro", self.sandbox_ro.join(", ")),
            ("resume", self.resume.clone()),
            ("build_cache", self.build_cache.word().to_string()),
            (
                "sandbox_build_cache",
                self.sandbox_build_cache.word().to_string(),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    /// Every key as `name → value`, in a stable order.
    pub fn fields(&self) -> BTreeMap<String, String> {
        let mut out = self.core.fields();
        out.extend(self.own_fields());
        out
    }

    /// Keys that differ from the defaults.
    pub fn overrides(&self) -> BTreeMap<String, String> {
        let defaults = Config::default().fields();
        self.fields()
            .into_iter()
            .filter(|(k, v)| defaults.get(k) != Some(v))
            .collect()
    }

    /// The controller's config: the core's knobs, and this side's overrides so the
    /// session-start record still names every key the user set.
    /// Whether a resumed conversation continues its belay state (`resume = continue`).
    pub fn resume_continues(&self) -> bool {
        self.resume == "continue"
    }

    pub fn session(&self) -> belay_core::Config {
        let defaults = Config::default().own_fields();
        let mut core = self.core.clone();
        core.other_overrides = self
            .own_fields()
            .into_iter()
            .filter(|(k, v)| defaults.get(k) != Some(v))
            .collect();
        core
    }
}

/// How much belay tells the user it needs them (`notify`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notify {
    Off,
    /// Blocks, loops, verify unmet, a finished turn, a prompt that waits.
    Attention,
    /// Those, and every warning belay has for the user.
    All,
}

impl Notify {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "off" => Self::Off,
            "attention" => Self::Attention,
            "all" => Self::All,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Attention => "attention",
            Self::All => "all",
        }
    }
}

/// `HH:MM` → minute of the day.
fn parse_minute(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60 && s.trim().len() == 5).then_some(h * 60 + m)
}

fn fmt_minute(m: u32) -> String {
    format!("{:02}:{:02}", m / 60, m % 60)
}

/// $BELAY_CONFIG, then $XDG_CONFIG_HOME/belay/config.toml, then ~/.config/belay/config.toml.
pub fn path() -> PathBuf {
    if let Some(p) = crate::paths::env_path("BELAY_CONFIG") {
        return p;
    }
    let base = crate::paths::env_path("XDG_CONFIG_HOME")
        .or_else(|| crate::paths::env_path("HOME").map(|h| h.join(".config")))
        .unwrap_or_else(env::temp_dir);
    base.join("belay/config.toml")
}

/// One line of the file the config could not use as written: a bad line (skipped), or
/// a 0.1 key or value (`old`: read, and `belay config migrate` rewrites it).
#[derive(Debug, Clone)]
pub struct Note {
    pub line: usize,
    pub text: String,
    /// The 0.1 key, or `key = look`, for the short once-a-day warning.
    pub old: Option<String>,
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.text)
    }
}

/// A parsed file: the config, which keys the file set (key → line), and its notes.
#[derive(Debug, Clone, Default)]
pub struct Parsed {
    pub config: Config,
    pub set: BTreeMap<String, usize>,
    pub notes: Vec<Note>,
}

/// The config, and one warning per line that was not used. A missing file gives the
/// defaults; a bad line is skipped and the rest still apply.
pub fn load_from(path: &Path) -> (Config, Vec<String>) {
    let (p, _) = parse_file(path);
    (p.config, p.notes.iter().map(Note::to_string).collect())
}

/// The parsed file and its text (`None`: no file). A file that cannot be read gives the
/// defaults with a note.
fn parse_file(path: &Path) -> (Parsed, Option<String>) {
    match fs::read_to_string(path) {
        Ok(text) => (parse_full(&text), Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Parsed::default(), None),
        Err(e) => {
            let mut p = Parsed::default();
            p.notes.push(Note {
                line: 0,
                text: format!("read {}: {e}; using defaults", path.display()),
                old: None,
            });
            (p, None)
        }
    }
}

static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// No config warnings from this process: `belay hook` prints nothing (rule 1), and
/// the status line shows only its line.
pub fn quiet() {
    QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Never fails. Warnings about bad lines go to stderr, each once per process: the
/// launcher's reaches the user before claude starts; the daemon's goes to `daemon.log`.
/// 0.1 keys get one short line, at most once a day per file, and only on a terminal.
pub fn load() -> Config {
    static SAID: std::sync::Mutex<std::collections::BTreeSet<String>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let p = path();
    let (parsed, text) = parse_file(&p);
    if QUIET.load(std::sync::atomic::Ordering::Relaxed) {
        return parsed.config;
    }
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    for n in parsed.notes.iter().filter(|n| n.old.is_none()) {
        if said.insert(n.to_string()) {
            eprintln!("belay: config {}: {n}", p.display());
        }
    }
    let old: Vec<&str> = parsed
        .notes
        .iter()
        .filter_map(|n| n.old.as_deref())
        .collect();
    if let (false, Some(text)) = (old.is_empty(), text) {
        use std::io::IsTerminal;
        let stamp = crate::paths::home().join("state/config-warned");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if said.insert("old keys".into())
            && std::io::stderr().is_terminal()
            && warn_due(&stamp, &p, &belay_core::text::sha256_hex(&text), now)
        {
            eprintln!("{}", old_keys_line(&p, &old));
        }
    }
    parsed.config
}

fn old_keys_line(path: &Path, old: &[&str]) -> String {
    format!(
        "belay: config {} has 0.1 keys ({}); they still work. Run `belay config migrate` to update the file.",
        path.display(),
        old.join(", ")
    )
}

const DAY_SECS: u64 = 24 * 3600;

/// Whether to warn about `cfg`'s old keys now: not if the stamp file says this same
/// content (`hash`) was warned about less than a day ago. When it says yes, it writes
/// the stamp. The stamp has one `path<TAB>hash<TAB>unix secs` line per config file.
fn warn_due(stamp: &Path, cfg: &Path, hash: &str, now: u64) -> bool {
    let key = cfg.to_string_lossy();
    let text = fs::read_to_string(stamp).unwrap_or_default();
    let mut lines: Vec<&str> = Vec::new();
    for l in text.lines() {
        let mut f = l.split('\t');
        let (p, h, t) = (
            f.next(),
            f.next(),
            f.next().and_then(|t| t.parse::<u64>().ok()),
        );
        if p == Some(&*key) {
            if h == Some(hash) && t.is_some_and(|t| now.saturating_sub(t) < DAY_SECS) {
                return false;
            }
        } else if !l.is_empty() {
            lines.push(l);
        }
    }
    let mine = format!("{key}\t{hash}\t{now}");
    lines.push(&mine);
    // A stamp that cannot be written means the warning may repeat: still fail open.
    if let Some(d) = stamp.parent() {
        let _ = fs::create_dir_all(d);
    }
    let _ = fs::write(stamp, lines.join("\n") + "\n");
    true
}

/// `belay config`: every key, its value, and where it came from.
pub fn show() -> Result<()> {
    let p = path();
    let (parsed, text) = parse_file(&p);
    print!("{}", describe(&p, &parsed, text.is_some()));
    Ok(())
}

fn describe(path: &Path, parsed: &Parsed, exists: bool) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    if exists {
        let _ = writeln!(out, "Config file: {}", path.display());
    } else {
        let _ = writeln!(out, "Config file: {} (none: all defaults)", path.display());
    }
    let fields = parsed.config.fields();
    let width = fields.keys().map(String::len).max().unwrap_or(0);
    // A long list (sandbox_ro) does not push every other row's source out.
    let vwidth = fields
        .values()
        .map(String::len)
        .filter(|n| *n <= 16)
        .max()
        .unwrap_or(0);
    for (k, v) in &fields {
        let from = match parsed.set.get(k) {
            Some(line) => format!("file, line {line}"),
            None => "default".to_string(),
        };
        let _ = writeln!(out, "{k:<width$}  {v:<vwidth$}  {from}");
    }
    if !parsed.notes.is_empty() {
        let _ = writeln!(out);
        for n in &parsed.notes {
            let _ = writeln!(out, "{n}");
        }
    }
    if parsed.notes.iter().any(|n| n.old.is_some()) {
        let _ = writeln!(out, "Run `belay config migrate` to rewrite the 0.1 keys.");
    }
    out
}

/// Every key of 0.2. The 0.1 names are in `RENAMED`.
const KEYS: [&str; 31] = [
    "shape_threshold_tokens",
    "max_summary_ratio",
    "loop_threshold",
    "tighten_after",
    "idle_gap_minutes",
    "keep_days",
    "experimental_audit",
    "notify",
    "notify_command",
    "notify_quiet",
    "guard",
    "loops",
    "shape",
    "wrap_tests",
    "verify",
    "memory",
    "recall_tokens",
    "awareness",
    "commons",
    "report",
    "sandbox_memory",
    "sandbox_procs",
    "sandbox_disk",
    "sandbox_wall",
    "sandbox_agent",
    "sandbox_tasks_per_hour",
    "sandbox_task_tokens",
    "sandbox_ro",
    "resume",
    "build_cache",
    "sandbox_build_cache",
];

/// 0.1 key names, and the 0.2 name each now has (`None`: ignored).
pub(crate) const RENAMED: [(&str, Option<&str>); 3] = [
    ("fleet", Some("awareness")),
    ("learn", Some("memory")),
    ("plugin_mode", None),
];

/// 0.1 values with a 0.2 word: key, old value, new value.
pub(crate) const OLD_VALUES: [(&str, &str, &str); 2] =
    [("notify", "true", "all"), ("notify", "false", "off")];

/// Keys that took `look` in 0.1; it reads as `on` now.
pub(crate) const LOOK_KEYS: [&str; 3] = ["awareness", "memory", "commons"];

/// The key of a `key = value` line (`None`: blank or comment), comments cut off.
pub(crate) fn line_key(raw: &str) -> Option<&str> {
    let line = raw.split('#').next().unwrap_or("").trim();
    Some(line.split_once('=').map_or(line, |(k, _)| k).trim()).filter(|_| !line.is_empty())
}

#[cfg(test)]
fn parse(text: &str) -> (Config, Vec<String>) {
    let p = parse_full(text);
    (p.config, p.notes.iter().map(Note::to_string).collect())
}

fn parse_full(text: &str) -> Parsed {
    let mut p = Parsed::default();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    // A 0.1 key yields to its new name anywhere in the file, as `config migrate` does.
    let present: std::collections::BTreeSet<&str> = text.lines().filter_map(line_key).collect();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut old = None;
        let text = match apply(&mut p.config, line, &mut p.set, &present, &mut old, n + 1) {
            Ok(notes) if notes.is_empty() => continue,
            Ok(notes) => notes.join("; "),
            Err(e) => {
                old = None;
                format!("{e}; line ignored")
            }
        };
        p.notes.push(Note {
            line: n + 1,
            text,
            old,
        });
    }
    p
}

/// Apply one `key = value` line. `Ok(notes)`: applied (or, for a 0.1 key, read or
/// skipped), with what changed since 0.1; `old` then names the 0.1 key or value.
fn apply(
    c: &mut Config,
    line: &str,
    seen: &mut BTreeMap<String, usize>,
    present: &std::collections::BTreeSet<&str>,
    old: &mut Option<String>,
    line_no: usize,
) -> Result<Vec<String>> {
    let (key, value) = line
        .split_once('=')
        .ok_or_else(|| anyhow!("expected `key = value`"))?;
    let (key, value) = (key.trim(), value.trim());
    let renamed = RENAMED.iter().find(|(k, _)| *k == key);
    if !KEYS.contains(&key) && renamed.is_none() {
        match KEYS.iter().find(|k| edit_distance(k, key) <= 2) {
            Some(k) => bail!("unknown key `{key}` (did you mean `{k}`?)"),
            None => bail!("unknown key `{key}`"),
        }
    }
    let mut notes = Vec::new();
    // 0.1 names (owner, 2026-10-31: modes are gone; features replace them).
    let key = match renamed {
        Some((old_key, None)) => {
            *old = Some(old_key.to_string());
            return Ok(vec![format!(
                "`{old_key}` is ignored: belay has no modes now (see `belay pause`)"
            )]);
        }
        Some((old_key, Some(new))) => {
            *old = Some(old_key.to_string());
            if present.contains(new) {
                return Ok(vec![format!("`{old_key}` is ignored: `{new}` is set")]);
            }
            notes.push(format!("`{old_key}` is now `{new}`"));
            *new
        }
        None => key,
    };
    if seen.contains_key(key) {
        bail!("`{key}` is set twice; the first value stays");
    }
    // Quotes are allowed on any value, as in TOML.
    let value = value.trim_matches('"');
    let value = if value == "look" && LOOK_KEYS.contains(&key) {
        notes.push(format!(
            "`{key} = look` reads as `{key} = on`: look mode is gone"
        ));
        if old.is_none() {
            *old = Some(format!("{key} = look"));
        }
        "on"
    } else if let Some((_, _, new)) = OLD_VALUES.iter().find(|(k, v, _)| *k == key && *v == value) {
        notes.push(format!("`{key} = {value}` reads as `{key} = {new}`"));
        if old.is_none() {
            *old = Some(format!("{key} = {value}"));
        }
        new
    } else {
        value
    };
    let int = |min: u64, max: u64| -> Result<u64> {
        let v: u64 = value.parse().map_err(|e| anyhow!("{key}: {e}"))?;
        if !(min..=max).contains(&v) {
            bail!("{key} must be in {min}..={max}, got {v}");
        }
        Ok(v)
    };
    let on_off = || -> Result<bool> {
        match value {
            "on" | "true" => Ok(true),
            "off" | "false" => Ok(false),
            v => bail!("{key} must be on or off, got {v}"),
        }
    };
    let size = |min: u64| -> Result<u64> {
        let v = limits::parse_size(value).map_err(|e| anyhow!("{key}: {e}"))?;
        if v < min {
            bail!(
                "{key} must be at least {}, got {value}",
                limits::fmt_size(min)
            );
        }
        Ok(v)
    };
    let boolean = || -> Result<bool> {
        match value {
            "true" => Ok(true),
            "false" => Ok(false),
            v => bail!("{key} must be true or false, got {v}"),
        }
    };
    match key {
        "shape_threshold_tokens" => c.core.shape_threshold_tokens = int(100, 10_000_000)? as usize,
        "max_summary_ratio" => {
            let v: f32 = value.parse().map_err(|e| anyhow!("{key}: {e}"))?;
            if !(v > 0.0 && v <= 1.0) {
                bail!("max_summary_ratio must be in (0, 1], got {v}");
            }
            c.core.max_summary_ratio = v;
        }
        "loop_threshold" => c.core.loop_threshold = int(1, 100)?,
        "tighten_after" => c.core.tighten_after = int(1, 1000)? as u32,
        "idle_gap_minutes" => c.core.idle_gap_minutes = int(1, 24 * 60)?,
        "keep_days" => c.keep_days = int(0, 36_500)?,
        "guard" => {
            c.core.guard = GuardSetting::parse(value)
                .ok_or_else(|| anyhow!("{key} must be deny, warn, or off, got {value}"))?
        }
        "report" => {
            c.core.report = ReportLevel::parse(value)
                .ok_or_else(|| anyhow!("{key} must be quiet, normal, or verbose, got {value}"))?
        }
        "loops" => c.core.loops = on_off()?,
        "shape" => c.core.shape = on_off()?,
        "verify" => c.core.verify = on_off()?,
        "memory" => c.core.memory = on_off()?,
        "recall_tokens" => c.core.recall_tokens = int(0, 20_000)? as usize,
        "awareness" => c.core.awareness = on_off()?,
        "commons" => c.core.commons = on_off()?,
        "wrap_tests" => c.core.wrap_tests = boolean()?,
        "notify" => {
            c.notify = Notify::parse(value)
                .ok_or_else(|| anyhow!("{key} must be off, attention, or all, got {value}"))?
        }
        "notify_command" => c.notify_command = Some(value.to_string()).filter(|v| !v.is_empty()),
        "notify_quiet" => {
            let window = value
                .split_once('-')
                .and_then(|(a, b)| Some((parse_minute(a)?, parse_minute(b)?)))
                .ok_or_else(|| anyhow!("{key} must be HH:MM-HH:MM, got {value}"))?;
            c.notify_quiet = Some(window);
        }
        "experimental_audit" => c.experimental_audit = boolean()?,
        "sandbox_memory" => c.sandbox_memory = size(16 << 20)?,
        "sandbox_disk" => c.sandbox_disk = size(16 << 20)?,
        "sandbox_procs" => c.sandbox_procs = int(8, 1_000_000)?,
        "sandbox_wall" => {
            c.sandbox_wall = limits::parse_duration(value).map_err(|e| anyhow!("{key}: {e}"))?;
            if c.sandbox_wall == 0 {
                bail!("{key} must be more than 0");
            }
        }
        "sandbox_agent" => c.sandbox_agent = on_off()?,
        "sandbox_tasks_per_hour" => c.sandbox_tasks_per_hour = int(0, 1000)?,
        "sandbox_task_tokens" => {
            c.sandbox_task_tokens =
                limits::parse_count(value).map_err(|e| anyhow!("{key}: {e}"))?;
            if c.sandbox_task_tokens < 1000 {
                bail!("{key} must be at least 1K, got {value}");
            }
        }
        "sandbox_ro" => {
            c.sandbox_ro = value
                .split(',')
                .map(|p| p.trim().trim_matches('"').to_string())
                .filter(|p| !p.is_empty())
                .collect()
        }
        "resume" => {
            if !matches!(value, "continue" | "fresh") {
                bail!("{key} must be continue or fresh, got {value}");
            }
            c.resume = value.to_string();
        }
        "build_cache" => {
            c.build_cache = BuildCache::parse(value)
                .ok_or_else(|| anyhow!("{key} must be shared, separate, or off, got {value}"))?;
        }
        "sandbox_build_cache" => {
            c.sandbox_build_cache = match BuildCache::parse(value) {
                Some(m @ (BuildCache::Shared | BuildCache::Off)) => m,
                _ => bail!("{key} must be shared or off, got {value}"),
            };
        }
        _ => unreachable!("checked against KEYS"),
    }
    seen.insert(key.to_string(), line_no);
    Ok(notes)
}

/// Levenshtein distance, for "did you mean".
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != *cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The repo policy for `dir`: the nearest `.belay.toml` in `dir` or above, up to the
/// git root (a directory with `.git`). Returns the policy (empty if there is none) and
/// warnings, each naming the file.
pub fn load_policy(dir: &Path) -> (belay_core::policy::Policy, Vec<String>) {
    let mut at = Some(dir);
    while let Some(d) = at {
        let file = d.join(".belay.toml");
        if let Ok(text) = fs::read_to_string(&file) {
            let (p, w) = belay_core::policy::parse(&text, &d.to_string_lossy());
            let w = w
                .into_iter()
                .map(|w| format!("{}: {w}", file.display()))
                .collect();
            return (p, w);
        }
        if d.join(".git").exists() {
            break;
        }
        at = d.parent();
    }
    (belay_core::policy::Policy::default(), Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_keys_parse_sizes_durations_and_lists() {
        let (c, w) = parse(
            "sandbox_memory = 512M\nsandbox_disk = 2G\nsandbox_procs = 64\nsandbox_wall = 90m\nsandbox_agent = off\nsandbox_ro = ~/.cargo, /opt/go\n",
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.sandbox_memory, 512 << 20);
        assert_eq!(c.sandbox_disk, 2 << 30);
        assert_eq!(c.sandbox_procs, 64);
        assert_eq!(c.sandbox_wall, 90 * 60);
        assert!(!c.sandbox_agent);
        assert_eq!(c.sandbox_ro, ["~/.cargo", "/opt/go"]);
        let o = c.overrides();
        assert_eq!(o["sandbox_memory"], "512M");
        assert_eq!(o["sandbox_wall"], "90m");
        assert!(!Config::default().overrides().contains_key("sandbox_wall"));
        let (_, w) = parse("sandbox_memory = 1K\nsandbox_wall = soon\n");
        assert_eq!(w.len(), 2, "{w:?}");
        let (c, w) = parse("sandbox_tasks_per_hour = 2\nsandbox_task_tokens = 500K\n");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(
            (c.sandbox_tasks_per_hour, c.sandbox_task_tokens),
            (2, 500_000)
        );
        assert_eq!(c.overrides()["sandbox_task_tokens"], "500K");
        assert_eq!(Config::default().fields()["sandbox_task_tokens"], "2M");
    }

    #[test]
    fn file_overrides_defaults() {
        let (c, w) =
            parse("# tuned\nloop_threshold = 4  # fewer false loops\n\nmax_summary_ratio = 0.5\n");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.loop_threshold, 4);
        assert_eq!(c.max_summary_ratio, 0.5);
        assert_eq!(c.tighten_after, Config::default().tighten_after);
        assert_eq!(c.overrides().len(), 2);
    }

    /// One bad line no longer throws away the good ones (U6).
    #[test]
    fn bad_lines_are_skipped_with_a_warning() {
        let (c, w) = parse(
            "loop_threshold = 4\nshape_threshold_tokens = 5000\nloop_treshold = 4\nmax_summary_ratio = 1.5\nidle_gap_minutes = 0\nloop_threshold = 9\n[section]\n",
        );
        assert_eq!(c.loop_threshold, 4);
        assert_eq!(c.shape_threshold_tokens, 5000);
        assert_eq!(c.idle_gap_minutes, Config::default().idle_gap_minutes);
        assert_eq!(w.len(), 5, "{w:?}");
        assert!(w[0].contains("did you mean `loop_threshold`"), "{w:?}");
        assert!(w[3].contains("set twice"), "{w:?}");
        assert!(parse("shape_threshold_tokens = 0").1[0].contains("100..="));
        assert!(parse("loop_threshold = three").1.len() == 1);
    }

    #[test]
    fn a_byte_order_mark_is_ignored() {
        let (c, w) = parse("\u{feff}loop_threshold = 4\n");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.loop_threshold, 4);
    }

    #[test]
    fn feature_keys() {
        let (c, w) = parse("guard = warn\nloops = off\nshape = off\nmemory = off\nawareness = off\ncommons = on\nreport = verbose\nwrap_tests = true\n");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.guard, GuardSetting::Warn);
        assert!(!c.loops && !c.shape && !c.memory && !c.awareness);
        assert!(c.commons && c.wrap_tests);
        assert_eq!(c.report, ReportLevel::Verbose);
        let (c, w) = parse("guard = yes");
        assert_eq!(c.guard, GuardSetting::Deny);
        assert!(w[0].contains("deny, warn, or off"), "{w:?}");
    }

    /// 0.1 config files keep working, and say once what changed.
    #[test]
    fn old_keys_and_look_values_load_with_a_warning() {
        let (c, w) = parse("fleet = look\n");
        assert!(c.awareness);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("`fleet` is now `awareness`"), "{w:?}");
        assert!(
            w[0].contains("`awareness = look` reads as `awareness = on`"),
            "{w:?}"
        );
        let (c, w) = parse("learn = off\ncommons = look\nplugin_mode = optimize\n");
        assert!(!c.memory && c.commons);
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(w[0].contains("`learn` is now `memory`"), "{w:?}");
        assert!(w[1].contains("`commons = look`"), "{w:?}");
        assert!(w[2].contains("`plugin_mode` is ignored"), "{w:?}");
        assert!(c.overrides().keys().all(|k| k != "plugin_mode"));
        // Old and new name for one key: the new one stays, as `config migrate` keeps it.
        let (c, w) = parse("fleet = off\nawareness = on\n");
        assert!(c.awareness);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].contains("`fleet` is ignored: `awareness` is set"),
            "{w:?}"
        );
    }

    /// A rejected line does not count as the first value, and quotes work everywhere.
    #[test]
    fn rejected_first_value_is_not_the_first() {
        let (c, w) =
            parse("loop_threshold = 0\nloop_threshold = 4\ncommons = \"on\"\nnotify = \"off\"\n");
        assert_eq!(c.loop_threshold, 4);
        assert!(c.commons);
        assert_eq!(c.notify, Notify::Off);
        assert_eq!(w.len(), 1, "{w:?}");
    }

    /// V3: `notify` takes a level, the 0.1 booleans still read, and the quiet window
    /// is two times of day.
    #[test]
    fn notify_keys() {
        assert_eq!(Config::default().notify, Notify::Attention);
        let (c, w) = parse("notify = true\n");
        assert_eq!(c.notify, Notify::All);
        assert!(
            w[0].contains("`notify = true` reads as `notify = all`"),
            "{w:?}"
        );
        assert_eq!(parse("notify = false\n").0.notify, Notify::Off);
        let (c, w) = parse(
            "notify = attention\nnotify_command = ~/bin/ping --json\nnotify_quiet = 22:30-07:05\n",
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.notify_command.as_deref(), Some("~/bin/ping --json"));
        assert_eq!(c.notify_quiet, Some((22 * 60 + 30, 7 * 60 + 5)));
        assert_eq!(c.fields()["notify_quiet"], "22:30-07:05");
        assert_eq!(
            c.fields()["notify_command"],
            "set",
            "never the command itself"
        );
        for bad in [
            "notify = yes",
            "notify_quiet = 22-7",
            "notify_quiet = 24:00-01:00",
        ] {
            let (c, w) = parse(&format!("{bad}\n"));
            assert_eq!(w.len(), 1, "{bad}: {w:?}");
            assert_eq!(c.notify_quiet, None);
        }
    }

    /// Keys the core does not read are still recorded at session start (U1b split).
    #[test]
    fn launcher_keys_reach_the_session_record() {
        let (c, w) = parse("keep_days = 7\nloop_threshold = 4\nnotify = all\n");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.keep_days, 7);
        let want: BTreeMap<String, String> = [
            ("keep_days", "7"),
            ("loop_threshold", "4"),
            ("notify", "all"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(c.overrides(), want);
        assert_eq!(c.session().overrides(), want);
        assert_eq!(c.session().loop_threshold, 4);
    }

    /// The 0.1-key warning shows once a day per file, and again when the file changes.
    #[test]
    fn old_key_warning_once_a_day() {
        let d = std::env::temp_dir().join(format!("belay-cfg-warned-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let stamp = d.join("state/config-warned");
        let (a, b) = (Path::new("/a/config.toml"), Path::new("/b/config.toml"));
        assert!(warn_due(&stamp, a, "h1", 1000));
        assert!(!warn_due(&stamp, a, "h1", 1000 + DAY_SECS - 1));
        // Another file has its own stamp; the first one's stays.
        assert!(warn_due(&stamp, b, "h1", 1000 + 10));
        assert!(!warn_due(&stamp, a, "h1", 1000 + 20));
        // A changed file warns again, and so does the next day.
        assert!(warn_due(&stamp, a, "h2", 1000 + 30));
        assert!(!warn_due(&stamp, a, "h2", 1000 + 40));
        assert!(warn_due(&stamp, a, "h2", 1000 + 30 + DAY_SECS));
        assert_eq!(fs::read_to_string(&stamp).unwrap().lines().count(), 2);
        let line = old_keys_line(a, &["fleet", "commons = look"]);
        assert!(line.contains("(fleet, commons = look)"), "{line}");
        assert!(line.contains("Run `belay config migrate`"), "{line}");
        let _ = fs::remove_dir_all(&d);
    }

    /// `belay config`: every key, its value, and where it came from.
    #[test]
    fn describe_lists_every_key_and_its_source() {
        let p = parse_full("# mine\nguard = warn\nlearn = off\nloop_treshold = 4\n");
        let out = describe(Path::new("/c.toml"), &p, true);
        assert!(out.starts_with("Config file: /c.toml\n"), "{out}");
        for k in KEYS {
            assert!(
                out.lines().any(|l| l.starts_with(&format!("{k} "))),
                "{k}: {out}"
            );
        }
        let line = |k: &str| {
            out.lines()
                .find(|l| l.starts_with(&format!("{k} ")))
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(line("guard"), "guard warn file, line 2");
        assert_eq!(line("memory"), "memory off file, line 3");
        assert_eq!(line("loop_threshold"), "loop_threshold 3 default");
        assert!(out.contains("line 4: unknown key `loop_treshold`"), "{out}");
        assert!(out.contains("Run `belay config migrate`"), "{out}");
        let none = describe(Path::new("/c.toml"), &Parsed::default(), false);
        assert!(
            none.contains("(none: all defaults)") && !none.contains("migrate"),
            "{none}"
        );
    }

    /// docs/guide.md § Config names every key.
    #[test]
    fn guide_lists_every_key() {
        let guide = include_str!("../../../docs/guide.md");
        let section = guide.split("\n## Config\n").nth(1).unwrap();
        let section = section.split("\n## ").next().unwrap();
        for k in KEYS {
            assert!(
                section.lines().any(|l| l.starts_with(&format!("{k} ="))),
                "{k} is missing from docs/guide.md § Config"
            );
        }
    }

    #[test]
    fn missing_file_is_defaults() {
        let (c, w) = load_from(Path::new("/nonexistent/belay/config.toml"));
        assert!(c.overrides().is_empty() && w.is_empty());
    }
}
