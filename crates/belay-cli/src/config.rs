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
//! plugin_mode = look         # mode of sessions the Claude Code plugin starts
//! notify = false             # desktop notification on a loop or a blocked edit
//! wrap_tests = true          # optimize: run tests through `belay run` (costs ~1-2 s each in auto mode)
//! fleet = look               # other belay sessions here: off, look (record), on (also tell the agent)
//! learn = look               # recall fixes other sessions learned, and project notes: off, look, on
//! ```

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Result};
use belay_core::{FleetSetting, Mode};

/// The whole config file: the core's knobs, and those only the launcher, the plugin
/// and the daemon read. Derefs to the core's, so `cfg.loop_threshold` works.
#[derive(Debug, Clone)]
pub struct Config {
    pub core: belay_core::Config,
    /// `belay claude` deletes sessions with no activity for this many days (0: never).
    pub keep_days: u64,
    /// `belay claude` audits what the session changed in tests and checks (experimental).
    pub experimental_audit: bool,
    /// The mode of sessions the Claude Code plugin starts (no `--belay-mode` there).
    pub plugin_mode: Mode,
    /// A desktop notification when belay steers a loop or blocks an edit (unattended runs).
    pub notify: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            core: belay_core::Config::default(),
            keep_days: 30,
            experimental_audit: false,
            plugin_mode: Mode::Look,
            notify: false,
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
            (
                "plugin_mode",
                format!("{:?}", self.plugin_mode).to_lowercase(),
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

/// Never fails. Warnings go to stderr: the launcher's reaches the user before claude
/// starts; the daemon's goes to `daemon.log`.
pub fn load() -> Config {
    let p = path();
    let (c, warnings) = load_from(&p);
    for w in warnings {
        eprintln!("belay: config {}: {w}", p.display());
    }
    c
}

const KEYS: [&str; 13] = [
    "shape_threshold_tokens",
    "max_summary_ratio",
    "loop_threshold",
    "tighten_after",
    "idle_gap_minutes",
    "keep_days",
    "experimental_audit",
    "plugin_mode",
    "notify",
    "wrap_tests",
    "fleet",
    "learn",
    "commons",
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
        if let Err(e) = apply(&mut c, line, &mut seen) {
            warnings.push(format!("line {}: {e}; line ignored", n + 1));
        }
    }
    (c, warnings)
}

fn apply(c: &mut Config, line: &str, seen: &mut std::collections::BTreeSet<String>) -> Result<()> {
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
    if seen.contains(key) {
        bail!("`{key}` is set twice; the first value stays");
    }
    // Quotes are allowed on any value, as in TOML.
    let value = value.trim_matches('"');
    let int = |min: u64, max: u64| -> Result<u64> {
        let v: u64 = value.parse().map_err(|e| anyhow!("{key}: {e}"))?;
        if !(min..=max).contains(&v) {
            bail!("{key} must be in {min}..={max}, got {v}");
        }
        Ok(v)
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
        "plugin_mode" => {
            c.plugin_mode = match value {
                "look" => Mode::Look,
                "optimize" => Mode::Optimize,
                v => bail!("{key} must be look or optimize, got {v}"),
            }
        }
        "fleet" => {
            c.core.fleet = match value {
                "off" => FleetSetting::Off,
                "look" => FleetSetting::Look,
                "on" => FleetSetting::On,
                v => bail!("{key} must be off, look, or on, got {v}"),
            }
        }
        "commons" => {
            c.core.commons = match value {
                "off" => FleetSetting::Off,
                "look" => FleetSetting::Look,
                "on" => FleetSetting::On,
                v => bail!("{key} must be off, look, or on, got {v}"),
            }
        }
        "learn" => {
            c.core.learn = match value {
                "off" => FleetSetting::Off,
                "look" => FleetSetting::Look,
                "on" => FleetSetting::On,
                v => bail!("{key} must be off, look, or on, got {v}"),
            }
        }
        "wrap_tests" => {
            c.core.wrap_tests = match value {
                "true" => true,
                "false" => false,
                v => bail!("{key} must be true or false, got {v}"),
            }
        }
        "notify" => {
            c.notify = match value {
                "true" => true,
                "false" => false,
                v => bail!("{key} must be true or false, got {v}"),
            }
        }
        "experimental_audit" => {
            c.experimental_audit = match value {
                "true" => true,
                "false" => false,
                v => bail!("{key} must be true or false, got {v}"),
            }
        }
        _ => unreachable!("checked against KEYS"),
    }
    seen.insert(key.to_string());
    Ok(())
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
    fn fleet_setting() {
        assert_eq!(parse("fleet = on").0.fleet, FleetSetting::On);
        assert_eq!(parse("fleet = off").0.fleet, FleetSetting::Off);
        let (c, w) = parse("fleet = yes");
        assert_eq!(c.fleet, FleetSetting::Look);
        assert!(w[0].contains("off, look, or on"), "{w:?}");
    }

    /// A rejected line does not count as the first value, and quotes work everywhere.
    #[test]
    fn rejected_first_value_is_not_the_first() {
        let (c, w) =
            parse("loop_threshold = 0\nloop_threshold = 4\nfleet = \"on\"\nnotify = \"true\"\n");
        assert_eq!(c.loop_threshold, 4);
        assert_eq!(c.fleet, FleetSetting::On);
        assert!(c.notify);
        assert_eq!(w.len(), 1, "{w:?}");
    }

    /// Keys the core does not read are still recorded at session start (U1b split).
    #[test]
    fn launcher_keys_reach_the_session_record() {
        let (c, w) = parse("keep_days = 7\nloop_threshold = 4\nplugin_mode = optimize\n");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.keep_days, 7);
        let want: BTreeMap<String, String> = [
            ("keep_days", "7"),
            ("loop_threshold", "4"),
            ("plugin_mode", "optimize"),
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
