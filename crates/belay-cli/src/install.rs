//! `belay install` / `belay uninstall`: the Claude Code plugin, the status line, and the
//! launcher alias, in one step. Each change is shown first and needs a yes.
//!
//! The plugin goes in through `claude plugin …` (never by writing Claude Code's plugin
//! files). The status line is the one file belay edits: `<claude dir>/settings.json`, with
//! a backup next to it.

use std::env;
use std::fs;
use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

/// The plugin id in the marketplace `.claude-plugin/marketplace.json` names.
pub const PLUGIN_ID: &str = "belay@belay";
const MARKETPLACE: &str = "belay";
const DEFAULT_SOURCE: &str = "prakhar479/belay";
const STATUS_COMMAND: &str = "belay status";

/// What to install or remove. None given means the command's default.
#[derive(Debug, Default, Clone, Copy, clap::Args)]
pub struct Parts {
    /// The Claude Code plugin (through `claude plugin`).
    #[arg(long)]
    pub plugin: bool,
    /// The shell alias that starts `claude` under belay (printed, not written).
    #[arg(long)]
    pub launcher: bool,
    /// `belay status` as Claude Code's status line (in settings.json, with a backup).
    #[arg(long)]
    pub statusline: bool,
}

impl Parts {
    /// These parts, or `default` when none was asked for.
    pub fn or(self, default: Parts) -> Parts {
        if self.plugin || self.launcher || self.statusline {
            self
        } else {
            default
        }
    }
}

pub struct Opts {
    pub parts: Parts,
    /// Marketplace source for the plugin: a GitHub repo or a local clone.
    pub from: Option<String>,
    /// Replace a status line that is not belay's.
    pub force: bool,
    pub yes: bool,
}

/// Claude Code's config dir: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_dir() -> PathBuf {
    if let Some(d) = env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join(".claude")
}

pub fn settings_path() -> PathBuf {
    claude_dir().join("settings.json")
}

fn claude_bin() -> String {
    env::var("BELAY_CLAUDE_BIN").unwrap_or_else(|_| "claude".into())
}

/// Whether a `statusLine` value runs `belay status` (by name or by a path to belay).
pub fn is_belay_statusline(v: &Value) -> bool {
    let Some(cmd) = v.get("command").and_then(Value::as_str) else {
        return false;
    };
    let mut words = cmd.split_whitespace();
    let prog = words.next().map(Path::new).and_then(Path::file_name);
    prog.is_some_and(|p| p == "belay") && words.next() == Some("status") && words.next().is_none()
}

fn parse_settings(text: &str) -> Result<Map<String, Value>> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => bail!("settings.json is not a JSON object; not changing it"),
        Err(e) => bail!("settings.json is not valid JSON ({e}); not changing it"),
    }
}

fn render(m: Map<String, Value>) -> String {
    let mut s = serde_json::to_string_pretty(&Value::Object(m)).unwrap_or_default();
    s.push('\n');
    s
}

/// The settings with belay's status line added, or `None` when it is already there.
/// `text` is `None` when the file does not exist. Refuses malformed JSON, and a different
/// status line unless `force`.
pub fn merge_statusline(text: Option<&str>, force: bool) -> Result<Option<String>> {
    let mut m = parse_settings(text.unwrap_or(""))?;
    match m.get("statusLine") {
        Some(v) if is_belay_statusline(v) => return Ok(None),
        Some(v) if !force => bail!(
            "settings.json already has a status line ({}); use --force to replace it",
            v.get("command")
                .and_then(Value::as_str)
                .unwrap_or("not a command")
        ),
        _ => {}
    }
    m.insert(
        "statusLine".into(),
        json!({"type": "command", "command": STATUS_COMMAND}),
    );
    Ok(Some(render(m)))
}

/// The settings without belay's status line, or `None` when there is nothing of belay's
/// to remove (another status line is left alone).
pub fn remove_statusline(text: &str) -> Result<Option<String>> {
    let mut m = parse_settings(text)?;
    if !m.get("statusLine").is_some_and(is_belay_statusline) {
        return Ok(None);
    }
    m.remove("statusLine");
    Ok(Some(render(m)))
}

/// Writes `new` to `path`, keeping the old file as `<path>.belay-backup`.
fn write_with_backup(path: &Path, new: &str) -> Result<Option<PathBuf>> {
    let backup = if path.exists() {
        let b = path.with_extension("json.belay-backup");
        fs::copy(path, &b).with_context(|| format!("back up {}", path.display()))?;
        Some(b)
    } else {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        None
    };
    let tmp = path.with_extension("json.belay-tmp");
    fs::write(&tmp, new).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))?;
    Ok(backup)
}

/// The alias line for the user's shell (from `$SHELL`), and the file it belongs in.
pub fn alias_line(shell: &str) -> (String, &'static str) {
    let name = Path::new(shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    match name {
        "fish" => (
            "alias claude 'belay claude'; funcsave claude".into(),
            "run it once in fish",
        ),
        "zsh" => ("alias claude='belay claude'".into(), "~/.zshrc"),
        _ => ("alias claude='belay claude'".into(), "~/.bashrc"),
    }
}

enum Step {
    Claude(Vec<String>),
    Settings { path: PathBuf, new: String },
}

impl Step {
    fn describe(&self) -> String {
        match self {
            Step::Claude(args) => format!("run `{} {}`", claude_bin(), args.join(" ")),
            Step::Settings { path, .. } => format!("edit {}", path.display()),
        }
    }

    fn apply(&self) -> Result<String> {
        match self {
            Step::Claude(args) => {
                let bin = claude_bin();
                let status = Command::new(&bin)
                    .args(args)
                    .status()
                    .with_context(|| format!("run `{bin}` (set BELAY_CLAUDE_BIN)"))?;
                if !status.success() {
                    bail!("`{bin} {}` failed ({status})", args.join(" "));
                }
                Ok(format!("done: {}", self.describe()))
            }
            Step::Settings { path, new } => {
                let backup = write_with_backup(path, new)?;
                Ok(match backup {
                    Some(b) => format!("wrote {} (backup {})", path.display(), b.display()),
                    None => format!("wrote {}", path.display()),
                })
            }
        }
    }
}

fn plugin_source(from: Option<&str>) -> String {
    match from {
        // A local clone: give `claude` an absolute path, since it may resolve it elsewhere.
        Some(f) if Path::new(f).exists() => fs::canonicalize(f)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| f.to_string()),
        Some(f) => f.to_string(),
        None => DEFAULT_SOURCE.to_string(),
    }
}

fn read_settings(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn confirm(yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    eprint!("Go ahead? [y/N] ");
    std::io::stderr().flush().ok();
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// Shows the steps, asks, and runs them. Returns the exit code.
fn run_steps(steps: Vec<Step>, notes: Vec<String>, yes: bool) -> Result<i32> {
    for n in &notes {
        println!("{n}");
    }
    if steps.is_empty() {
        println!("Nothing to change.");
        return Ok(0);
    }
    println!("belay will:");
    for s in &steps {
        println!("  - {}", s.describe());
    }
    if !confirm(yes)? {
        println!("Nothing changed.");
        return Ok(1);
    }
    for s in &steps {
        println!("{}", s.apply()?);
    }
    Ok(0)
}

pub fn install(o: &Opts) -> Result<i32> {
    let mut steps = Vec::new();
    let mut notes = Vec::new();
    if o.parts.plugin {
        steps.push(Step::Claude(vec![
            "plugin".into(),
            "marketplace".into(),
            "add".into(),
            plugin_source(o.from.as_deref()),
        ]));
        steps.push(Step::Claude(vec![
            "plugin".into(),
            "install".into(),
            PLUGIN_ID.into(),
        ]));
    }
    if o.parts.statusline {
        let path = settings_path();
        match merge_statusline(read_settings(&path)?.as_deref(), o.force)? {
            Some(new) => steps.push(Step::Settings { path, new }),
            None => notes.push("The status line already runs `belay status`.".into()),
        }
    }
    if o.parts.launcher {
        let (line, place) = alias_line(&env::var("SHELL").unwrap_or_default());
        notes.push(format!(
            "To start every session under belay, add this line ({place}):\n  {line}"
        ));
    }
    run_steps(steps, notes, o.yes)
}

pub fn uninstall(o: &Opts) -> Result<i32> {
    let mut steps = Vec::new();
    let mut notes = Vec::new();
    if o.parts.plugin {
        steps.push(Step::Claude(vec![
            "plugin".into(),
            "uninstall".into(),
            PLUGIN_ID.into(),
        ]));
        steps.push(Step::Claude(vec![
            "plugin".into(),
            "marketplace".into(),
            "remove".into(),
            MARKETPLACE.into(),
        ]));
    }
    if o.parts.statusline {
        let path = settings_path();
        let new = match read_settings(&path)? {
            Some(text) => remove_statusline(&text)?,
            None => None,
        };
        match new {
            Some(new) => steps.push(Step::Settings { path, new }),
            None => notes.push("The status line is not belay's; leaving it.".into()),
        }
    }
    if o.parts.launcher {
        let (line, _) = alias_line(&env::var("SHELL").unwrap_or_default());
        notes.push(format!(
            "If you added the alias, remove this line from your shell config:\n  {line}"
        ));
    }
    run_steps(steps, notes, o.yes)
}

/// For `belay doctor`: whether Claude Code lists belay's plugin as installed.
pub fn plugin_installed(dir: &Path) -> bool {
    fs::read_to_string(dir.join("plugins/installed_plugins.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .is_some_and(|v| v.get("plugins").and_then(|p| p.get(PLUGIN_ID)).is_some())
}

/// For `belay doctor`: the status line in a settings file, if any, and whether it is
/// belay's.
pub fn statusline_of(text: &str) -> Option<(String, bool)> {
    let v = serde_json::from_str::<Value>(text).ok()?;
    let sl = v.get("statusLine")?;
    let cmd = sl
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
    Some((cmd, is_belay_statusline(sl)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn merge_into_absent_file_adds_only_the_status_line() {
        let out = merge_statusline(None, false).unwrap().unwrap();
        assert_eq!(
            obj(&out),
            json!({"statusLine": {"type": "command", "command": "belay status"}})
        );
    }

    #[test]
    fn merge_keeps_other_keys() {
        let before = r#"{"model": "opus", "enabledPlugins": {"x@y": true}}"#;
        let out = merge_statusline(Some(before), false).unwrap().unwrap();
        let v = obj(&out);
        assert_eq!(v["model"], "opus");
        assert_eq!(v["enabledPlugins"]["x@y"], true);
        assert_eq!(v["statusLine"]["command"], "belay status");
    }

    #[test]
    fn merge_when_already_belay_changes_nothing() {
        let before = r#"{"statusLine": {"type": "command", "command": "belay status"}}"#;
        assert!(merge_statusline(Some(before), false).unwrap().is_none());
        let by_path = r#"{"statusLine": {"type": "command", "command": "/opt/bin/belay status"}}"#;
        assert!(merge_statusline(Some(by_path), false).unwrap().is_none());
    }

    #[test]
    fn merge_refuses_a_different_status_line_without_force() {
        let before = r#"{"statusLine": {"type": "command", "command": "~/bin/line.sh"}}"#;
        let err = merge_statusline(Some(before), false).unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");
        let out = merge_statusline(Some(before), true).unwrap().unwrap();
        assert_eq!(obj(&out)["statusLine"]["command"], "belay status");
    }

    #[test]
    fn malformed_settings_are_refused() {
        assert!(merge_statusline(Some("{ not json"), false).is_err());
        assert!(merge_statusline(Some("[1, 2]"), true).is_err());
        assert!(remove_statusline("{ not json").is_err());
    }

    #[test]
    fn empty_file_is_treated_as_no_settings() {
        assert!(merge_statusline(Some("  \n"), false).unwrap().is_some());
    }

    #[test]
    fn remove_leaves_a_status_line_that_is_not_belays() {
        let before = r#"{"statusLine": {"type": "command", "command": "belay-other status"}}"#;
        assert!(remove_statusline(before).unwrap().is_none());
        assert!(remove_statusline(r#"{"model": "opus"}"#).unwrap().is_none());
    }

    #[test]
    fn install_then_uninstall_round_trips_through_a_file() {
        let home = std::env::temp_dir().join(format!("belay-install-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let path = home.join(".claude/settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let before = json!({"model": "opus", "permissions": {"allow": ["Bash(ls:*)"]}});
        fs::write(&path, before.to_string()).unwrap();

        let new = merge_statusline(read_settings(&path).unwrap().as_deref(), false)
            .unwrap()
            .unwrap();
        let backup = write_with_backup(&path, &new).unwrap().unwrap();
        assert_eq!(obj(&fs::read_to_string(&backup).unwrap()), before);
        let installed = fs::read_to_string(&path).unwrap();
        assert!(statusline_of(&installed).unwrap().1);

        let new = remove_statusline(&installed).unwrap().unwrap();
        write_with_backup(&path, &new).unwrap();
        assert_eq!(obj(&fs::read_to_string(&path).unwrap()), before);
        assert!(!home.join(".claude/settings.json.belay-tmp").exists());
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn install_into_a_missing_dir_creates_it_without_a_backup() {
        let home = std::env::temp_dir().join(format!("belay-install-new-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let path = home.join(".claude/settings.json");
        let new = merge_statusline(read_settings(&path).unwrap().as_deref(), false)
            .unwrap()
            .unwrap();
        assert!(write_with_backup(&path, &new).unwrap().is_none());
        assert!(path.exists());
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn doctor_sees_the_installed_plugin() {
        let dir = std::env::temp_dir().join(format!("belay-install-doc-{}", std::process::id()));
        fs::create_dir_all(dir.join("plugins")).unwrap();
        assert!(!plugin_installed(&dir));
        fs::write(
            dir.join("plugins/installed_plugins.json"),
            r#"{"version": 2, "plugins": {"belay@belay": [{"scope": "user"}]}}"#,
        )
        .unwrap();
        assert!(plugin_installed(&dir));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn alias_matches_the_shell() {
        assert_eq!(alias_line("/usr/bin/zsh").1, "~/.zshrc");
        assert!(alias_line("/usr/bin/fish").0.contains("funcsave"));
        assert_eq!(alias_line("").0, "alias claude='belay claude'");
    }
}
