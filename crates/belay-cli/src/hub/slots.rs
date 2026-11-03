//! Starting slots (agent sessions), their workspaces, checks and output, and the process helpers.

use super::*;

impl Hub {
    /// Queue one JSON line for a slot's stdin. False if it has none.
    pub(super) fn write_line(&mut self, slot: &str, v: &serde_json::Value) -> bool {
        let Some(stdin) = self.procs.get(slot).and_then(|p| p.stdin.as_ref()) else {
            return false;
        };
        stdin.send(v.to_string()).is_ok()
    }

    pub(super) fn spawn(
        &mut self,
        slot: &str,
        workspace: &Workspace,
        worktree_of: Option<&str>,
        model: Option<&str>,
        permission_mode: Option<&str>,
        brief: &str,
    ) -> Result<String> {
        let cwd = self.workspace_dir(worktree_of.unwrap_or(slot), workspace)?;
        // A stream slot that ran before resumes its conversation (G1); a oneshot always
        // starts fresh, as a task's attempts do.
        let resume = self
            .model
            .role_of(slot)
            .filter(|r| r.run == Run::Stream)
            .and(self.last_session.get(slot).cloned());
        let session = match &resume {
            Some(s) => s.clone(),
            None => uuid_v4()?,
        };
        let exe = std::env::current_exe().context("find belay executable")?;
        let files = self.dir.join("slots");
        paths::ensure_private_dir(&files)?;
        let name = file_name(slot);
        let err = OpenOptions::new()
            .create(true)
            .append(true)
            .open(files.join(format!("{name}.err")))?;
        let mut cmd = Command::new(exe);
        cmd.args(["claude", "-p"])
            .args([
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
            ])
            .args(match &resume {
                Some(s) => ["--resume", s.as_str()],
                None => ["--session-id", session.as_str()],
            })
            .arg("--append-system-prompt")
            .arg(brief);
        if let Some(m) = model {
            cmd.args(["--model", m]);
        }
        if let Some(p) = permission_mode {
            cmd.args(["--permission-mode", p]);
        }
        let mut child = cmd
            .current_dir(&cwd)
            .env("BELAY_FLEET", &self.meta.id)
            .env("BELAY_FLEET_SLOT", slot)
            .env("BELAY_FLEET_RULES", self.rules_for(slot, &cwd))
            .envs(self.build_env(&cwd))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(err)
            .process_group(0)
            .spawn()
            .context("start belay claude")?;
        let pid = child.id() as i32;
        // For a restarted hub to stop it (G1): the launcher leads the session's group.
        let _ = fs::write(files.join(format!("{name}.pid")), pid.to_string());
        // Its own writer: a full pipe blocks this thread, not the hub.
        let stdin = child.stdin.take().map(|mut pipe| {
            let (tx, rx) = mpsc::channel::<String>();
            thread::spawn(move || {
                for mut line in rx {
                    line.push('\n');
                    if pipe
                        .write_all(line.as_bytes())
                        .and_then(|_| pipe.flush())
                        .is_err()
                    {
                        return;
                    }
                }
            });
            tx
        });
        let stdout = child.stdout.take();
        let exited = Arc::new(AtomicBool::new(false));
        // The exit waits a little for the output's last lines: a session that stops at
        // a usage limit must be known as limited when its exit is judged (BG0).
        let (read_tx, read_rx) = mpsc::channel::<()>();
        if let Some(out) = stdout {
            let log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(files.join(format!("{name}.jsonl")))?;
            let (tx, slot) = (self.tx.clone(), slot.to_string());
            thread::spawn(move || {
                read_output(out, log, &slot, &tx);
                drop(read_tx);
            });
        }
        {
            let (tx, slot, exited) = (self.tx.clone(), slot.to_string(), Arc::clone(&exited));
            thread::spawn(move || {
                let code = child.wait().ok().and_then(|s| s.code());
                // A child that keeps the pipe open must not hold the exit up for long.
                let _ = read_rx.recv_timeout(Duration::from_secs(2));
                exited.store(true, Ordering::SeqCst);
                let _ = tx.send(Msg::Event(FleetEvent::Exited { slot, code }));
            });
        }
        self.worktrees.insert(slot.to_string(), cwd.clone());
        self.procs.insert(
            slot.to_string(),
            Proc {
                pid,
                stdin,
                turns: 0,
                exited,
                worktree: cwd,
            },
        );
        Ok(session)
    }

    /// The slot's working directory: its own worktree (made once, kept across restarts;
    /// for a task's next attempt, the slot's where the last one stopped), a team's
    /// shared one, or the checkout itself.
    pub(super) fn workspace_dir(&self, slot: &str, ws: &Workspace) -> Result<PathBuf> {
        let (Some(repo), Some(base)) = (&self.meta.repo, &self.meta.base) else {
            return Ok(PathBuf::from(&self.meta.cwd));
        };
        let name = match ws {
            Workspace::Shared => return Ok(PathBuf::from(&self.meta.cwd)),
            Workspace::WorktreePerWorker => slot.to_string(),
            Workspace::Team { name } => format!("team-{name}"),
        };
        let path = self.dir.join("worktrees").join(file_name(&name));
        if path.exists() {
            return Ok(path);
        }
        paths::ensure_private_dir(&self.dir.join("worktrees"))?;
        let branch = branch_name(&self.meta.id, &name);
        let repo = Path::new(repo);
        let have_branch = git_out(repo, &["rev-parse", "--verify", "--quiet", &branch]).is_some();
        let mut args = vec!["worktree", "add", "--quiet"];
        let p = path.to_string_lossy().to_string();
        if have_branch {
            args.extend([p.as_str(), branch.as_str()]);
        } else {
            args.extend(["-b", branch.as_str(), p.as_str(), base.as_str()]);
        }
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(&args)
            .stdin(Stdio::null())
            .output()
            .context("run git worktree add")?;
        if !out.status.success() {
            bail!(
                "git worktree add: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(path)
    }
}

impl Hub {
    /// Run a task's check in its slot's worktree, off the hub's loop; the result comes
    /// back as a CheckResult event.
    pub(super) fn run_check(&mut self, task: &str, slot: &str, command: &str) {
        let tx = self.tx.clone();
        let (task, command) = (task.to_string(), command.to_string());
        let wt = self
            .worktrees
            .get(slot)
            .cloned()
            .or_else(|| self.slot_dir(slot));
        let base = self.bases.get(&task).cloned();
        let scratch = self.dir.join("scratch");
        let lock = Arc::clone(&self.checking);
        let audit = self.audit;
        let env = wt.as_deref().map(|w| self.build_env(w)).unwrap_or_default();
        thread::spawn(move || {
            let _one = lock.lock().unwrap_or_else(|e| e.into_inner());
            let event = match wt {
                Some(wt) => {
                    // The tree the check judges: what the task's dependents get if it
                    // passes (the session may go on editing after).
                    if let Some(tree) = crate::audit::repo_root(&wt)
                        .ok()
                        .and_then(|root| crate::audit::worktree_tree(&root, &scratch).ok())
                    {
                        let _ = tx.send(Msg::Tree(task.clone(), tree));
                    }
                    let (exit, output) = run_with_timeout(&wt, &command, &env, CHECK_TIMEOUT);
                    let passed = exit == Some(0);
                    let flags = match (&base, passed && audit) {
                        (Some((root, from)), true) => audit_flags(root, from, &scratch),
                        _ => Vec::new(),
                    };
                    let diff_lines = if passed {
                        diff_size(&wt, base.as_ref().map(|b| b.1.as_str()))
                    } else {
                        None
                    };
                    FleetEvent::CheckResult {
                        task,
                        passed,
                        exit,
                        excerpt: if passed {
                            String::new()
                        } else {
                            excerpt(&output)
                        },
                        flags,
                        diff_lines,
                    }
                }
                None => FleetEvent::CheckResult {
                    task,
                    passed: false,
                    exit: None,
                    excerpt: "belay found no worktree for the session to check.".into(),
                    flags: Vec::new(),
                    diff_lines: None,
                },
            };
            let _ = tx.send(Msg::Event(event));
        });
    }

    /// A terminal session joins as an attached role (F4). The reply is JSON with its slot
    /// and rules, or a refusal.
    pub(super) fn join(
        &mut self,
        role: Option<String>,
        name: Option<String>,
        pid: i32,
        cwd: String,
    ) -> String {
        let attached: Vec<&String> = self
            .meta
            .spec
            .roles
            .iter()
            .filter(|(_, r)| r.run == Run::Attached)
            .map(|(n, _)| n)
            .collect();
        let role = match (role, attached.as_slice()) {
            (Some(r), _) => r,
            (None, [one]) => one.to_string(),
            (None, []) => return "belay: refused. This fleet has no attached role; add one with run = \"attached\".".into(),
            (None, _) => return "belay: refused. This fleet has several attached roles; pick one with --belay-role.".into(),
        };
        let recs = self.apply(FleetEvent::Attached {
            role,
            name,
            session: format!("pid-{pid}"),
        });
        let answer = recs.iter().find_map(|r| match &r.body {
            FleetBody::Command {
                command: Cmd::Answer { text, .. },
                ..
            } => Some(text.clone()),
            _ => None,
        });
        let Some(slot) = answer.clone().filter(|a| !a.starts_with("belay: refused")) else {
            return answer.unwrap_or_else(|| "belay: refused.".into());
        };
        let cwd = PathBuf::from(cwd);
        self.worktrees.insert(slot.clone(), cwd.clone());
        self.attached.insert(slot.clone());
        self.procs.insert(
            slot.clone(),
            Proc {
                pid,
                stdin: None,
                turns: 0,
                exited: Arc::new(AtomicBool::new(false)),
                worktree: cwd.clone(),
            },
        );
        serde_json::json!({"slot": slot, "rules": self.rules_for(&slot, &cwd)}).to_string()
    }

    /// A session's messages (its inbox file), or the fleet's inbox for the user.
    pub(super) fn inbox_of(&self, by: &Actor) -> String {
        let path = match by {
            Actor::Slot { id } => self
                .dir
                .join("slots")
                .join(format!("{}.inbox.jsonl", file_name(id))),
            _ => self.dir.join("inbox.jsonl"),
        };
        render_inbox(&path)
    }

    /// Where a slot works, without making anything: its worktree if it exists, or the
    /// checkout (G1: a restarted hub, a check after its session ended).
    pub(super) fn slot_dir(&self, slot: &str) -> Option<PathBuf> {
        self.model.role_of(slot)?;
        let Some(name) = self.model.worktree_name(slot) else {
            return Some(PathBuf::from(&self.meta.cwd));
        };
        let p = self.dir.join("worktrees").join(file_name(&name));
        if p.exists() {
            Some(p)
        } else if self.meta.base.is_none() {
            Some(PathBuf::from(&self.meta.cwd))
        } else {
            None
        }
    }

    /// The build-cache variables for a session or check in `cwd` (P0.4). Only a belay
    /// worktree gets them: the user's own checkout (a shared workspace) keeps its
    /// `target/`. Keyed by the worktree's name, so no two running sessions share one.
    pub(super) fn build_env(&self, cwd: &Path) -> Vec<(String, String)> {
        let (Some(repo), Some(key)) = (
            &self.meta.repo,
            cwd.strip_prefix(self.dir.join("worktrees"))
                .ok()
                .and_then(|r| r.to_str())
                .filter(|r| !r.is_empty()),
        ) else {
            return Vec::new();
        };
        let (mode, env) = &self.build;
        let cache = crate::disk::cache_root().join(crate::disk::repo_key(Path::new(repo)));
        crate::disk::cargo_env(*mode, &cache, cwd, key, env)
    }

    /// A session's write rules (F5c), as JSON for `BELAY_FLEET_RULES`.
    pub(super) fn rules_for(&self, slot: &str, cwd: &Path) -> String {
        let role = self.model.role_of(slot);
        let rules = belay_core::fleet::scope::RulesFile {
            root: cwd.display().to_string(),
            slot: slot.to_string(),
            may_write: role.map(|r| r.may_write.clone()).unwrap_or_default(),
            may_not_write: role.map(|r| r.may_not_write.clone()).unwrap_or_default(),
            claims: Some(self.dir.join("claims.json").display().to_string()),
            inbox: Some(
                self.dir
                    .join("slots")
                    .join(format!("{}.inbox.jsonl", file_name(slot)))
                    .display()
                    .to_string(),
            ),
        };
        serde_json::to_string(&rules).unwrap_or_default()
    }

    /// The scopes of tasks being worked on, for sessions in the same worktree (F5c).
    /// Written only when they change, by rename, so a reader never sees half a file.
    pub(super) fn write_claims(&mut self) {
        use belay_core::fleet::model::TaskState;
        let claims: Vec<belay_core::fleet::scope::ClaimFile> = self
            .model
            .tasks()
            .filter(|t| {
                matches!(
                    t.state,
                    TaskState::Working | TaskState::Checking | TaskState::Bound
                )
            })
            .filter(|t| !t.scope.is_empty())
            .filter_map(|t| {
                let slot = t.slot.clone()?;
                let worktree = self.worktrees.get(&slot)?.display().to_string();
                Some(belay_core::fleet::scope::ClaimFile {
                    slot,
                    task: t.id.clone(),
                    worktree,
                    scope: t.scope.clone(),
                })
            })
            .collect();
        let text = serde_json::to_string(&claims).unwrap_or_default();
        if self.claims_text.as_deref() == Some(text.as_str()) {
            return;
        }
        let tmp = self.dir.join("claims.json.t");
        if fs::write(&tmp, &text).is_ok() && fs::rename(&tmp, self.dir.join("claims.json")).is_ok()
        {
            self.claims_text = Some(text);
        }
    }

    pub(super) fn status(&self) -> String {
        let mut out = render_status(&self.meta, &self.model, &self.procs, true);
        out.push_str(&render_tasks(&self.model));
        out
    }
}

/// A check that runs longer is stopped and fails.
pub(super) const CHECK_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Output kept from a check, at most.
pub(super) const CHECK_OUTPUT_BYTES: usize = 4 << 20;

/// Run `command` with `sh` in `dir`, stdout and stderr together. Its exit code (None:
/// killed or timed out) and output.
pub(super) fn run_with_timeout(
    dir: &Path,
    command: &str,
    env: &[(String, String)],
    timeout: Duration,
) -> (Option<i32>, String) {
    let child = Command::new("sh")
        .arg("-c")
        .arg(format!("exec 2>&1\n{command}"))
        .current_dir(dir)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (None, format!("could not run the check: {e}")),
    };
    let out = child.stdout.take();
    let reader = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut out) = out {
            let _ = (&mut out)
                .take(CHECK_OUTPUT_BYTES as u64)
                .read_to_end(&mut buf);
            // Past the cap: keep reading, so the check is not blocked or killed by SIGPIPE.
            let _ = std::io::copy(&mut out, &mut std::io::sink());
        }
        String::from_utf8_lossy(&buf).to_string()
    });
    let start = std::time::Instant::now();
    let mut signal = None;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                signal = std::os::unix::process::ExitStatusExt::signal(&status);
                // Anything the check left running holds the pipe open and would block
                // the reader, and every later check behind the lock.
                kill(-(child.id() as i32), libc::SIGKILL);
                break status.code();
            }
            Ok(None) if start.elapsed() < timeout => thread::sleep(Duration::from_millis(100)),
            _ => {
                kill(-(child.id() as i32), libc::SIGKILL);
                let _ = child.wait();
                break None;
            }
        }
    };
    let mut output = reader.join().unwrap_or_default();
    if code.is_none() && start.elapsed() >= timeout {
        output.push_str(&format!(
            "\n[belay: the check ran over {} min and was stopped]",
            timeout.as_secs() / 60
        ));
    }
    if code.is_none() && start.elapsed() < timeout {
        if let Some(sig) = signal {
            output.push_str(&format!("\n[belay: the check was killed by signal {sig}]"));
        }
    }
    (code, output)
}

/// What a session needs from a failed check: the failures belay's parsers find, or the
/// end of the output.
pub(super) fn excerpt(output: &str) -> String {
    let failures = belay_core::perception::extract_failures(output);
    if !failures.is_empty() {
        return failures
            .iter()
            .take(10)
            .map(|f| match &f.test {
                Some(t) => format!("{t}: {}", f.message),
                None => f.message.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    let lines: Vec<&str> = output.lines().collect();
    lines[lines.len().saturating_sub(30)..].join("\n")
}

/// Lines a worktree changed (added plus removed): from the task's base tree when the
/// audit recorded one, else from the worktree's HEAD; new files count whole (G9).
pub(super) fn diff_size(wt: &Path, base_tree: Option<&str>) -> Option<u64> {
    let from = base_tree.unwrap_or("HEAD");
    let numstat = git_out(wt, &["diff", "--numstat", from]).unwrap_or_default();
    let mut n: u64 = numstat
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            Some(f.next()?.parse::<u64>().unwrap_or(0) + f.next()?.parse::<u64>().unwrap_or(0))
        })
        .sum();
    let new = untracked(wt);
    for f in &new {
        n += fs::read_to_string(wt.join(f)).map_or(0, |t| t.lines().count() as u64);
    }
    Some(n)
}

/// The audit of a task's diff (experimental): what may weaken tests or checks.
pub(super) fn audit_flags(root: &Path, from: &str, scratch: &Path) -> Vec<String> {
    let Ok(to) = crate::audit::worktree_tree(root, scratch) else {
        return Vec::new();
    };
    let policy = crate::config::load_policy(root).0;
    let Ok(report) = crate::audit::run(root, from, &to, &|p| policy.audit_ignores(p)) else {
        return Vec::new();
    };
    let mut flags: Vec<String> = report
        .tests
        .iter()
        .filter(|t| !t.weakened.is_empty())
        .map(|t| format!("{}: {}", t.path, t.weakened.join(", ")))
        .collect();
    flags.extend(
        report
            .config
            .iter()
            .filter(|c| !c.weaker.is_empty())
            .map(|c| format!("{}: {}", c.path, c.weaker.join(", "))),
    );
    flags
}

/// Copy a slot's output to its log, and tell the hub about activity and turn ends.
pub(super) fn read_output(out: impl Read, mut log: File, slot: &str, tx: &Sender<Msg>) {
    let mut last = 0;
    let mut reader = BufReader::new(out);
    let mut raw = Vec::new();
    loop {
        raw.clear();
        // Bytes, not `lines()`: a line that is not UTF-8 must not end the reading.
        if !matches!(reader.read_until(b'\n', &mut raw), Ok(n) if n > 0) {
            break;
        }
        let line = String::from_utf8_lossy(&raw);
        let line = line.trim_end_matches(['\n', '\r']);
        let _ = writeln!(log, "{line}");
        let kind = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(String::from));
        let sent = if kind.as_deref() == Some("result") {
            if let Some(e) = usage_of(line, slot) {
                let _ = tx.send(Msg::Event(e));
            }
            if let Some(e) = limit_of(line, slot, now_ms()) {
                let _ = tx.send(Msg::Event(e));
            }
            tx.send(Msg::TurnEnded(slot.to_string()))
        } else if now_ms().saturating_sub(last) >= ACTIVITY_EVERY_MS {
            last = now_ms();
            tx.send(Msg::Event(FleetEvent::Activity {
                slot: slot.to_string(),
                turn_ended: false,
            }))
        } else {
            Ok(())
        };
        if sent.is_err() {
            return;
        }
    }
}

/// A restarted hub stops the sessions the old one left: each slot's process group,
/// from `slots/<slot>.pid`, is sent SIGTERM, then SIGKILL after a second.
pub(super) fn stop_old_sessions(dir: &Path) {
    let mut groups = Vec::new();
    for e in fs::read_dir(dir.join("slots"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "pid") {
            if let Some(pid) = fs::read_to_string(&p)
                .ok()
                .and_then(|t| t.trim().parse::<i32>().ok())
            {
                // SAFETY: signal 0 only checks that the group exists. The pid may have
                // been reused since: signal it only if it is still a `belay claude`.
                if pid > 1 && unsafe { libc::kill(-pid, 0) } == 0 && is_belay_claude(pid) {
                    kill(-pid, libc::SIGTERM);
                    groups.push(pid);
                }
            }
            let _ = fs::remove_file(&p);
        }
    }
    if !groups.is_empty() {
        thread::sleep(Duration::from_secs(1));
        for pid in groups {
            kill(-pid, libc::SIGKILL);
        }
    }
}

pub(super) fn is_belay_claude(pid: i32) -> bool {
    let cmd = fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
        .or_else(|_| {
            Command::new("ps")
                .args(["-o", "command=", "-p", &pid.to_string()])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        })
        .unwrap_or_default();
    cmd.contains("belay") && cmd.contains("claude") && cmd.contains("stream-json")
}

/// The pid of the process at the other end of a Unix socket.
#[cfg(target_os = "linux")]
pub(super) fn peer_pid(s: &UnixStream) -> Option<i32> {
    use std::os::unix::io::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `cred`, a ucred.
    let rc = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (rc == 0 && cred.pid > 0).then_some(cred.pid)
}

#[cfg(target_os = "macos")]
pub(super) fn peer_pid(s: &UnixStream) -> Option<i32> {
    use std::os::unix::io::AsRawFd;
    // <sys/un.h>: SOL_LOCAL 0, LOCAL_PEERPID 0x002.
    const SOL_LOCAL: libc::c_int = 0;
    const LOCAL_PEERPID: libc::c_int = 0x002;
    let mut pid: libc::pid_t = 0;
    let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: getsockopt writes at most `len` bytes into `pid`, a pid_t.
    let rc = unsafe {
        libc::getsockopt(
            s.as_raw_fd(),
            SOL_LOCAL,
            LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut len,
        )
    };
    (rc == 0 && pid > 0).then_some(pid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn peer_pid(_: &UnixStream) -> Option<i32> {
    None
}

/// `pid` and its parents, up to init. A process that left its tree (a double fork)
/// is not found: the agent-side check (`fleet_bypass`) covers the plain cases.
pub(super) fn ancestors(pid: i32) -> Vec<i32> {
    let mut out = vec![pid];
    let mut at = pid;
    for _ in 0..64 {
        let Some(parent) = parent_of(at).filter(|&p| p > 1) else {
            break;
        };
        out.push(parent);
        at = parent;
    }
    out
}

pub(super) fn parent_of(pid: i32) -> Option<i32> {
    if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) {
        // pid (comm) state ppid …; comm may hold spaces and parens.
        return stat
            .rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok();
    }
    let out = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// A result line's usage: the turn's tokens (prompt, cache, and output) and the
/// session's cost so far (F5d).
pub(super) fn usage_of(line: &str, slot: &str) -> Option<FleetEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let u = v.get("usage")?;
    let n = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let tokens = n("input_tokens")
        + n("cache_creation_input_tokens")
        + n("cache_read_input_tokens")
        + n("output_tokens");
    Some(FleetEvent::Usage {
        slot: slot.to_string(),
        tokens,
        cost_usd: v
            .get("total_cost_usd")
            .and_then(|c| c.as_f64())
            .unwrap_or(0.0),
    })
}

/// A result that says the plan's usage limit was hit (the bench runner's signal), and
/// when it resets if it says so, read at `now`.
pub(crate) fn limit_of(line: &str, slot: &str, now: u64) -> Option<FleetEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let text = v.get("result").and_then(|r| r.as_str()).unwrap_or("");
    let lower = text.to_lowercase();
    let is_error = v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false);
    // "hit your 5-hour limit", "hit your weekly limit": the window's name varies.
    let hit_your_limit = lower.split("hit your ").skip(1).any(|rest| {
        rest.split_whitespace()
            .take(2)
            .any(|w| w.starts_with("limit"))
    });
    let named = lower.contains("usage limit") || lower.contains("session limit") || hit_your_limit;
    // Not an error: only a short line that says the limit was hit ("…usage limit
    // reached", "You've hit your session limit"), not an answer that mentions one.
    let reached = lower.contains("limit reached") || lower.contains("hit your");
    let hit = named && (is_error || (lower.len() < 300 && reached));
    hit.then(|| FleetEvent::UsageLimit {
        slot: slot.to_string(),
        text: text.to_string(),
        resets_at_ms: supervise::reset_at(text, now),
    })
}

pub(super) fn kill(pid: i32, sig: libc::c_int) {
    // SAFETY: kill has no memory effects; a stale pid is the only risk, and the
    // caller checks `exited` first.
    unsafe {
        libc::kill(pid, sig);
    }
}

/// Slot ids hold `/` in teams; a file or directory name may not.
pub(super) fn file_name(slot: &str) -> String {
    slot.replace('/', "__")
}

/// A slot's branch. Nested slots are spelled flat (`sub-1__worker-1`): git cannot have
/// `belay/F/sub-1` and `belay/F/sub-1/worker-1` at once.
pub(super) fn branch_name(fleet: &str, name: &str) -> String {
    format!("belay/{fleet}/{}", file_name(name))
}

/// New files (untracked, not ignored), exact: `-z`, because git quotes a name with a
/// quote, tab, or backslash otherwise, and `git_out` trims a name's leading space.
pub(super) fn untracked(dir: &Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["ls-files", "-z", "--others", "--exclude-standard"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => o
            .stdout
            .split(|b| *b == 0)
            .filter(|n| !n.is_empty())
            .map(|n| String::from_utf8_lossy(n).to_string())
            .collect(),
        _ => Vec::new(),
    }
}

pub(super) fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// A random version-4 UUID, for `claude --session-id`.
pub(super) fn uuid_v4() -> Result<String> {
    let mut b = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

pub(super) fn inbox(dir: &Path, text: &str) {
    let line = serde_json::json!({"ts_ms": now_ms(), "text": text});
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("inbox.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}
