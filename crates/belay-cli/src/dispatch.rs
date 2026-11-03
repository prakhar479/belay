//! Dispatch: runs the parsed subcommand.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::cli::*;
use crate::*;

fn pause(feature: Option<String>, session: Option<String>, resume: bool) -> Result<i32> {
    let dir = sessions::resolve(session.as_deref(), true)?;
    let req = ipc::Request::Pause {
        feature,
        resume,
        token: paths::read_token(&dir),
    };
    let reply = ipc::send(&paths::socket_path(&dir), &req, Duration::from_secs(1))
        .with_context(|| format!("session {} is not live", dir.display()))?;
    let said = reply.message.unwrap_or_default();
    println!("{said}");
    Ok(i32::from(
        !(said.starts_with("Paused") || said.starts_with("Resumed")),
    ))
}

fn fleet_cmd(c: FleetCmd) -> Result<i32> {
    let text = match c {
        FleetCmd::Up { file, preset, mode } => {
            if mode.is_some() {
                eprintln!("belay: --mode is ignored: belay has no modes now. Fleet sessions run with guard = deny and commons = on.");
            }
            let preset = match preset {
                Some(p) => Some(
                    serde_json::from_value(serde_json::Value::String(p.clone()))
                        .map_err(|_| anyhow::anyhow!("no preset `{p}`"))?,
                ),
                None => None,
            };
            let id = hub::up(file.as_deref(), preset)?;
            format!(
                "fleet {id} is up.\n{}",
                hub::status(Some(&id)).unwrap_or_default()
            )
        }
        FleetCmd::Status { fleet } => hub::status(fleet.as_deref())?,
        FleetCmd::Stop { slot, fleet } => {
            hub::ask(fleet.as_deref(), hub::HubRequest::Stop { slot })?
        }
        FleetCmd::Spawn { role, fleet } => {
            hub::ask(fleet.as_deref(), hub::HubRequest::Spawn { role })?
        }
        FleetCmd::Scale { role, n, fleet } => {
            hub::ask(fleet.as_deref(), hub::HubRequest::Scale { role, n })?
        }
        FleetCmd::Down { fleet } => hub::down(fleet.as_deref())?,
        FleetCmd::Report { fleet } => hub::report(fleet.as_deref())?,
        FleetCmd::Merge { yes, fleet } => hub::merge(fleet.as_deref(), yes)?,
        FleetCmd::Pause { fleet } => hub::ask(fleet.as_deref(), hub::HubRequest::Pause)?,
        FleetCmd::Resume { fleet } => hub::ask(fleet.as_deref(), hub::HubRequest::Resume)?,
        FleetCmd::Prune { yes, branches } => {
            if yes {
                paths::users_only("`belay fleet prune --yes`")?;
            }
            hub::prune(yes, branches)?
        }
        FleetCmd::Hub { dir, resume } => {
            hub::run_hub(&dir, resume)?;
            return Ok(0);
        }
        FleetCmd::Restart { fleet } => hub::restart(fleet.as_deref())?,
        FleetCmd::Replay { fleet, file } => hub::replay_cmd(fleet.as_deref(), file.as_deref())?,
    };
    println!("{}", text.trim_end());
    Ok(0)
}

/// `belay config` actions.
pub(crate) fn run(cli: Cli) -> Result<i32> {
    match cli.cmd {
        Cmd::Claude { mode, args } => {
            if mode.is_some() {
                launcher::warn_mode_flag();
            }
            launcher::run_claude(args)
        }
        Cmd::Launch { agent, args } => launcher::launch(&agent, args),
        Cmd::Codex { .. } => bail!("`belay codex` is not built yet"),
        Cmd::Init { agent } => bail!("`belay init {agent}` is not built yet"),
        Cmd::Sandbox { cmd } => sandbox::cli(cmd),
        Cmd::Bg { cmd } => bg::cli(cmd),
        Cmd::Undo { id, session, yes } => {
            let dir = sessions::resolve(session.as_deref(), false)?;
            undo::run(&dir, id.as_deref(), yes)
        }
        Cmd::Mode { .. } => {
            println!("belay has no modes now: it acts within what the agent supports. To stop it, run `belay pause` (or `belay pause guard` for one feature); `belay resume` undoes it.");
            Ok(1)
        }
        Cmd::Pause { feature, session } => pause(feature, session, false),
        Cmd::Resume { feature, session } => pause(feature, session, true),
        Cmd::Allow { id, session } => {
            let dir = sessions::resolve(session.as_deref(), true)?;
            let req = ipc::Request::Allow {
                action: id,
                token: paths::read_token(&dir),
            };
            let reply = ipc::send(&paths::socket_path(&dir), &req, Duration::from_secs(1))
                .with_context(|| format!("session {} is not live", dir.display()))?;
            let said = reply.message.unwrap_or_default();
            println!("{said}");
            Ok(if said.starts_with("belay: refused") {
                1
            } else {
                0
            })
        }
        Cmd::Report { session, full } => {
            let dir = sessions::resolve(session.as_deref(), false)?;
            // A resumed session: each earlier run first, one header each (U8).
            let runs = store::read_chain(&dir)?;
            for (n, run) in runs.iter().enumerate() {
                let label = run
                    .dir
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                if runs.len() > 1 {
                    if n > 0 {
                        println!();
                    }
                    println!("Run {} of {} of this conversation:", n + 1, runs.len());
                }
                print!("{}", report::render(&label, &run.entries, true, full));
            }
            if let Some(a) = std::fs::read_to_string(dir.join("audit.json"))
                .ok()
                .and_then(|t| serde_json::from_str::<belay_core::audit::Report>(&t).ok())
            {
                println!(
                    "{}: {} (`belay audit --session`)",
                    audit::LABEL,
                    a.one_line()
                );
            }
            let entries: Vec<_> = runs.into_iter().flat_map(|r| r.entries).collect();
            print!(
                "{}",
                flags::report_lines(&entries, &store::read_flags(&dir), full)
            );
            Ok(0)
        }
        Cmd::Brief {
            session,
            markdown,
            json,
        } => {
            let format = match (markdown, json) {
                (true, _) => brief::Format::Markdown,
                (_, true) => brief::Format::Json,
                _ => brief::Format::Text,
            };
            brief::run(session.as_deref(), format)
        }
        Cmd::Show { id, session } => {
            let dir = sessions::resolve(session.as_deref(), false)?;
            // Output ids go on across resumed runs (U8): the newest run that has it.
            let runs = store::read_chain(&dir)?;
            let (run, hash) = runs
                .iter()
                .rev()
                .find_map(|r| Some((r, report::find_output_blob(&r.entries, &id)?)))
                .ok_or_else(|| anyhow::anyhow!("no output {id} in {}", dir.display()))?;
            print!("{}", store::read_blob(&run.dir, &hash)?);
            Ok(0)
        }
        Cmd::Flag { words, session } => {
            println!("{}", flags::flag(words, session.as_deref())?);
            Ok(0)
        }
        Cmd::Usage {
            today,
            week: _,
            since,
            by,
            json,
        } => {
            let since = match (today, since) {
                (true, _) => usage::Since::Today,
                (_, Some(d)) => usage::Since::Date(usage::parse_date(&d)?),
                _ => usage::Since::Week,
            };
            print!("{}", usage::run(since, by, json)?);
            Ok(0)
        }
        Cmd::Watch { session, verbose } => watch::run(session.as_deref(), verbose),
        Cmd::Status => {
            config::quiet();
            // A status line must never show an error: print nothing instead. Under the
            // launcher the session is in the environment; under the plugin, Claude Code
            // passes the session id on stdin.
            let dir = std::env::var_os("BELAY_SESSION_DIR")
                .map(PathBuf::from)
                .or_else(|| {
                    use std::io::{IsTerminal, Read};
                    if std::io::stdin().is_terminal() {
                        return None;
                    }
                    let mut input = String::new();
                    std::io::stdin().read_to_string(&mut input).ok()?;
                    let v: serde_json::Value = serde_json::from_str(&input).ok()?;
                    paths::conversation_dir(v.get("session_id")?.as_str()?)
                });
            if let Some(dir) = dir {
                if let Ok(entries) = store::read_entries(&dir) {
                    let mut line = report::status_line(&entries);
                    line.push_str(&report::budget_segment());
                    // In a fleet session: messages waiting for the next hook (G6).
                    let inbox = std::env::var("BELAY_FLEET_RULES")
                        .ok()
                        .and_then(|r| {
                            serde_json::from_str::<belay_core::fleet::scope::RulesFile>(&r).ok()
                        })
                        .and_then(|r| r.inbox);
                    if let Some(inbox) = inbox {
                        let n = hub::unread_for_session(std::path::Path::new(&inbox), &entries);
                        if n > 0 {
                            line.push_str(&format!(
                                " · {n} new fleet message{}",
                                if n == 1 { "" } else { "s" }
                            ));
                        }
                    }
                    println!("{line}");
                }
            }
            Ok(0)
        }
        Cmd::Audit {
            range,
            format,
            session,
        } => {
            let (report, title) = match session {
                Some(spec) => {
                    let dir = sessions::resolve(Some(&spec), false)?;
                    let text = std::fs::read_to_string(dir.join("audit.json")).with_context(|| {
                        format!(
                            "no audit in session {} (set `experimental_audit = true` in the config)",
                            dir.display()
                        )
                    })?;
                    let id = dir.file_name().map(|n| n.to_string_lossy().to_string());
                    (
                        serde_json::from_str(&text)?,
                        format!("session {}", id.unwrap_or_default()),
                    )
                }
                None => {
                    let root = audit::repo_root(&std::env::current_dir()?)?;
                    let scratch = std::env::temp_dir();
                    let (from, to) = audit::resolve_range(&root, range.as_deref(), &scratch)?;
                    let title = range.unwrap_or_else(|| "HEAD..working tree".into());
                    let policy = config::load_policy(&root).0;
                    (
                        audit::run(&root, &from, &to, &|p| policy.audit_ignores(p))?,
                        title,
                    )
                }
            };
            match format {
                AuditFormat::Text => print!("{}", audit::render_text(&report, &title)),
                AuditFormat::Markdown => print!("{}", audit::render_markdown(&report, &title)),
                AuditFormat::Json => println!("{}", serde_json::to_string_pretty(&report)?),
            }
            Ok(0)
        }
        Cmd::Export {
            session,
            no_outputs,
            action,
            fixture,
        } => {
            let dir = sessions::resolve(session.as_deref(), false)?;
            if let (Some(id), Some(out)) = (action, fixture) {
                for f in export::fixture(&dir, &id, &out)? {
                    println!("wrote {}", f.display());
                }
                println!("{}", export::FIXTURE_WARNING);
            } else {
                println!("{}", export::export(&dir, !no_outputs)?);
            }
            Ok(0)
        }
        Cmd::Purge {
            session,
            all,
            older_than,
        } => {
            let root = paths::sessions_dir();
            let removed = match (session, all, older_than) {
                (Some(spec), _, _) => {
                    let dir = sessions::resolve(Some(&spec), false)?;
                    if !in_sessions_dir(&dir, &root) {
                        bail!(
                            "{} is not a stored belay session; not deleting it",
                            dir.display()
                        );
                    }
                    if sessions::is_live(&dir) {
                        bail!("session {} is live; end it first", dir.display());
                    }
                    undo::delete_refs(&dir);
                    std::fs::remove_dir_all(&dir)
                        .with_context(|| format!("remove {}", dir.display()))?;
                    vec![dir]
                }
                (None, true, _) => sessions::prune(&root, 0, u64::MAX / 2)?,
                (None, false, Some(days)) => sessions::prune(&root, days, paths::now_ms() / 1000)?,
                (None, false, None) => {
                    bail!("say what to purge: --session ID, --older-than DAYS, or --all")
                }
            };
            println!("{} sessions removed", removed.len());
            Ok(0)
        }
        Cmd::Install {
            parts,
            from,
            force,
            yes,
        } => install::install(&install::Opts {
            asked_statusline: parts.statusline,
            parts: parts.or(install::Parts {
                plugin: true,
                statusline: true,
                permissions: true,
                launcher: false,
            }),
            from,
            force,
            yes,
        }),
        Cmd::Uninstall { parts, yes } => install::uninstall(&install::Opts {
            parts: parts.or(install::Parts {
                plugin: true,
                statusline: true,
                permissions: true,
                launcher: false,
            }),
            from: None,
            force: false,
            yes,
            asked_statusline: parts.statusline,
        }),
        Cmd::Doctor { json } => {
            let (report, code) = doctor::run(json);
            print!("{report}");
            Ok(code)
        }
        Cmd::KeepAwake { for_, pid, cmd } => keepawake::run(for_.as_deref(), pid, &cmd),
        Cmd::Review => {
            print!("{}", flags::review(&paths::list_sessions()));
            Ok(0)
        }
        Cmd::Why { id, session } => {
            let dir = sessions::resolve(session.as_deref(), false)?;
            let entries = store::chain_entries(&store::read_chain(&dir)?);
            print!("{}", report::why_across(&entries, &id, &fleet::peer_entry)?);
            Ok(0)
        }
        Cmd::Card {
            task,
            note,
            name,
            offers,
            join,
            leave,
        } => {
            let Some(sock) = std::env::var_os("BELAY_SOCKET").map(PathBuf::from) else {
                println!(
                    "belay card works inside a belay session; `belay sessions` shows every card."
                );
                return Ok(1);
            };
            let req = ipc::Request::Card {
                name,
                task,
                note,
                offers,
                join,
                leave,
                handoff: None,
                from: my_session(),
            };
            let reply = ipc::send(&sock, &req, Duration::from_secs(2))?;
            let text = reply.message.unwrap_or_default();
            println!("{}", text.trim_end());
            Ok(if text.starts_with("belay: refused") {
                1
            } else {
                0
            })
        }
        Cmd::Fleet { cmd: None, all } => {
            let (fleets, sessions) = (hub::list(all), fleet::view_default());
            print!("{fleets}{sessions}");
            if fleets.is_empty() && sessions.starts_with("No live belay sessions") {
                println!("No fleets either. Start one with `belay fleet up`.");
            }
            print!("{}", hub::disk_use());
            Ok(0)
        }
        Cmd::Fleet { cmd: Some(c), .. } => fleet_cmd(c),
        Cmd::Msg {
            to,
            text,
            kind,
            fleet,
        } => {
            let said = send_message(fleet, to, kind, text)?;
            println!("{}", said.trim_end());
            Ok(if said.starts_with("belay: refused") {
                1
            } else {
                0
            })
        }
        Cmd::Note { text } => {
            println!("{}", learnstore::note(&text)?);
            Ok(0)
        }
        Cmd::Notes { verb, id, all } => {
            let action = match (verb.as_deref(), id.as_deref()) {
                (None, _) => None,
                (Some(v @ ("approve" | "drop")), Some(id)) => Some((v, id)),
                _ => anyhow::bail!("use `belay notes approve <id>` or `belay notes drop <id>`"),
            };
            let text = learnstore::notes(action, all)?;
            println!("{}", text.trim_end());
            Ok(0)
        }
        Cmd::Fixes { verb, all } => {
            match verb.as_deref() {
                None => print!("{}", learnstore::fixes(all)?),
                Some("forget") => println!("{}", learnstore::forget(all)?),
                Some(v) => anyhow::bail!("unknown `belay fixes {v}`; use `belay fixes forget`"),
            }
            Ok(0)
        }
        Cmd::Inbox { fleet } => {
            println!("{}", show_inbox(fleet)?.trim_end());
            Ok(0)
        }
        Cmd::Task { cmd, fleet } => {
            let verb = match cmd {
                TaskCmd::Add {
                    title,
                    body,
                    body_file,
                    depends,
                    scope,
                    role,
                    check,
                } => hub::TaskVerb::Add {
                    title,
                    body: match body_file {
                        Some(p) if p.as_os_str() == "-" => {
                            let mut b = String::new();
                            std::io::Read::read_to_string(&mut std::io::stdin(), &mut b)?;
                            b
                        }
                        Some(p) => std::fs::read_to_string(&p)
                            .with_context(|| format!("read {}", p.display()))?,
                        None => body,
                    },
                    depends_on: depends,
                    scope,
                    role,
                    check,
                },
                TaskCmd::Assign { task, to } => hub::TaskVerb::Assign { task, to },
                TaskCmd::Claim { task } => hub::TaskVerb::Claim { task },
                TaskCmd::Done { task, summary } => hub::TaskVerb::Done { task, summary },
                TaskCmd::Blocked { task, reason } => hub::TaskVerb::Blocked { task, reason },
                TaskCmd::Reject { task, reason } => hub::TaskVerb::Reject { task, reason },
                TaskCmd::Drop { task, reason } => hub::TaskVerb::Drop { task, reason },
                TaskCmd::Show { task } => hub::TaskVerb::Show { task },
                TaskCmd::List => hub::TaskVerb::List,
            };
            let text = hub::task(fleet.as_deref(), verb)?;
            println!("{}", text.trim_end());
            // A refusal is an error to the caller (an agent's script, or the user).
            Ok(if text.starts_with("belay: refused") {
                1
            } else {
                0
            })
        }
        Cmd::Retro {
            transcript,
            all,
            projects,
            include_tmp,
            full,
            summary,
        } => {
            match transcript {
                Some(t) if !all => print!("{}", retro::run(&t, full)?),
                _ => {
                    let root = projects.unwrap_or_else(paths::claude_projects_dir);
                    print!("{}", retro::run_all(&root, 5, include_tmp, summary)?);
                }
            }
            Ok(0)
        }
        Cmd::Sessions {
            prune: Some(days), ..
        } => {
            let now = paths::now_ms() / 1000;
            let removed = sessions::prune(&paths::sessions_dir(), days, now)?;
            for d in &removed {
                println!("removed {}", d.display());
            }
            println!("{} sessions removed", removed.len());
            Ok(0)
        }
        Cmd::Handoff { text } => {
            if let Some(text) = text {
                let Some(sock) = std::env::var_os("BELAY_SOCKET").map(PathBuf::from) else {
                    println!("belay handoff \"…\" works inside a belay session. For every session here, use `belay note`.");
                    return Ok(1);
                };
                let req = ipc::Request::Card {
                    name: None,
                    task: None,
                    note: None,
                    offers: None,
                    join: None,
                    leave: None,
                    handoff: Some(text),
                    from: my_session(),
                };
                let reply = ipc::send(&sock, &req, Duration::from_secs(2))?;
                let said = reply.message.unwrap_or_default();
                if said.starts_with("belay: refused") {
                    println!("{}", said.trim_end());
                    return Ok(1);
                }
                println!("Left for the next session in this repo (it sees it at start).");
                return Ok(0);
            }
            let cards = commons::read_cards(&paths::sessions_dir());
            let project = learnstore::project_of(&std::env::current_dir()?);
            let me = std::env::var("BELAY_SESSION_DIR")
                .ok()
                .and_then(|d| {
                    std::path::Path::new(&d)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                })
                .unwrap_or_default();
            match commons::latest_handoff(&cards, &me, &project, paths::now_ms()) {
                Some(h) => println!("{}", belay_core::commons::handoff_text(&h, paths::now_ms())),
                None => println!(
                    "No handoff for this repo: no session here ended in the last week with one."
                ),
            }
            Ok(0)
        }
        Cmd::Share {
            text,
            kind,
            tag,
            file,
            code_file,
        } => {
            let code = match code_file {
                Some(p) if p.as_os_str() == "-" => {
                    let mut b = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut b)?;
                    Some(b)
                }
                Some(p) => Some(
                    std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?,
                ),
                None => None,
            };
            let said = match std::env::var_os("BELAY_SOCKET").map(PathBuf::from) {
                Some(sock) => {
                    let req = ipc::Request::Share {
                        kind,
                        text,
                        tags: tag,
                        files: file,
                        code,
                        from: my_session(),
                    };
                    ipc::send(&sock, &req, Duration::from_secs(5))?
                        .message
                        .unwrap_or_default()
                }
                None => {
                    findings::share_as_user(kind.as_deref(), &text, &tag, &file, code.as_deref())?
                }
            };
            println!("{}", said.trim_end());
            Ok(if said.starts_with("belay: refused") {
                1
            } else {
                0
            })
        }
        Cmd::Ask { query, here, json } => {
            let said = match std::env::var_os("BELAY_SOCKET").map(PathBuf::from) {
                Some(sock) => {
                    let req = ipc::Request::Ask {
                        query,
                        here,
                        json,
                        from: my_session(),
                    };
                    ipc::send(&sock, &req, Duration::from_secs(5))?
                        .message
                        .unwrap_or_default()
                }
                None => findings::ask_as_user(&query, here, json)?,
            };
            print!("{said}");
            Ok(0)
        }
        Cmd::Mcp => mcp::run(),
        Cmd::Skill { action } => {
            let said = match action.unwrap_or(SkillCmd::List) {
                SkillCmd::Preview { url } => skillstore::preview(&url)?,
                SkillCmd::Add {
                    url,
                    scope,
                    name,
                    yes,
                } => skillstore::add(&url, scope.as_deref(), name.as_deref(), yes)?,
                SkillCmd::Approve { name, yes } => skillstore::approve(&name, yes)?,
                SkillCmd::Reject { name } => skillstore::reject(&name)?,
                SkillCmd::Remove { name, scope } => skillstore::remove(&name, scope.as_deref())?,
                SkillCmd::List => skillstore::list()?,
                SkillCmd::Update { name } => skillstore::update(name.as_deref())?,
            };
            println!("{}", said.trim_end());
            Ok(0)
        }
        Cmd::Config { action: None } => {
            config::show()?;
            Ok(0)
        }
        Cmd::Config {
            action: Some(ConfigCmd::Migrate { yes }),
        } => {
            config::migrate::run(yes)?;
            Ok(0)
        }
        Cmd::Memory { action } => {
            let said = match action.unwrap_or(MemoryCmd::List { all: false }) {
                MemoryCmd::List { all } => memorystore::list(all)?,
                MemoryCmd::Search { query } => memorystore::search(&query)?,
                MemoryCmd::Add {
                    text,
                    kind,
                    tag,
                    file,
                    scope,
                } => memorystore::add(&text, kind.as_deref(), &tag, &file, scope.as_deref())?,
                MemoryCmd::Show { id } => memorystore::show(&id)?,
                MemoryCmd::Drop { id } => memorystore::drop(&id)?,
                MemoryCmd::Approve { id } => memorystore::approve(&id)?,
                MemoryCmd::Migrate => memorystore::migrate()?,
            };
            println!("{}", said.trim_end());
            Ok(0)
        }
        Cmd::Findings {
            verb,
            ids,
            all,
            json,
        } => {
            match verb.as_deref() {
                None => print!("{}", findings::list(all, json)?),
                Some("drop") if !ids.is_empty() => println!("{}", findings::drop_ids(&ids)?),
                _ => anyhow::bail!("use `belay findings` or `belay findings drop f-N`"),
            }
            Ok(0)
        }
        Cmd::Sessions { history: true, .. } => {
            print!("{}", sessions::list(&paths::sessions_dir()));
            Ok(0)
        }
        Cmd::Sessions { all, json, .. } => {
            print!("{}", commons::board_default(all, json));
            Ok(0)
        }
        Cmd::Daemon {
            session_dir,
            parent_pid,
            agent,
            idle_exit_minutes,
            resume_from,
            resume_conversation,
            resume_picker,
        } => {
            let resume = match resume_from {
                Some(dir) => daemon::Resume::From {
                    dir,
                    conversation: resume_conversation,
                },
                None if resume_picker => daemon::Resume::Picker,
                None => daemon::Resume::No,
            };
            daemon::run(session_dir, parent_pid, &agent, idle_exit_minutes, resume)?;
            Ok(0)
        }
        Cmd::Hook { .. } | Cmd::Run { .. } => unreachable!("handled in main"),
    }
}

/// `belay msg`: the fleet's hub for fleet addresses, else the commons (C2).
fn send_message(
    fleet: Option<String>,
    to: String,
    kind: Option<String>,
    text: String,
) -> Result<String> {
    let my_fleet = std::env::var("BELAY_FLEET").ok();
    if fleet.is_some() || to.starts_with('@') || to.starts_with('#') {
        return hub::msg(fleet.as_deref(), to, kind, text);
    }
    let cards = commons::read_cards(&paths::sessions_dir());
    let card = cards
        .iter()
        .find(|(c, live)| *live && c.name == to)
        .map(|(c, _)| c);
    // In a fleet session: the user and the fleet's own sessions go through its hub.
    if let Some(f) = &my_fleet {
        if to == "user" {
            return hub::msg(Some(f), to, kind, text);
        }
        if let Some(c) = card.filter(|c| c.fleet.as_deref() == Some(f.as_str())) {
            let slot = c.slot.clone().unwrap_or(to);
            return hub::msg(Some(f), slot, kind, text);
        }
    }
    // In any session: its daemon sends, names it, and records it.
    if let Some(sock) = std::env::var_os("BELAY_SOCKET").map(PathBuf::from) {
        let req = ipc::Request::Send {
            to,
            kind,
            text,
            from: my_session(),
        };
        let reply = ipc::send(&sock, &req, Duration::from_secs(5))?;
        return Ok(reply.message.unwrap_or_default());
    }
    // From your terminal: a fleet session through its hub (an idle one gets a turn).
    if let Some(c) = card {
        if let (Some(f), Some(slot)) = (&c.fleet, &c.slot) {
            return hub::msg(Some(f), slot.clone(), kind, text);
        }
    }
    if card.is_some() || to.starts_with("topic:") || to == "repo" {
        return commons::send_as_user(&to, kind.as_deref(), &text);
    }
    // A name no card has: a slot of the newest fleet, as before C2.
    match hub::msg(None, to.clone(), kind.clone(), text.clone()) {
        Ok(said) => Ok(said),
        Err(_) => commons::send_as_user(&to, kind.as_deref(), &text),
    }
}

/// `belay inbox`: in a fleet session, the fleet's (and a line for the commons); in any
/// other session, its commons inbox; from your terminal, yours (then marked read), and
/// the newest fleet's if one is up.
fn show_inbox(fleet: Option<String>) -> Result<String> {
    if fleet.is_some() || std::env::var_os("BELAY_FLEET").is_some() {
        let mut out = hub::show_inbox(fleet.as_deref())?;
        if let Some(d) = std::env::var_os("BELAY_SESSION_DIR") {
            let n = commons::unread(&commons::inbox_path(std::path::Path::new(&d)));
            if n > 0 {
                out.push_str(&format!(
                    "\n{n} commons message(s) arrive at the next hook.\n"
                ));
            }
        }
        return Ok(out);
    }
    if let Some(d) = std::env::var_os("BELAY_SESSION_DIR") {
        return Ok(hub::render_inbox_file(&commons::inbox_path(
            std::path::Path::new(&d),
        )));
    }
    let lines = commons::user_lines();
    let mut out = hub::render_inbox_file(&commons::user_inbox());
    commons::mark_user_read(lines);
    if let Ok(f) = hub::show_inbox(None) {
        out.push_str("\n\nThe newest fleet's inbox:\n");
        out.push_str(&f);
    }
    Ok(out)
}

/// The session this shell belongs to (`belay claude` sets `BELAY_SESSION_DIR` in the
/// agent's shell): commons requests say who sends them.
fn my_session() -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("BELAY_SESSION_DIR")?);
    Some(dir.file_name()?.to_string_lossy().to_string())
}

/// Whether `dir` is a direct child of the sessions dir (`--session` also takes a path;
/// purge must not delete an arbitrary directory).
fn in_sessions_dir(dir: &std::path::Path, root: &std::path::Path) -> bool {
    match (std::fs::canonicalize(dir), std::fs::canonicalize(root)) {
        (Ok(d), Ok(r)) => d.parent() == Some(r.as_path()),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purge_only_deletes_inside_the_sessions_dir() {
        let t = std::env::temp_dir().join(format!("belay-purge-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let root = t.join("sessions");
        std::fs::create_dir_all(root.join("s1")).unwrap();
        std::fs::create_dir_all(t.join("other")).unwrap();
        assert!(in_sessions_dir(&root.join("s1"), &root));
        assert!(!in_sessions_dir(&t.join("other"), &root));
        assert!(!in_sessions_dir(&root, &root));
        assert!(!in_sessions_dir(&root.join("s1/.."), &root));
        let _ = std::fs::remove_dir_all(&t);
    }
}
