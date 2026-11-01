//! `belay sandbox claude` and `belay sandbox task`: an agent in a sandbox, supervised by
//! a belay session outside. The launcher starts the daemon here; the agent runs inside
//! through the backend, and its hooks reach the daemon through the bound socket. The
//! agent's program and headless output come from its connector: nothing here names one.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use belay_core::Event;
use serde::{Deserialize, Serialize};

use super::{
    backend, caller_socket, exec, git, here, landed_line, layout, limits, log, open_or_create,
    reason, remove_work, root, tell_session, Io, Limits, Meta, Outcome, Spec,
};
use crate::connector::{self, Connector};
use crate::launcher::{self, Sandboxed};
use crate::{audit, config, paths};

/// The supervision and the spec of an agent run in `meta`'s sandbox.
fn start(
    meta: &Meta,
    cfg: &config::Config,
    rel: &Path,
    connector: &dyn Connector,
    args: Vec<String>,
) -> Result<(Sandboxed, Spec)> {
    let sup = launcher::sandboxed(connector.agent(), args)?;
    let spec = layout(
        meta,
        cfg,
        &connector.sandbox_files(),
        rel,
        Some(&sup.sock),
        sup.argv.clone(),
    );
    let mut spec = match spec {
        Ok(s) => s,
        Err(e) => {
            sup.finish();
            return Err(e);
        }
    };
    for p in &sup.ro {
        spec.ro.push((p.clone(), p.clone()));
    }
    spec.env
        .retain(|(k, _)| !sup.env.iter().any(|(s, _)| s == k));
    spec.env.extend(sup.env.iter().cloned());
    spec.env.sort();
    let started = Event::SandboxStarted {
        id: meta.id.clone(),
        branch: meta.branch.clone(),
    };
    // The session inside shows the sandbox on its card; a calling session records it.
    tell_session(Some(&sup.sock), started.clone());
    tell_session(caller_socket().as_deref(), started);
    log(meta, "started", &format!("agent session {}", sup.id()));
    Ok((sup, spec))
}

/// Record the end of the run in the sandbox's log and both sessions, then stop the
/// supervision (the session's summary prints here).
fn stop(meta: &Meta, sup: Sandboxed, out: &Result<Outcome>) {
    if let Ok(Outcome {
        stopped: Some(why), ..
    }) = out
    {
        log(meta, "limit", why);
        eprintln!("belay: stopped sandbox {}: {why}", meta.id);
    }
    let reason = reason(out, "the agent");
    log(meta, "stopped", &reason);
    let stopped = Event::SandboxStopped {
        id: meta.id.clone(),
        reason,
    };
    tell_session(Some(&sup.sock), stopped.clone());
    tell_session(caller_socket().as_deref(), stopped);
    sup.finish();
}

/// `belay sandbox claude`: the user's whole session, in a sandbox that is kept (a later
/// `--id S --resume <conversation>` goes on in it).
pub fn claude(id: Option<String>, args: Vec<String>) -> Result<i32> {
    paths::users_only("`belay sandbox claude`")?;
    let cfg = config::load();
    let backend = backend()?;
    let connector = connector::for_agent(connector::DEFAULT_AGENT)?;
    let (repo, rel) = here()?;
    let (meta, _) = open_or_create(&repo, id)?;
    let (sup, spec) = start(&meta, &cfg, &rel, &*connector, args)?;
    let session = sup.id().to_string();
    eprintln!("belay: sandbox {}, branch {}", meta.id, meta.branch);
    // [Verify] the agent's terminal UI works with no controlling terminal: bwrap's
    // --new-session (against TIOCSTI injection) leaves it only stdin in raw mode.
    let out = exec(
        &meta,
        &*backend,
        &spec,
        Limits::from_config(&cfg),
        Io::Inherit,
    );
    stop(&meta, sup, &out);
    let message = format!("sandbox {}: agent session {session}", meta.id);
    let landed = git::land(&meta, &*backend, &message);
    eprintln!("belay: {}", landed_line(&meta, &landed));
    let code = out?.code;
    landed?;
    Ok(code)
}

pub struct TaskArgs {
    pub prompt: String,
    pub id: Option<String>,
    pub resume: Option<String>,
    pub model: Option<String>,
    pub keep: bool,
}

/// One line of `<sandboxes>/tasks.jsonl`: who started a task, when.
#[derive(Debug, Serialize, Deserialize)]
struct Started {
    at_ms: u64,
    by: String,
    id: String,
}

const HOUR_MS: u64 = 3_600_000;

/// Who asks: the calling belay session, or the user, or an agent outside belay.
fn caller(by_agent: bool) -> String {
    match paths::env_path("BELAY_SESSION_DIR").and_then(|d| Some(d.file_name()?.to_owned())) {
        Some(name) => name.to_string_lossy().into_owned(),
        None if by_agent => "agent".into(),
        None => "user".into(),
    }
}

/// How many tasks `by` started in the hour before `now_ms`.
fn started_within_hour(log: &str, by: &str, now_ms: u64) -> usize {
    log.lines()
        .filter_map(|l| serde_json::from_str::<Started>(l).ok())
        .filter(|s| s.by == by && s.at_ms + HOUR_MS > now_ms)
        .count()
}

fn tasks_file() -> PathBuf {
    root().join("tasks.jsonl")
}

fn record_start(by: &str, id: &str) -> Result<()> {
    paths::ensure_private_dir(&root())?;
    let line = serde_json::to_string(&Started {
        at_ms: paths::now_ms(),
        by: by.into(),
        id: id.into(),
    })?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(tasks_file())?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// Counts an agent's tokens from its headless output and stops it past the budget.
struct Meter<'a> {
    connector: &'a dyn Connector,
    budget: u64,
    calls: BTreeSet<String>,
    used: u64,
    result: Option<String>,
}

impl Meter<'_> {
    fn line(&mut self, line: &str) -> Option<String> {
        let l = self.connector.headless_line(line)?;
        if l.result.is_some() {
            self.result = l.result;
        }
        if l.call.is_none_or(|c| self.calls.insert(c)) {
            self.used += l.tokens;
        }
        (self.used > self.budget).then(|| {
            format!(
                "sandbox_task_tokens: the agent used {} tokens, past the budget of {}",
                self.used,
                limits::fmt_count(self.budget)
            )
        })
    }
}

/// Copy the conversation a task resumes into the sandbox's private home.
fn copy_conversation(
    meta: &Meta,
    connector: &dyn Connector,
    conversation: &str,
    rel: &Path,
) -> Result<()> {
    let cwd = Some(meta.work().join(rel))
        .filter(|c| c.is_dir())
        .unwrap_or_else(|| meta.work());
    let files = connector.conversation_files(conversation, &cwd);
    if files.is_empty() {
        bail!("belay: refused: no conversation `{conversation}` here to start from");
    }
    for (src, rel) in files {
        let dst = meta.home().join(&rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(&src, &dst).with_context(|| format!("copy {}", src.display()))?;
    }
    Ok(())
}

/// `belay sandbox task`: a headless follow-up in a sandbox, for the user or an agent.
pub fn task(a: TaskArgs) -> Result<i32> {
    let cfg = config::load();
    let by_agent = !paths::users_terminal();
    if by_agent && !cfg.sandbox_agent {
        bail!("belay: refused: `sandbox_agent = off` in belay's config: only the user starts a sandbox");
    }
    let by = caller(by_agent);
    if by_agent {
        let log = fs::read_to_string(tasks_file()).unwrap_or_default();
        let n = started_within_hour(&log, &by, paths::now_ms());
        if n as u64 >= cfg.sandbox_tasks_per_hour {
            bail!(
                "belay: refused: this session started {n} sandbox tasks in the last hour, the most `sandbox_tasks_per_hour` allows"
            );
        }
    }
    let backend = backend()?;
    let connector = connector::for_agent(connector::DEFAULT_AGENT)?;
    let args = connector.headless_argv(&a.prompt, a.resume.as_deref(), a.model.as_deref())?;
    let (repo, rel) = here()?;
    let (meta, created) = open_or_create(&repo, a.id)?;
    let prepared = (|| -> Result<(Sandboxed, Spec)> {
        record_start(&by, &meta.id)?;
        if let Some(c) = &a.resume {
            copy_conversation(&meta, &*connector, c, &rel)?;
        }
        log(&meta, "task", &a.prompt);
        start(&meta, &cfg, &rel, &*connector, args)
    })();
    let (sup, spec) = match prepared {
        Ok(p) => p,
        Err(e) => {
            if created {
                let _ = remove_work(&meta);
            }
            return Err(e);
        }
    };
    let mut meter = Meter {
        connector: &*connector,
        budget: cfg.sandbox_task_tokens,
        calls: BTreeSet::new(),
        used: 0,
        result: None,
    };
    let mut each = |line: &str| meter.line(line);
    let out = exec(
        &meta,
        &*backend,
        &spec,
        Limits::from_config(&cfg),
        Io::Lines(&mut each),
    );
    stop(&meta, sup, &out);
    let prompt = belay_core::text::truncate_chars(&a.prompt, 200);
    let landed = git::land(&meta, &*backend, &format!("sandbox {}: {prompt}", meta.id));
    print!("{}", task_report(&meta, &out, &meter, &landed));
    if created && !a.keep {
        remove_work(&meta)?;
    }
    let code = out?.code;
    landed?;
    Ok(code)
}

/// What a task did, short: for the user's terminal or the agent that asked.
fn task_report(
    meta: &Meta,
    out: &Result<Outcome>,
    meter: &Meter,
    landed: &Result<Vec<String>>,
) -> String {
    let id = &meta.id;
    let mut s = format!("sandbox task {id}: {}\n", reason(out, "the agent"));
    let result = meter.result.as_deref().unwrap_or("(no final message)");
    s.push_str(&format!("result: {}\n", result.trim()));
    match landed {
        Ok(f) if f.is_empty() => {
            s.push_str(&format!("branch: {} (nothing changed)\n", meta.branch))
        }
        Ok(f) => s.push_str(&format!(
            "branch: {} ({} file(s): {})\n",
            meta.branch,
            f.len(),
            f.join(", ")
        )),
        Err(e) => s.push_str(&format!(
            "branch: {} (could not bring the work out: {e:#})\n",
            meta.branch
        )),
    }
    if let Ok(r) = audit::run(&meta.repo, &meta.base, &meta.branch, &|_| false) {
        s.push_str(&format!("audit: {}\n", r.one_line()));
    }
    s.push_str(&format!(
        "tokens: {} of {}\n",
        meter.used,
        limits::fmt_count(meter.budget)
    ));
    let limits: Vec<String> = super::events(meta)
        .into_iter()
        .filter(|e| e.kind == "limit")
        .map(|e| e.text)
        .collect();
    let stopped = out.as_ref().ok().and_then(|o| o.stopped.clone());
    match stopped {
        Some(why) => s.push_str(&format!("limit: {why}\n")),
        None if limits.is_empty() => s.push_str("limits: none reached\n"),
        None => s.push_str(&format!("limits (earlier runs): {}\n", limits.join("; "))),
    }
    s.push_str(&format!(
        "`belay sandbox show {id}` · `belay sandbox diff {id}` · `git merge {}`\n",
        meta.branch
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hourly_limit_counts_one_caller_in_the_last_hour() {
        let now = 10 * HOUR_MS;
        let line = |at: u64, by: &str| format!(r#"{{"at_ms":{at},"by":"{by}","id":"x"}}"#);
        let log = [
            line(now - 10, "s1"),
            line(now - HOUR_MS + 1, "s1"),
            line(now - HOUR_MS, "s1"),
            line(now - 5, "s2"),
            "junk".to_string(),
        ]
        .join("\n");
        assert_eq!(started_within_hour(&log, "s1", now), 2);
        assert_eq!(started_within_hour(&log, "s2", now), 1);
        assert_eq!(started_within_hour(&log, "s3", now), 0);
    }

    #[test]
    fn the_meter_stops_past_the_budget_and_keeps_the_result() {
        let c = connector::for_agent(connector::DEFAULT_AGENT).unwrap();
        let mut m = Meter {
            connector: &*c,
            budget: 8000,
            calls: BTreeSet::new(),
            used: 0,
            result: None,
        };
        let text = include_str!("../../../../fixtures/claude_code/headless_stream.jsonl");
        let stops: Vec<String> = text.lines().filter_map(|l| m.line(l)).collect();
        assert!(stops[0].starts_with("sandbox_task_tokens"), "{stops:?}");
        assert_eq!(m.used, 8606 + 152);
        assert_eq!(m.result.as_deref(), Some("Done: a.txt says b."));
    }
}
