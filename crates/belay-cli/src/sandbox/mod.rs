//! `belay sandbox`: run a command, a whole agent session, or a headless follow-up in a
//! sandbox: its own clone of the repo, a private home, limits. Threat model and layout:
//! docs/sandbox.md.
//!
//! A sandbox fails closed (CLAUDE.md rule 1): if it cannot be built as specified, belay
//! refuses and runs nothing. Its work lands on branch `belay/sandbox/<id>`; the user
//! merges it.

pub mod bwrap;
pub mod git;
pub mod limits;
pub mod probe;
pub mod session;

use std::ffi::OsString;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use belay_core::Event;
use clap::Subcommand;
use serde::{Deserialize, Serialize};

use crate::connector::SandboxFiles;
use crate::{audit, config, ipc, paths};
use limits::{Limits, Target, Watchdog};

#[derive(Subcommand)]
pub enum SandboxCmd {
    /// Run a command in a sandbox of this repo: its own clone on branch
    /// `belay/sandbox/<id>`, a private home, limits. Changes are committed there.
    Run {
        /// Reuse (or name) this sandbox.
        #[arg(long)]
        id: Option<String>,
        /// Keep the clone and private home after the run (the branch always stays).
        #[arg(long)]
        keep: bool,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// A whole agent session in a sandbox. belay supervises from outside; the agent's
    /// arguments follow (`--resume <id>` with `--id S` continues one).
    Claude {
        /// Reuse (or name) this sandbox.
        #[arg(long)]
        id: Option<String>,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// A headless follow-up in a sandbox: the agent works on `prompt` alone and stops.
    Task {
        prompt: String,
        /// Reuse (or name) this sandbox.
        #[arg(long)]
        id: Option<String>,
        /// Start from a copy of this conversation's context; the original is not changed.
        #[arg(long)]
        resume: Option<String>,
        /// The model the agent uses.
        #[arg(long)]
        model: Option<String>,
        /// Keep the clone and private home after the task (the branch always stays).
        #[arg(long)]
        keep: bool,
    },
    /// The sandboxes on this machine.
    List,
    /// A sandbox: branch, commits, the audit of its branch, limit events.
    Show { id: String },
    /// What a sandbox's branch changed since it started.
    Diff { id: String },
    /// Stop a running sandbox.
    Stop { id: String },
    /// Remove a sandbox's worktree and private home. Run it yourself, in your own terminal.
    Rm {
        id: String,
        /// Delete its branch too.
        #[arg(long)]
        branch: bool,
    },
    /// Try to escape a fresh sandbox, and say whether each try was contained.
    Doctor,
}

/// How a sandbox is made on this OS.
pub trait Backend {
    fn name(&self) -> &'static str;
    /// Whether it works here: Err says why not.
    fn check(&self) -> Result<()>;
    /// The program and its arguments that run `spec`.
    fn argv(&self, spec: &Spec) -> Vec<OsString>;
}

/// What runs, and what it sees.
#[derive(Debug, Clone, Default)]
pub struct Spec {
    /// The worktree, read-write, at the same path.
    pub work: PathBuf,
    /// The private home, read-write, at the same path.
    pub home: PathBuf,
    pub cwd: PathBuf,
    /// Host path → sandbox path, read-only (skipped when the host path is missing).
    pub ro: Vec<(PathBuf, PathBuf)>,
    /// The whole environment.
    pub env: Vec<(String, String)>,
    pub argv: Vec<String>,
}

/// The backend for this machine, checked. None works: refuse (fail closed).
pub fn backend() -> Result<Box<dyn Backend>> {
    if cfg!(target_os = "linux") {
        let b = bwrap::Bwrap;
        b.check()
            .context("belay: refused: no sandbox can be built here, so nothing runs")?;
        return Ok(Box::new(b));
    }
    bail!("belay: refused: belay has no sandbox for this OS yet (Linux with bubblewrap only), so nothing runs")
}

/// One sandbox's record: `<home>/sandboxes/<id>/meta.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    /// The repo's top directory (the checkout it was made from).
    pub repo: PathBuf,
    /// The commit it started from.
    pub base: String,
    pub branch: String,
    /// The repo's object dir the clone borrows (`--shared`), bound read-only. None: a
    /// sandbox made by belay before clones (a worktree), which this belay does not run.
    #[serde(default)]
    pub objects: Option<PathBuf>,
    pub created_ms: u64,
}

impl Meta {
    pub fn dir(&self) -> PathBuf {
        root().join(&self.id)
    }
    pub fn work(&self) -> PathBuf {
        self.dir().join("work")
    }
    pub fn home(&self) -> PathBuf {
        self.dir().join("home")
    }
    pub fn objects(&self) -> Result<PathBuf> {
        self.objects.clone().ok_or_else(|| {
            anyhow!(
                "belay: refused: sandbox {0} was made by an older belay (a worktree); `belay sandbox rm {0}` and start a new one",
                self.id
            )
        })
    }
}

pub fn root() -> PathBuf {
    paths::home().join("sandboxes")
}

/// Whether `p` is in a sandbox's clone (`<sandboxes>/<id>/work/...`), seen from the host.
pub fn inside_clone(p: &Path) -> bool {
    let canon = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (p, root) = (canon(p), canon(&root()));
    p.strip_prefix(&root).is_ok_and(|rest| {
        let mut c = rest.components();
        c.next().is_some() && c.next().is_some_and(|w| w.as_os_str() == "work")
    })
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 40
        && !id.starts_with(['.', '-'])
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c))
}

fn new_id(repo: &Path) -> String {
    let seed = format!(
        "{}:{}:{}",
        repo.display(),
        paths::now_ms(),
        std::process::id()
    );
    format!("sb-{}", &belay_core::text::sha256_hex(&seed)[..6])
}

pub fn open(id: &str) -> Result<Meta> {
    if !valid_id(id) {
        bail!("`{id}` is not a sandbox id");
    }
    let file = root().join(id).join("meta.json");
    let text = fs::read_to_string(&file).with_context(|| format!("no sandbox `{id}`"))?;
    serde_json::from_str(&text).with_context(|| format!("read {}", file.display()))
}

/// A new sandbox of `repo` at its HEAD. Anything half-made is removed on failure.
pub fn create(repo: &Path, id: &str) -> Result<Meta> {
    if !valid_id(id) {
        bail!("`{id}` is not a valid sandbox id (lowercase letters, digits, - _ .)");
    }
    let dir = root().join(id);
    if dir.exists() {
        bail!("sandbox `{id}` exists");
    }
    paths::ensure_private_dir(&root())?;
    paths::ensure_private_dir(&dir)?;
    let made = (|| -> Result<Meta> {
        let branch = format!("belay/sandbox/{id}");
        let head = audit::git(repo, &["rev-parse", "HEAD"], None)
            .context("the repo has no commit to start a sandbox from")?
            .trim()
            .to_string();
        // A branch left by an earlier sandbox of this id: go on from it.
        let earlier = audit::git(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{branch}^{{commit}}"),
            ],
            None,
        )
        .ok()
        .map(|s| s.trim().to_string());
        let start = earlier.as_deref().unwrap_or(&head);
        let objects = git::clone(repo, &dir.join("work"), &branch, start)?;
        let base = match &earlier {
            Some(_) => audit::git(repo, &["merge-base", "HEAD", &branch], None)?
                .trim()
                .to_string(),
            None => head.clone(),
        };
        paths::ensure_private_dir(&dir.join("home"))?;
        let meta = Meta {
            id: id.to_string(),
            repo: repo.to_path_buf(),
            base,
            branch,
            objects: Some(objects),
            created_ms: paths::now_ms(),
        };
        fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
        Ok(meta)
    })();
    if made.is_err() {
        let _ = remove_tree(&dir);
    }
    made
}

/// The user's home, for `~/` paths and the agent's files. None: they are left out.
fn real_home() -> Option<PathBuf> {
    paths::env_path("HOME")
}

fn expand(p: &str, home: Option<&Path>) -> Option<PathBuf> {
    match p.strip_prefix("~/") {
        Some(rest) => home.map(|h| h.join(rest)),
        None if p == "~" => home.map(Path::to_path_buf),
        None => Some(PathBuf::from(p)),
    }
}

/// Environment names passed into the sandbox; everything else is cleared.
const PASS: [&str; 6] = ["PATH", "LANG", "TERM", "USER", "SHELL", "TZ"];
/// Never passed, whatever the agent's connector asks: they reach out of the sandbox.
const NEVER: [&str; 5] = [
    "WSL_INTEROP",
    "SSH_AUTH_SOCK",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
];
/// Toolchain dirs whose tools need to be told where they are once HOME moves.
const TOOL_HOMES: [(&str, &str); 3] = [
    (".rustup", "RUSTUP_HOME"),
    (".nvm", "NVM_DIR"),
    (".pyenv", "PYENV_ROOT"),
];

fn passes(key: &str, agent_env: &[&str]) -> bool {
    !NEVER.contains(&key)
        && (PASS.contains(&key)
            || key.starts_with("LC_")
            || key.starts_with("BELAY_")
            || agent_env.contains(&key))
}

/// The sandbox's environment from the host's: an allowlist, a private HOME, and
/// `BELAY_SANDBOX`.
pub fn environment(
    host: &[(String, String)],
    agent_env: &[&str],
    home: &Path,
    id: &str,
    exe_dir: Option<&Path>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = host
        .iter()
        .filter(|(k, _)| passes(k, agent_env) && k != "BELAY_SANDBOX")
        .cloned()
        .collect();
    if let Some(dir) = exe_dir {
        let dir = dir.to_string_lossy();
        match env.iter_mut().find(|(k, _)| k == "PATH") {
            Some((_, v)) => *v = format!("{dir}:{v}"),
            None => env.push(("PATH".into(), format!("{dir}:/usr/bin:/bin"))),
        }
    }
    env.push(("HOME".into(), home.to_string_lossy().into_owned()));
    env.push(("BELAY_SANDBOX".into(), id.to_string()));
    env.sort();
    env
}

/// Refuse a read-only dir that would show the host: `/`, the home or above it, or
/// belay's own data.
fn safe_ro(p: &Path, home: Option<&Path>) -> Result<()> {
    let bad = p == Path::new("/")
        || home.is_some_and(|h| h.starts_with(p))
        || paths::home().starts_with(p)
        || p.starts_with(paths::home());
    if bad {
        bail!(
            "belay: refused: sandbox_ro names {}, which would show the host's home or belay's data inside the sandbox",
            p.display()
        );
    }
    Ok(())
}

/// Fill the private home with read-only copies of the agent's files.
fn prepare_home(
    meta: &Meta,
    files: &SandboxFiles,
    home: Option<&Path>,
) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut ro = Vec::new();
    let Some(h) = home else { return Ok(ro) };
    let private = meta.home();
    for rel in &files.copy {
        let src = h.join(rel);
        if !src.is_file() {
            continue;
        }
        let dst = private.join(rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        if dst.exists() {
            let _ = fs::set_permissions(&dst, fs::Permissions::from_mode(0o600));
            fs::remove_file(&dst).with_context(|| format!("replace {}", dst.display()))?;
        }
        fs::copy(&src, &dst).with_context(|| format!("copy {}", src.display()))?;
        fs::set_permissions(&dst, fs::Permissions::from_mode(0o400))?;
        // The copy is bound read-only on itself: the sandbox reads it, never changes it.
        ro.push((dst.clone(), dst));
    }
    for rel in &files.ro_dirs {
        let src = h.join(rel);
        if src.is_dir() {
            let dst = private.join(rel);
            fs::create_dir_all(&dst)?;
            ro.push((src, dst));
        }
    }
    Ok(ro)
}

/// The spec for running `argv` in `meta`'s sandbox, `rel` below its top. `socket`: the
/// belay session the command reaches, bound in.
pub fn layout(
    meta: &Meta,
    cfg: &config::Config,
    files: &SandboxFiles,
    rel: &Path,
    socket: Option<&Path>,
    argv: Vec<String>,
) -> Result<Spec> {
    let home = real_home();
    let home = home.as_deref();
    let work = meta.work();
    if !work.is_dir() {
        bail!(
            "belay: refused: sandbox {}'s clone is gone; `belay sandbox rm {}` and start a new one",
            meta.id,
            meta.id
        );
    }
    let objects = meta.objects()?;
    let mut ro = vec![(objects.clone(), objects)];
    let mut tool_env = Vec::new();
    for p in &cfg.sandbox_ro {
        let Some(p) = expand(p, home) else { continue };
        safe_ro(&p, home)?;
        if p.exists() {
            if let Some((_, var)) = TOOL_HOMES
                .iter()
                .find(|(name, _)| p.file_name().is_some_and(|n| n == *name))
            {
                tool_env.push((var.to_string(), p.to_string_lossy().into_owned()));
            }
            ro.push((p.clone(), p));
        }
    }
    ro.extend(prepare_home(meta, files, home)?);
    // belay itself, so commands inside can run it; and the session's socket, so they
    // reach the daemon outside.
    let exe = std::env::current_exe().context("find the belay binary")?;
    ro.push((exe.clone(), exe.clone()));
    if let Some(sock) = socket.filter(|s| s.exists()) {
        ro.push((sock.to_path_buf(), sock.to_path_buf()));
    }
    let host: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| socket.is_some() || k != "BELAY_SOCKET")
        .collect();
    let mut env = environment(&host, &files.env, &meta.home(), &meta.id, exe.parent());
    env.extend(tool_env);
    env.sort();
    let cwd = Some(work.join(rel))
        .filter(|c| c.is_dir())
        .unwrap_or_else(|| work.clone());
    Ok(Spec {
        work,
        home: meta.home(),
        cwd,
        ro,
        env,
        argv,
    })
}

/// One line of `<dir>/events.jsonl`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Logged {
    pub at_ms: u64,
    pub kind: String,
    pub text: String,
}

pub(crate) fn log(meta: &Meta, kind: &str, text: &str) {
    let line = Logged {
        at_ms: paths::now_ms(),
        kind: kind.into(),
        text: text.into(),
    };
    let Ok(json) = serde_json::to_string(&line) else {
        return;
    };
    if let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(meta.dir().join("events.jsonl"))
    {
        let _ = writeln!(f, "{json}");
    }
}

pub fn events(meta: &Meta) -> Vec<Logged> {
    fs::read_to_string(meta.dir().join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Record in a belay session (best effort: the sandbox's own log has it too).
pub(crate) fn tell_session(sock: Option<&Path>, event: Event) {
    if let Some(sock) = sock {
        let _ = ipc::send(
            sock,
            &ipc::Request::Sandbox { event },
            Duration::from_secs(2),
        );
    }
}

/// The calling belay session's socket, when there is one.
pub(crate) fn caller_socket() -> Option<PathBuf> {
    paths::env_path("BELAY_SOCKET")
}

/// `pid starttime` of a running sandbox's outer process.
fn pid_file(meta: &Meta) -> PathBuf {
    meta.dir().join("pid")
}

fn start_time(pid: i32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// The outer process of a running sandbox, checked against its start time.
pub fn running(meta: &Meta) -> Option<i32> {
    let text = fs::read_to_string(pid_file(meta)).ok()?;
    let (pid, start) = text.trim().split_once(' ')?;
    let pid: i32 = pid.parse().ok()?;
    (start_time(pid)? == start.parse::<u64>().ok()?).then_some(pid)
}

pub struct Outcome {
    pub code: i32,
    /// Why belay stopped it (a limit, the user), or None if it ended itself.
    pub stopped: Option<String>,
    pub stdout: Vec<u8>,
}

/// What the sandboxed process's stdin and stdout are.
pub enum Io<'a> {
    /// The terminal's.
    Inherit,
    /// No stdin; stdout kept (in `Outcome::stdout`).
    Capture,
    /// No stdin; each stdout line goes to the closure. A reason back stops the sandbox.
    Lines(&'a mut dyn FnMut(&str) -> Option<String>),
}

/// Run `spec` in the sandbox under the watchdog.
pub fn exec(
    meta: &Meta,
    backend: &dyn Backend,
    spec: &Spec,
    lim: Limits,
    mut io: Io,
) -> Result<Outcome> {
    let mut argv = backend.argv(spec);
    if limits::systemd_scope_works() {
        let mut pre: Vec<OsString> = limits::systemd_prefix(&lim)
            .into_iter()
            .map(Into::into)
            .collect();
        pre.append(&mut argv);
        argv = pre;
    }
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).env_clear();
    // systemd-run needs the user's bus; bwrap clears the rest itself (--clearenv).
    for k in ["PATH", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"] {
        if let Some(v) = std::env::var_os(k) {
            cmd.env(k, v);
        }
    }
    match io {
        Io::Inherit => {}
        Io::Capture => {
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
        }
        Io::Lines(_) => {
            cmd.stdin(Stdio::null()).stdout(Stdio::piped());
        }
    }
    limits::pre_exec(&mut cmd, lim.disk);
    let _ = fs::remove_file(meta.dir().join("stop"));
    // Ctrl-C ends the command, not belay: it still commits what was done.
    // SAFETY: setting a signal disposition has no memory preconditions.
    let old = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    let child = cmd
        .spawn()
        .with_context(|| format!("start {}", backend.name()));
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            // SAFETY: as above.
            unsafe { libc::signal(libc::SIGINT, old) };
            return Err(e);
        }
    };
    let pid = child.id() as i32;
    if let Some(start) = start_time(pid) {
        let _ = fs::write(pid_file(meta), format!("{pid} {start}"));
    }
    let dog = Watchdog::start(pid, lim, vec![meta.work(), meta.home()]);
    let waited = match &mut io {
        Io::Inherit => child.wait().map(|s| (s, Vec::new())),
        Io::Capture => child.wait_with_output().map(|o| (o.status, o.stdout)),
        Io::Lines(each) => {
            use std::io::BufRead;
            let target = Target::new(pid);
            let done = std::sync::atomic::AtomicBool::new(false);
            if let Some(out) = child.stdout.take() {
                for line in std::io::BufReader::new(out).lines() {
                    let Ok(line) = line else { break };
                    if let Some(why) = each(&line) {
                        if !meta.dir().join("stop").exists() {
                            let _ = fs::write(meta.dir().join("stop"), &why);
                            limits::stop(&target, &done);
                        }
                    }
                }
            }
            child.wait().map(|s| (s, Vec::new()))
        }
    };
    let breach = dog.finish();
    // SAFETY: as above.
    unsafe { libc::signal(libc::SIGINT, old) };
    let _ = fs::remove_file(pid_file(meta));
    let (status, stdout) = waited.context("wait for the sandbox")?;
    let by_user = fs::read_to_string(meta.dir().join("stop")).ok();
    let _ = fs::remove_file(meta.dir().join("stop"));
    use std::os::unix::process::ExitStatusExt;
    let code = status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    Ok(Outcome {
        code,
        stopped: breach.or(by_user),
        stdout,
    })
}

/// Remove the clone and the private home; the record and the branch stay.
fn remove_work(meta: &Meta) -> Result<()> {
    remove_tree(&meta.work())?;
    remove_tree(&meta.home())
}

/// remove_dir_all, after making every dir writable (a sandbox may leave some not).
pub(crate) fn remove_tree(p: &Path) -> Result<()> {
    if !p.exists() {
        return Ok(());
    }
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(m) = fs::symlink_metadata(&d) else {
            continue;
        };
        if m.is_dir() {
            let _ = fs::set_permissions(&d, fs::Permissions::from_mode(0o700));
            if let Ok(rd) = fs::read_dir(&d) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
        }
    }
    fs::remove_dir_all(p).with_context(|| format!("remove {}", p.display()))
}

fn refuse_nesting() -> Result<()> {
    if std::env::var_os("BELAY_SANDBOX").is_some() {
        bail!("belay: refused: this is already a sandbox; sandboxes do not nest");
    }
    Ok(())
}

pub fn cli(cmd: SandboxCmd) -> Result<i32> {
    refuse_nesting()?;
    match cmd {
        SandboxCmd::Run { id, keep, cmd } => run(id, keep, cmd),
        SandboxCmd::Claude { id, args } => session::claude(id, args),
        SandboxCmd::Task {
            prompt,
            id,
            resume,
            model,
            keep,
        } => session::task(session::TaskArgs {
            prompt,
            id,
            resume,
            model,
            keep,
        }),
        SandboxCmd::List => {
            print!("{}", list());
            Ok(0)
        }
        SandboxCmd::Show { id } => {
            print!("{}", show(&open(&id)?)?);
            Ok(0)
        }
        SandboxCmd::Diff { id } => {
            let meta = open(&id)?;
            let range = format!("{}..{}", meta.base, meta.branch);
            let status = Command::new("git")
                .arg("-C")
                .arg(&meta.repo)
                .args(["--no-pager", "diff", &range])
                .status()?;
            Ok(status.code().unwrap_or(1))
        }
        SandboxCmd::Stop { id } => {
            let meta = open(&id)?;
            let Some(pid) = running(&meta) else {
                println!("sandbox {id} is not running.");
                return Ok(1);
            };
            fs::write(meta.dir().join("stop"), "the user stopped it")?;
            let target = Target::new(pid);
            let done = std::sync::atomic::AtomicBool::new(false);
            limits::stop(&target, &done);
            println!("Stopped sandbox {id}.");
            Ok(0)
        }
        SandboxCmd::Rm { id, branch } => {
            paths::users_only("`belay sandbox rm`")?;
            let meta = open(&id)?;
            if running(&meta).is_some() {
                bail!("sandbox {id} is running; `belay sandbox stop {id}` first");
            }
            remove_work(&meta)?;
            remove_tree(&meta.dir())?;
            if branch {
                audit::git(&meta.repo, &["branch", "-D", "--quiet", &meta.branch], None)?;
                println!("Removed sandbox {id} and its branch {}.", meta.branch);
            } else {
                println!("Removed sandbox {id}. Its branch {} stays.", meta.branch);
            }
            Ok(0)
        }
        SandboxCmd::Doctor => probe::doctor(),
    }
}

/// The repo of the current directory, and the path below its top.
pub(crate) fn here() -> Result<(PathBuf, PathBuf)> {
    let cwd = std::env::current_dir()?;
    let repo = audit::repo_root(&cwd).map_err(|_| {
        anyhow!(
            "belay: refused: a sandbox needs a git repo, and {} is not in one",
            cwd.display()
        )
    })?;
    let rel = cwd
        .strip_prefix(&repo)
        .unwrap_or(Path::new(""))
        .to_path_buf();
    Ok((repo, rel))
}

/// Sandbox `id` of `repo` (made when it does not exist yet), and whether it was made
/// now. A running one is refused: two runs never share a clone.
pub(crate) fn open_or_create(repo: &Path, id: Option<String>) -> Result<(Meta, bool)> {
    match id {
        Some(id) if root().join(&id).join("meta.json").exists() => {
            let meta = open(&id)?;
            if meta.repo != repo {
                bail!(
                    "belay: refused: sandbox {id} is of {}, not this repo",
                    meta.repo.display()
                );
            }
            if running(&meta).is_some() {
                bail!("belay: refused: sandbox {id} is running");
            }
            meta.objects()?;
            Ok((meta, false))
        }
        Some(id) => Ok((create(repo, &id)?, true)),
        None => Ok((create(repo, &new_id(repo))?, true)),
    }
}

/// What a stopped sandbox says: its exit or the limit that stopped it.
pub(crate) fn reason(out: &Result<Outcome>, what: &str) -> String {
    match out {
        Ok(o) => o
            .stopped
            .clone()
            .unwrap_or_else(|| format!("{what} exited with {}", o.code)),
        Err(e) => format!("it did not run: {e}"),
    }
}

fn run(id: Option<String>, keep: bool, argv: Vec<String>) -> Result<i32> {
    let cfg = config::load();
    if !cfg.sandbox_agent && !paths::users_terminal() {
        bail!("belay: refused: `sandbox_agent = off` in belay's config: only the user starts a sandbox");
    }
    let backend = backend()?;
    let (repo, rel) = here()?;
    let (meta, created) = open_or_create(&repo, id)?;
    let files = crate::connector::for_agent(crate::connector::DEFAULT_AGENT)?.sandbox_files();
    let caller = caller_socket();
    let spec = match layout(&meta, &cfg, &files, &rel, caller.as_deref(), argv.clone()) {
        Ok(s) => s,
        Err(e) => {
            if created {
                let _ = remove_work(&meta);
            }
            return Err(e);
        }
    };
    let id = meta.id.clone();
    log(&meta, "started", &argv.join(" "));
    tell_session(
        caller.as_deref(),
        Event::SandboxStarted {
            id: id.clone(),
            branch: meta.branch.clone(),
        },
    );
    eprintln!("belay: sandbox {id}, branch {}", meta.branch);
    let out = exec(
        &meta,
        &*backend,
        &spec,
        Limits::from_config(&cfg),
        Io::Inherit,
    );
    let cmd = belay_core::text::truncate_chars(&argv.join(" "), 200);
    let landed = git::land(&meta, &*backend, &format!("sandbox {id}: {cmd}"));
    let code = out.as_ref().map_or(1, |o| o.code);
    let reason = reason(&out, "the command");
    if let Ok(Outcome {
        stopped: Some(why), ..
    }) = &out
    {
        log(&meta, "limit", why);
        eprintln!("belay: stopped sandbox {id}: {why}");
    }
    log(&meta, "stopped", &reason);
    tell_session(
        caller.as_deref(),
        Event::SandboxStopped {
            id: id.clone(),
            reason,
        },
    );
    eprintln!("belay: {}", landed_line(&meta, &landed));
    if created && !keep {
        remove_work(&meta)?;
    }
    out?;
    landed?;
    Ok(code)
}

/// One line on what reached the branch.
pub(crate) fn landed_line(meta: &Meta, landed: &Result<Vec<String>>) -> String {
    let id = &meta.id;
    match landed {
        Ok(f) if f.is_empty() => format!("sandbox {id}: nothing changed"),
        Ok(f) => format!(
            "sandbox {id}: {} file(s) on {}; `belay sandbox show {id}` and `belay sandbox diff {id}` show them",
            f.len(),
            meta.branch
        ),
        Err(e) => format!("sandbox {id}: could not bring its work out: {e:#}"),
    }
}

fn state(meta: &Meta) -> &'static str {
    if running(meta).is_some() {
        "running"
    } else if meta.work().exists() {
        "kept"
    } else {
        "removed"
    }
}

fn list() -> String {
    let mut ids: Vec<String> = fs::read_dir(root())
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    let metas: Vec<Meta> = ids.iter().filter_map(|id| open(id).ok()).collect();
    if metas.is_empty() {
        return "No sandboxes. `belay sandbox run -- <cmd>` starts one.\n".into();
    }
    let mut out = String::new();
    for m in metas {
        out.push_str(&format!(
            "{:<14} {:<8} {:<28} {}\n",
            m.id,
            state(&m),
            m.branch,
            m.repo.display()
        ));
    }
    out
}

fn show(meta: &Meta) -> Result<String> {
    let mut out = format!(
        "sandbox {} ({})\nrepo:   {}\nbranch: {} (from {})\n",
        meta.id,
        state(meta),
        meta.repo.display(),
        meta.branch,
        &meta.base[..meta.base.len().min(12)]
    );
    if meta.work().exists() {
        let used = limits::disk_use(&[meta.work(), meta.home()]);
        out.push_str(&format!(
            "disk:   {}\n",
            limits::fmt_size(used / 1024 * 1024)
        ));
    }
    let range = format!("{}..{}", meta.base, meta.branch);
    let log = audit::git(&meta.repo, &["log", "--oneline", &range], None).unwrap_or_default();
    out.push_str("\ncommits:\n");
    if log.trim().is_empty() {
        out.push_str("  none\n");
    }
    for l in log.lines() {
        out.push_str(&format!("  {l}\n"));
    }
    if let Ok(r) = audit::run(&meta.repo, &meta.base, &meta.branch, &|_| false) {
        out.push('\n');
        out.push_str(&audit::render_text(&r, &range));
    }
    let evs = events(meta);
    if !evs.is_empty() {
        out.push_str("\nevents:\n");
        for e in evs {
            out.push_str(&format!("  {} {:<8} {}\n", e.at_ms, e.kind, e.text));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_is_an_allowlist() {
        let host: Vec<(String, String)> = [
            ("PATH", "/usr/bin"),
            ("LC_ALL", "C"),
            ("TERM", "xterm"),
            ("BELAY_SOCKET", "/s"),
            ("BELAY_SANDBOX", "outer"),
            ("ANTHROPIC_API_KEY", "k"),
            ("AWS_SECRET_ACCESS_KEY", "s"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
            ("WSL_INTEROP", "/run/WSL/1"),
            ("DISPLAY", ":0"),
            ("WAYLAND_DISPLAY", "w"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:"),
            ("HOME", "/home/u"),
            ("GITHUB_TOKEN", "t"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .to_vec();
        let agent = ["ANTHROPIC_API_KEY", "SSH_AUTH_SOCK"];
        let env = environment(
            &host,
            &agent,
            Path::new("/sb/home"),
            "s1",
            Some(Path::new("/opt/belay")),
        );
        let get = |k: &str| env.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        assert_eq!(get("PATH"), Some("/opt/belay:/usr/bin"));
        assert_eq!(get("HOME"), Some("/sb/home"));
        assert_eq!(get("BELAY_SANDBOX"), Some("s1"));
        assert_eq!(get("ANTHROPIC_API_KEY"), Some("k"));
        for k in ["LC_ALL", "TERM", "BELAY_SOCKET"] {
            assert!(get(k).is_some(), "{k}");
        }
        // Even when the agent's connector names one of them.
        for k in NEVER
            .iter()
            .chain(&["AWS_SECRET_ACCESS_KEY", "GITHUB_TOKEN"])
        {
            assert!(get(k).is_none(), "{k} passed");
        }
    }

    #[test]
    fn ids_are_plain_names() {
        assert!(valid_id("sb-1a2b3c"));
        assert!(valid_id("my.box_2"));
        for bad in ["", "../x", "a/b", "-x", ".x", "UPPER", &"x".repeat(41)] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn read_only_dirs_never_show_the_home() {
        let home = Path::new("/home/u");
        assert!(safe_ro(Path::new("/"), Some(home)).is_err());
        assert!(safe_ro(Path::new("/home"), Some(home)).is_err());
        assert!(safe_ro(Path::new("/home/u"), Some(home)).is_err());
        assert!(safe_ro(Path::new("/home/u/.cargo"), Some(home)).is_ok());
        assert!(safe_ro(Path::new("/opt/go"), Some(home)).is_ok());
        assert_eq!(
            expand("~/.cargo", Some(home)),
            Some(PathBuf::from("/home/u/.cargo"))
        );
        assert_eq!(expand("~/.cargo", None), None);
    }
}
