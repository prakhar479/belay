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
//! notify = false             # desktop notification on a loop or a blocked edit
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
//! ```
//!
//! 0.1 keys still load, with a warning: `look` reads as `on`; `fleet` is `awareness`,
//! `learn` is `memory`; `plugin_mode` is ignored.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Result};
use belay_core::{GuardSetting, ReportLevel};

use crate::sandbox::limits;

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
    /// A desktop notification when belay steers a loop or blocks an edit (unattended runs).
    pub notify: bool,
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            core: belay_core::Config::default(),
            keep_days: 30,
            experimental_audit: false,
            notify: false,
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
            ("notify", self.notify.to_string()),
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

/// The config, and one warning per line that was not used. A missing file gives the
/// defaults; a bad line is skipped and the rest still apply.
pub fn load_from(path: &Path) -> (Config, Vec<String>) {
    match fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), Vec::new()),
        Err(e) => (
            Config::default(),
            vec![format!("read {}: {e}; using defaults", path.display())],
        ),
    }
}

/// Never fails. Warnings go to stderr, each once per process: the launcher's reaches
/// the user before claude starts; the daemon's goes to `daemon.log`.
pub fn load() -> Config {
    static SAID: std::sync::Mutex<std::collections::BTreeSet<String>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let p = path();
    let (c, warnings) = load_from(&p);
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    for w in warnings {
        if said.insert(w.clone()) {
            eprintln!("belay: config {}: {w}", p.display());
        }
    }
    c
}

const KEYS: [&str; 30] = [
    "shape_threshold_tokens",
    "max_summary_ratio",
    "loop_threshold",
    "tighten_after",
    "idle_gap_minutes",
    "keep_days",
    "experimental_audit",
    "notify",
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
    // 0.1 keys: read with a warning.
    "plugin_mode",
    "fleet",
    "learn",
];

fn parse(text: &str) -> (Config, Vec<String>) {
    let mut c = Config::default();
    let mut warnings = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        match apply(&mut c, line, &mut seen) {
            Ok(Some(note)) => warnings.push(format!("line {}: {note}", n + 1)),
            Ok(None) => {}
            Err(e) => warnings.push(format!("line {}: {e}; line ignored", n + 1)),
        }
    }
    (c, warnings)
}

/// Apply one `key = value` line. `Ok(Some(note))`: applied, with a warning for the user
/// (a 0.1 key or value).
fn apply(
    c: &mut Config,
    line: &str,
    seen: &mut std::collections::BTreeSet<String>,
) -> Result<Option<String>> {
    let (key, value) = line
        .split_once('=')
        .ok_or_else(|| anyhow!("expected `key = value`"))?;
    let (key, value) = (key.trim(), value.trim());
    if !KEYS.contains(&key) {
        match KEYS.iter().find(|k| edit_distance(k, key) <= 2) {
            Some(k) => bail!("unknown key `{key}` (did you mean `{k}`?)"),
            None => bail!("unknown key `{key}`"),
        }
    }
    let mut notes = Vec::new();
    // 0.1 names (owner, 2026-10-31: modes are gone; features replace them).
    let key = match key {
        "plugin_mode" => {
            return Ok(Some(
                "`plugin_mode` is ignored: belay has no modes now (see `belay pause`)".into(),
            ))
        }
        "fleet" => {
            notes.push("`fleet` is now `awareness`".to_string());
            "awareness"
        }
        "learn" => {
            notes.push("`learn` is now `memory`".to_string());
            "memory"
        }
        k => k,
    };
    if seen.contains(key) {
        bail!("`{key}` is set twice; the first value stays");
    }
    // Quotes are allowed on any value, as in TOML.
    let value = value.trim_matches('"');
    let value = if value == "look" && matches!(key, "awareness" | "memory" | "commons") {
        notes.push(format!(
            "`{key} = look` reads as `{key} = on`: look mode is gone"
        ));
        "on"
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
        "notify" => c.notify = boolean()?,
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
        _ => unreachable!("checked against KEYS"),
    }
    seen.insert(key.to_string());
    Ok((!notes.is_empty()).then(|| notes.join("; ")))
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
        // Old and new name for one key: the first stays.
        let (c, w) = parse("fleet = off\nawareness = on\n");
        assert!(!c.awareness);
        assert!(w[1].contains("set twice"), "{w:?}");
    }

    /// A rejected line does not count as the first value, and quotes work everywhere.
    #[test]
    fn rejected_first_value_is_not_the_first() {
        let (c, w) =
            parse("loop_threshold = 0\nloop_threshold = 4\ncommons = \"on\"\nnotify = \"true\"\n");
        assert_eq!(c.loop_threshold, 4);
        assert!(c.commons);
        assert!(c.notify);
        assert_eq!(w.len(), 1, "{w:?}");
    }

    /// Keys the core does not read are still recorded at session start (U1b split).
    #[test]
    fn launcher_keys_reach_the_session_record() {
        let (c, w) = parse("keep_days = 7\nloop_threshold = 4\nnotify = true\n");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.keep_days, 7);
        let want: BTreeMap<String, String> = [
            ("keep_days", "7"),
            ("loop_threshold", "4"),
            ("notify", "true"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(c.overrides(), want);
        assert_eq!(c.session().overrides(), want);
        assert_eq!(c.session().loop_threshold, 4);
    }

    #[test]
    fn missing_file_is_defaults() {
        let (c, w) = load_from(Path::new("/nonexistent/belay/config.toml"));
        assert!(c.overrides().is_empty() && w.is_empty());
    }
}
