//! `belay doctor`: will belay 0.2 work here? Each check is one line, `ok`, `warn`, or
//! `fail`, with one sentence and the fix. It changes nothing and runs no sandbox probe
//! (`belay sandbox doctor` does that).
//!
//! `gather` does the I/O into `Facts`; `checks` turns facts into lines and is pure, so
//! each check is tested with injected inputs.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use serde_json::Value;

use crate::sandbox::{self, Backend as _};
use crate::{config, disk, install, keepawake, paths, sessions, skillstore};

#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

/// One line of the report.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub status: Status,
    /// What it is about: `belay`, `claude`, `home`, `plugin`, `sandbox`, `data`, ….
    pub area: &'static str,
    /// One sentence: what is so.
    pub text: String,
    /// What to do about it, when there is something to do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

fn check(status: Status, area: &'static str, text: impl Into<String>, fix: Option<&str>) -> Check {
    Check {
        status,
        area,
        text: text.into(),
        fix: fix.map(str::to_string),
    }
}

/// The report as text, and the exit code: 1 only when a check fails.
pub fn run(json: bool) -> (String, i32) {
    let all = checks(&gather());
    let code = i32::from(all.iter().any(|c| c.status == Status::Fail));
    let out = if json { to_json(&all) } else { render(&all) };
    (out, code)
}

pub fn render(all: &[Check]) -> String {
    let mut out = String::new();
    for c in all {
        let status = match c.status {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        };
        let _ = write!(out, "{status:<5} {:<10} {}", c.area, c.text);
        if let Some(fix) = &c.fix {
            let _ = write!(out, " Fix: {fix}");
        }
        out.push('\n');
    }
    out
}

pub fn to_json(all: &[Check]) -> String {
    let v = serde_json::json!({
        "ok": !all.iter().any(|c| c.status == Status::Fail),
        "checks": all,
    });
    serde_json::to_string_pretty(&v).unwrap_or_default() + "\n"
}

/// What `gather` found. Every field is plain data, so tests build one by hand.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub version: String,
    /// This binary, and the `belay` on PATH.
    pub exe: Option<PathBuf>,
    pub on_path: Option<PathBuf>,
    pub same_binary: bool,
    /// The agent binary's name, and its version (None: not found or failed).
    pub claude_bin: String,
    pub claude_version: Option<String>,
    /// Claude Code versions with a record in docs/conformance/ when this was built.
    pub conformant: Vec<String>,
    pub home: PathBuf,
    pub home_state: HomeState,
    pub config_path: PathBuf,
    /// The config file's text (None: no file).
    pub config_text: Option<String>,
    pub config_warnings: Vec<String>,
    pub config_overrides: usize,
    /// Whether `belay config migrate` is in this build.
    pub has_config_migrate: bool,
    pub plugin: Plugin,
    /// The launcher alias: the file looked in, and whether it is there.
    pub alias_place: String,
    pub alias_line: String,
    pub alias: bool,
    /// The status line command, and whether it is belay's.
    pub statusline: Option<(String, bool)>,
    pub sandbox: SandboxFacts,
    /// From the skill store: drift and pending lines (status, text).
    pub skills: Vec<(&'static str, String)>,
    pub data: DataFacts,
    /// Disk use by part (fleets, sandboxes, sessions, cache), P0.4.
    pub disk: disk::Usage,
    /// What `keepawake` can hold off here, for background sessions.
    pub background: keepawake::Assessment,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HomeState {
    #[default]
    Missing,
    Symlink,
    /// The dir's permission bits.
    Mode(u32),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Plugin {
    #[default]
    Absent,
    /// Installed, with the version Claude Code recorded (if any).
    Installed(Option<String>),
}

#[derive(Debug, Clone, Default)]
pub struct SandboxFacts {
    pub linux: bool,
    pub bwrap: bool,
    /// Why the backend's check says it cannot make a sandbox (None: it can).
    pub userns_error: Option<String>,
    pub wsl: bool,
    /// Whether the layout leaves out WSL's ways out (/init, /mnt, WSL_INTEROP).
    pub interop_hidden: bool,
    pub systemd_scope: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DataFacts {
    pub home_bytes: u64,
    pub sessions: usize,
    pub live: usize,
    /// Days since the least recently active session was last active.
    pub oldest_days: Option<u64>,
    pub keep_days: u64,
}

pub fn checks(f: &Facts) -> Vec<Check> {
    let mut out = vec![binary(f), claude(f), home(f)];
    out.extend(config_checks(f));
    out.push(plugin(f));
    out.push(alias(f));
    out.push(statusline(f));
    out.extend(sandbox_checks(&f.sandbox));
    out.extend(skills(&f.skills));
    out.push(data(&f.data));
    out.push(disk_check(&f.disk));
    out.push(background(&f.background));
    out
}

// The `belay` the agent will run: hooks call this binary by absolute path; the agent's
// `belay show` goes through PATH.
fn binary(f: &Facts) -> Check {
    match (&f.on_path, &f.exe) {
        (Some(_), Some(e)) if f.same_binary => check(
            Status::Ok,
            "belay",
            format!("belay {} at {}.", f.version, e.display()),
            None,
        ),
        (Some(p), _) => check(
            Status::Warn,
            "belay",
            format!("`belay` on PATH is {}, not this binary.", p.display()),
            Some("reinstall with `cargo install --path crates/belay-cli`."),
        ),
        (None, _) => check(
            Status::Warn,
            "belay",
            "`belay` is not on PATH, so the agent cannot run `belay show`.",
            Some("put belay's install dir on PATH."),
        ),
    }
}

fn claude(f: &Facts) -> Check {
    match &f.claude_version {
        Some(v) if f.conformant.iter().any(|c| c == v) => check(
            Status::Ok,
            "claude",
            format!("Claude Code {v} is one belay was checked against."),
            None,
        ),
        Some(v) => check(
            Status::Warn,
            "claude",
            format!("Claude Code {v} has no conformance record, so some hooks may differ."),
            Some("run scripts/conformance.py --record from belay's source tree."),
        ),
        None => check(
            Status::Fail,
            "claude",
            format!("`{}` was not found or failed.", f.claude_bin),
            Some("install Claude Code, or set BELAY_CLAUDE_BIN."),
        ),
    }
}

fn home(f: &Facts) -> Check {
    let h = f.home.display();
    match f.home_state {
        HomeState::Missing => check(
            Status::Ok,
            "home",
            format!("Data dir {h} does not exist yet; belay makes it on start."),
            None,
        ),
        HomeState::Symlink => check(
            Status::Fail,
            "home",
            format!("Data dir {h} is a symlink, and belay will not use it."),
            Some("replace it with a real directory."),
        ),
        HomeState::Mode(m) if m & 0o077 != 0 => check(
            Status::Warn,
            "home",
            format!("Data dir {h} is readable by others ({:o}).", m & 0o777),
            Some(&format!("`chmod 700 {h}` (belay also does it on start).")),
        ),
        HomeState::Mode(_) => check(Status::Ok, "home", format!("Data dir {h}."), None),
    }
}

/// 0.1 names in a config file: keys that were renamed or dropped, and `look` values.
pub fn old_keys(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim().trim_matches('"'));
        if matches!(k, "plugin_mode" | "fleet" | "learn") {
            out.push(k.to_string());
        } else if v == "look" {
            out.push(format!("{k} = look"));
        }
    }
    out
}

fn config_checks(f: &Facts) -> Vec<Check> {
    let p = f.config_path.display();
    let Some(text) = &f.config_text else {
        return vec![check(
            Status::Ok,
            "config",
            format!("No config at {p}; the defaults apply."),
            None,
        )];
    };
    let mut out = Vec::new();
    let old = old_keys(text);
    if !old.is_empty() {
        let fix = if f.has_config_migrate {
            "run `belay config migrate`."
        } else {
            "rename them as docs/guide.md § Config says."
        };
        out.push(check(
            Status::Warn,
            "config",
            format!("{p} uses 0.1 names: {}.", old.join(", ")),
            Some(fix),
        ));
    }
    // The 0.1 names already have their line; the rest are bad lines.
    let other: Vec<&String> = f
        .config_warnings
        .iter()
        .filter(|w| old.is_empty() || !is_old_key_warning(w))
        .collect();
    for w in other {
        out.push(check(
            Status::Warn,
            "config",
            format!("{p}: {w}."),
            Some("fix or remove that line."),
        ));
    }
    if out.is_empty() {
        out.push(check(
            Status::Ok,
            "config",
            format!("Config {p} ({} values set).", f.config_overrides),
            None,
        ));
    }
    out
}

fn is_old_key_warning(w: &str) -> bool {
    ["`plugin_mode`", "`fleet`", "`learn`", "= look`"]
        .iter()
        .any(|k| w.contains(k))
}

/// The plugin's entry in Claude Code's `plugins/installed_plugins.json`.
pub fn plugin_of(text: &str) -> Plugin {
    let Some(entry) = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get("plugins")?.get(install::PLUGIN_ID).cloned())
    else {
        return Plugin::Absent;
    };
    // An array of installs (one per scope); an object in older files.
    let first = entry
        .as_array()
        .and_then(|a| a.first().cloned())
        .unwrap_or(entry);
    Plugin::Installed(
        first
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_string),
    )
}

fn plugin(f: &Facts) -> Check {
    match &f.plugin {
        Plugin::Absent => check(
            Status::Warn,
            "plugin",
            "The Claude Code plugin is not installed, so plain `claude` runs without belay.",
            Some("`belay install`."),
        ),
        Plugin::Installed(Some(v)) if *v != f.version => check(
            Status::Warn,
            "plugin",
            format!("The plugin is {v}, but this belay is {}.", f.version),
            Some("`belay install` again to update it."),
        ),
        Plugin::Installed(v) => check(
            Status::Ok,
            "plugin",
            format!(
                "Plugin {} {} installed.",
                install::PLUGIN_ID,
                v.as_deref().unwrap_or("(no version recorded)")
            ),
            None,
        ),
    }
}

/// Whether a shell file starts `claude` under belay: a non-comment line with
/// `belay claude` in it (bash/zsh alias, fish function).
pub fn has_alias(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .any(|l| !l.starts_with('#') && l.contains("belay claude"))
}

fn alias(f: &Facts) -> Check {
    if f.alias {
        check(
            Status::Ok,
            "alias",
            format!("`claude` starts under belay ({}).", f.alias_place),
            None,
        )
    } else if matches!(f.plugin, Plugin::Installed(_)) {
        check(
            Status::Ok,
            "alias",
            format!(
                "No launcher alias in {}; the plugin supervises plain `claude`.",
                f.alias_place
            ),
            None,
        )
    } else {
        check(
            Status::Warn,
            "alias",
            format!("No launcher alias in {}.", f.alias_place),
            Some(&format!("add `{}`, or run `belay install`.", f.alias_line)),
        )
    }
}

fn statusline(f: &Facts) -> Check {
    match &f.statusline {
        Some((_, true)) => check(
            Status::Ok,
            "statusline",
            "The status line shows `belay status`.",
            None,
        ),
        Some((cmd, false)) => check(
            Status::Warn,
            "statusline",
            format!("The status line runs `{cmd}`, so belay's notes do not show there."),
            Some("`belay install --statusline --force` replaces it."),
        ),
        None => check(
            Status::Warn,
            "statusline",
            "There is no status line, so belay's notes do not show there.",
            Some("`belay install --statusline`."),
        ),
    }
}

// The sandbox is optional: when it cannot be built, `belay sandbox` refuses (rule 1), and
// the rest of belay works. So these warn, never fail.
fn sandbox_checks(s: &SandboxFacts) -> Vec<Check> {
    const MORE: &str = "`belay sandbox doctor` tries real escapes.";
    if !s.linux {
        return vec![check(
            Status::Warn,
            "sandbox",
            "belay has no sandbox for this OS yet (Linux with bubblewrap only).",
            None,
        )];
    }
    let mut out = Vec::new();
    if !s.bwrap {
        out.push(check(
            Status::Warn,
            "sandbox",
            "bubblewrap is not installed, so `belay sandbox` refuses to run.",
            Some("install bubblewrap (`apt install bubblewrap`, `dnf install bubblewrap`)."),
        ));
    } else {
        match &s.userns_error {
            None => out.push(check(
                Status::Ok,
                "sandbox",
                "bwrap can make a sandbox here (user namespaces work).",
                None,
            )),
            Some(e) => out.push(check(
                Status::Warn,
                "sandbox",
                format!("bwrap cannot make a sandbox here: {e}."),
                Some("enable unprivileged user namespaces (sysctl kernel.unprivileged_userns_clone=1, or an AppArmor profile for bwrap)."),
            )),
        }
    }
    if s.wsl {
        if s.interop_hidden {
            out.push(check(
                Status::Ok,
                "sandbox",
                format!("WSL: the layout hides /init, /mnt, and WSL_INTEROP. {MORE}"),
                None,
            ));
        } else {
            out.push(check(
                Status::Fail,
                "sandbox",
                "WSL: the sandbox layout leaves a way to Windows open.",
                Some("report it; do not use `belay sandbox` here."),
            ));
        }
    }
    out.push(if s.systemd_scope {
        check(
            Status::Ok,
            "sandbox",
            "Limits use a systemd user scope, with belay's watchdog as well.",
            None,
        )
    } else {
        check(
            Status::Ok,
            "sandbox",
            "No systemd user scope here, so belay's watchdog alone enforces limits.",
            None,
        )
    });
    out
}

/// The skill store's lines (U9): drift warns; skills waiting for approval warn too, as
/// they need the user.
fn skills(lines: &[(&'static str, String)]) -> Vec<Check> {
    if lines.is_empty() {
        return vec![check(
            Status::Ok,
            "skills",
            "No skills installed by belay.",
            None,
        )];
    }
    lines
        .iter()
        .map(|(status, text)| {
            let (status, fix) = match *status {
                "ok" => (Status::Ok, None),
                _ if text.contains("wait for") => (
                    Status::Warn,
                    Some("`belay skill list`, then approve or remove each."),
                ),
                _ => (Status::Warn, Some("`belay skill list` shows what changed.")),
            };
            check(status, "skills", format!("{text}."), fix)
        })
        .collect()
}

fn data(d: &DataFacts) -> Check {
    let size = format!(
        "belay home {}, {} sessions ({} live)",
        human_size(d.home_bytes),
        d.sessions,
        d.live
    );
    match d.oldest_days {
        Some(days) if d.keep_days > 0 && days > d.keep_days => check(
            Status::Warn,
            "data",
            format!(
                "{size}; the oldest is {days} days old, past keep_days = {}.",
                d.keep_days
            ),
            Some(&format!(
                "`belay claude` prunes on start, or `belay purge --older-than {}`.",
                d.keep_days
            )),
        ),
        Some(days) => check(
            Status::Ok,
            "data",
            format!(
                "{size}; the oldest is {days} days old; {}.",
                retention(d.keep_days)
            ),
            None,
        ),
        None => check(
            Status::Ok,
            "data",
            format!("{size}; {}.", retention(d.keep_days)),
            None,
        ),
    }
}

use disk::human as human_size;

/// What fleets, sandboxes, sessions and the build cache take; a warning past 10 GB.
fn disk_check(u: &disk::Usage) -> Check {
    if u.over() {
        check(
            Status::Warn,
            "disk",
            format!("{} That is over {}.", u.line(), human_size(disk::WARN_BYTES)),
            Some("`belay fleet prune --yes` removes finished fleet worktrees; `belay sandbox rm <id>` a sandbox; `belay purge --older-than <days>` old sessions; the build cache can go (`rm -r <home>/cache`)."),
        )
    } else {
        check(Status::Ok, "disk", u.line(), None)
    }
}

// A background session needs the machine awake. belay never changes the setting itself,
// so anything short of both held is a warning with the user's fix.
fn background(a: &keepawake::Assessment) -> Check {
    if a.all_held() {
        check(Status::Ok, "background", a.summary(), None)
    } else {
        check(Status::Warn, "background", a.summary(), a.fix.as_deref())
    }
}

/// What `belay claude` does with old sessions. `keep_days = 0` keeps them all.
fn retention(keep_days: u64) -> String {
    if keep_days == 0 {
        "`belay claude` keeps old sessions (keep_days = 0)".into()
    } else {
        format!("`belay claude` deletes those older than keep_days = {keep_days}")
    }
}

// ---- I/O: everything below reads the machine.

fn gather() -> Facts {
    let exe = env::current_exe().ok();
    let on_path = find_on_path("belay");
    let same_binary = matches!((&on_path, &exe), (Some(p), Some(e)) if same_file(p, e));
    let claude_bin = env::var("BELAY_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    let claude_version = version_of(&claude_bin)
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .next()
                .map(str::to_string)
        });
    let home = paths::home();
    let home_state = match fs::symlink_metadata(&home) {
        Err(_) => HomeState::Missing,
        Ok(m) if m.file_type().is_symlink() => HomeState::Symlink,
        Ok(m) => HomeState::Mode(m.permissions().mode()),
    };
    let config_path = config::path();
    let (cfg, config_warnings) = config::load_from(&config_path);
    let config_text = fs::read_to_string(&config_path).ok();
    let plugin = plugin_of(
        &fs::read_to_string(install::claude_dir().join("plugins/installed_plugins.json"))
            .unwrap_or_default(),
    );
    let shell = env::var("SHELL").unwrap_or_default();
    let (alias_line, _) = install::alias_line(&shell);
    let alias_file = alias_file(&shell);
    let alias = alias_file
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .is_some_and(|t| has_alias(&t));
    let alias_place = alias_file.map_or("your shell config".into(), |p| p.display().to_string());
    let settings = fs::read_to_string(install::settings_path()).unwrap_or_default();
    // One walk of belay's home: the data line and the disk line both come from it.
    let disk = disk::usage(&home);
    let root = paths::sessions_dir();
    let all: Vec<PathBuf> = fs::read_dir(&root)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    let now = paths::now_ms() / 1000;
    let oldest_days = all
        .iter()
        .map(|d| sessions::last_active_secs(d))
        .min()
        .map(|t| now.saturating_sub(t) / 86_400);
    Facts {
        version: env!("CARGO_PKG_VERSION").into(),
        exe,
        on_path,
        same_binary,
        claude_bin,
        claude_version,
        conformant: conformant(),
        home_state,
        config_path,
        config_text,
        config_warnings,
        config_overrides: cfg.overrides().len(),
        // `belay config migrate` is not in this build; the check points to the guide.
        has_config_migrate: false,
        plugin,
        alias_place,
        alias_line,
        alias,
        statusline: install::statusline_of(&settings),
        sandbox: gather_sandbox(),
        skills: skillstore::doctor_lines(),
        data: DataFacts {
            home_bytes: disk.total() + disk.rest,
            sessions: all.len(),
            live: all.iter().filter(|d| sessions::is_live(d)).count(),
            oldest_days,
            keep_days: cfg.keep_days,
        },
        disk,
        background: keepawake::assess(),
        home,
    }
}

/// The versions build.rs baked in from docs/conformance/<version>.md.
pub fn conformant() -> Vec<String> {
    env!("BELAY_CONFORMANT")
        .split(',')
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .collect()
}

/// Where the launcher alias lives for this shell.
fn alias_file(shell: &str) -> Option<PathBuf> {
    let home = paths::env_path("HOME")?;
    let name = Path::new(shell).file_name().and_then(|n| n.to_str());
    Some(match name {
        Some("fish") => home.join(".config/fish/functions/claude.fish"),
        Some("zsh") => home.join(".zshrc"),
        _ => home.join(".bashrc"),
    })
}

fn gather_sandbox() -> SandboxFacts {
    let linux = cfg!(target_os = "linux");
    let bwrap = linux && find_on_path("bwrap").is_some();
    let userns_error = if bwrap {
        sandbox::bwrap::Bwrap
            .check()
            .err()
            .map(|e| format!("{e:#}"))
    } else {
        None
    };
    SandboxFacts {
        linux,
        bwrap,
        userns_error,
        wsl: linux && sandbox::probe::wsl(),
        interop_hidden: interop_hidden(),
        systemd_scope: linux && sandbox::limits::systemd_scope_works(),
    }
}

/// Whether the sandbox layout, as built, leaves out WSL's ways to Windows: the /init
/// interop binary, the /mnt drives, and the WSL_INTEROP socket. Built from the real
/// layout code with every host path present; nothing runs.
pub fn interop_hidden() -> bool {
    let spec = sandbox::Spec {
        work: "/w".into(),
        home: "/h".into(),
        cwd: "/w".into(),
        env: sandbox::environment(
            &[("WSL_INTEROP".into(), "/run/WSL/1_interop".into())],
            &["WSL_INTEROP"],
            Path::new("/h"),
            "doctor",
            None,
        ),
        ..Default::default()
    };
    let args = sandbox::bwrap::args(&spec, &|_| true);
    !args.iter().any(|a| {
        let a = a.to_string_lossy();
        a == "/" || a == "/init" || a == "/mnt" || a.starts_with("/mnt/") || a == "WSL_INTEROP"
    })
}

pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// `<bin> --version`, given at most 3 s: a stuck binary must not hang `belay doctor`.
fn version_of(bin: &str) -> std::io::Result<std::process::Output> {
    keepawake::output_within(
        Command::new(bin).arg("--version"),
        std::time::Duration::from_secs(3),
    )
}
