//! belay CLI: launcher, hook shim, session daemon, and inspection commands.

// Unix sockets, process groups, and signals throughout: say so instead of a page of
// errors (I5, X1).
#[cfg(not(unix))]
compile_error!("belay supports Unix (Linux, macOS) only for now");

mod audit;
mod bg;
mod brief;
mod cli;
mod commons;
mod config;
mod connector;
mod daemon;
mod disk;
mod dispatch;
mod doctor;
mod export;
mod findings;
mod flags;
mod fleet;
mod hub;
mod install;
mod ipc;
mod keepawake;
mod launcher;
mod learnstore;
mod mcp;
mod memorystore;
mod paths;
mod plugin;
mod report;
mod retro;
mod run;
mod sandbox;
mod sessions;
mod shim;
mod skillstore;
mod store;
mod undo;
mod usage;
mod watch;

use clap::Parser;

use cli::{Cli, Cmd};
use dispatch::run;

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    // The shim must never fail the agent: handle it before clap, whose errors exit 2, and
    // exit 2 from a PreToolUse hook blocks the tool call. Extra or missing arguments (a
    // newer settings file, an older binary) still exit 0 (rule 1).
    if argv.get(1).map(String::as_str) == Some("hook") {
        let event = argv.get(2).cloned().unwrap_or_default();
        // `--plugin`: called by the Claude Code plugin, with no launcher (plugin.rs).
        let from_plugin = argv.iter().skip(3).any(|a| a == "--plugin");
        config::quiet();
        // Exit 0 even if something panics.
        let _ = std::panic::catch_unwind(|| {
            if from_plugin {
                plugin::run(&event)
            } else {
                shim::run(&event)
            }
        });
        std::process::exit(0);
    }
    // `belay reap <pgid>`: the watchdog `belay run` starts (see run.rs). No clap: it must
    // start fast and never fail loudly.
    if argv.get(1).map(String::as_str) == Some("reap") {
        std::process::exit(run::reap(argv.get(2).map_or("", String::as_str)));
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            // `belay run` stands in for the agent's command: if its other arguments do not
            // parse, still run the command, unfiltered.
            if argv.get(1).map(String::as_str) == Some("run") {
                if let Some(sh) = arg_value(&argv, "--sh") {
                    std::process::exit(run::run("", None, &sh));
                }
            }
            e.exit()
        }
    };
    if let Cmd::Run { id, socket, sh } = &cli.cmd {
        std::process::exit(run::run(id, socket.as_deref(), sh));
    }
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("belay: {e:#}");
            std::process::exit(1);
        }
    }
}

/// Whether `dir` is a direct child of the sessions dir (`--session` also takes a path;
/// purge must not delete an arbitrary directory).
/// The value of `--name value` or `--name=value` in raw arguments.
fn arg_value(argv: &[String], name: &str) -> Option<String> {
    let eq = format!("{name}=");
    argv.iter().enumerate().find_map(|(i, a)| {
        if a == name {
            argv.get(i + 1).cloned()
        } else {
            a.strip_prefix(&eq).map(str::to_string)
        }
    })
}
