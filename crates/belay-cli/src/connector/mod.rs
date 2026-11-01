//! Connectors translate agent hook traffic into core events, and core responses into hook output.
//! A new agent = a new connector. The core does not change.

/// The contract every connector meets, run over its hook fixtures. A new connector
/// instantiates this with its own fixture dir and the few readers that know its JSON:
///
/// - every fixture maps without a panic (to zero or more events), alone and in turn;
/// - an empty `Response` encodes to nothing, for every hook event;
/// - a deny encodes to a denial before a tool runs;
/// - `replace_output` encodes to the new output after a tool ran;
/// - `add_context` reads back from what is encoded, before and after a tool.
#[cfg(test)]
macro_rules! connector_contract_tests {
    (
        $name:ident,
        connector: $make:expr,
        fixtures: $dir:literal,
        hook_event: $hook_event:expr,
        pre_tool: $pre:literal,
        post_tool: $post:literal,
        denied: $denied:expr,
        replaced_output: $replaced:expr,
        context: $context:expr $(,)?
    ) => {
        #[cfg(test)]
        mod $name {
            use belay_core::{Permission, Response};
            use serde_json::Value;
            use $crate::connector::Connector;

            fn make() -> Box<dyn Connector> {
                Box::new($make)
            }

            /// `(hook event, payload)` for each fixture, sorted by file name.
            fn fixtures() -> Vec<(String, String, Value)> {
                let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../", $dir);
                let mut paths: Vec<_> = std::fs::read_dir(dir)
                    .expect("fixture dir")
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().is_some_and(|x| x == "json"))
                    .collect();
                paths.sort();
                let hook_event: fn(&Value) -> Option<String> = $hook_event;
                let out: Vec<_> = paths
                    .iter()
                    .map(|p| {
                        let v: Value =
                            serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
                        let name = p.file_name().unwrap().to_string_lossy().to_string();
                        let ev = hook_event(&v).unwrap_or_else(|| panic!("{name}: no hook event"));
                        (name, ev, v)
                    })
                    .collect();
                assert!(!out.is_empty(), "no fixtures in {dir}");
                out
            }

            fn payload_for(event: &str) -> Value {
                fixtures()
                    .into_iter()
                    .find(|(_, e, _)| e == event)
                    .unwrap_or_else(|| panic!("no {event} fixture"))
                    .2
            }

            #[test]
            fn every_fixture_maps_without_panic() {
                let mut one = make();
                for (name, event, payload) in fixtures() {
                    let alone = make().map(&event, &payload);
                    let in_turn = one.map(&event, &payload);
                    assert!(alone.len() <= 64 && in_turn.len() <= 64, "{name}");
                }
            }

            #[test]
            fn an_empty_response_encodes_to_nothing() {
                let c = make();
                for (name, event, payload) in fixtures() {
                    assert_eq!(
                        c.encode(&event, &payload, &Response::default()),
                        None,
                        "{name}"
                    );
                }
            }

            #[test]
            fn a_deny_encodes_to_a_denial() {
                let r = Response {
                    permission: Some(Permission::Deny("no".into())),
                    ..Response::default()
                };
                let out = make()
                    .encode($pre, &payload_for($pre), &r)
                    .expect("a deny is encoded");
                let denied: fn(&Value) -> bool = $denied;
                assert!(denied(&out), "{out}");
            }

            #[test]
            fn replace_output_encodes_the_new_output() {
                let r = Response {
                    replace_output: Some("short summary".into()),
                    ..Response::default()
                };
                let out = make()
                    .encode($post, &payload_for($post), &r)
                    .expect("a replacement is encoded");
                let replaced: fn(&Value) -> Option<String> = $replaced;
                assert_eq!(replaced(&out).as_deref(), Some("short summary"), "{out}");
            }

            #[test]
            fn add_context_round_trips() {
                let r = Response {
                    add_context: vec!["first note".into(), "second note".into()],
                    ..Response::default()
                };
                let context: fn(&Value) -> Option<String> = $context;
                for event in [$pre, $post] {
                    let out = make()
                        .encode(event, &payload_for(event), &r)
                        .unwrap_or_else(|| panic!("{event}: context is encoded"));
                    let got = context(&out).unwrap_or_default();
                    assert!(
                        got.contains("first note") && got.contains("second note"),
                        "{event}: {out}"
                    );
                }
            }
        }
    };
}

pub mod claude_code;

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use belay_core::{Capabilities, Event, Response};
use serde_json::Value;

use crate::report::Usage;

pub trait Connector: Send {
    fn agent(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    /// One hook call can produce several events (for example FileEdited + ToolCompleted).
    fn map(&mut self, hook_event: &str, payload: &Value) -> Vec<Event>;
    /// JSON the shim prints on stdout. None = print nothing.
    fn encode(&self, hook_event: &str, payload: &Value, response: &Response) -> Option<Value>;
    /// The repo's `.belay.toml`: protected paths are snapshotted for Bash writes too.
    fn set_policy(&mut self, _policy: &belay_core::policy::Policy) {}
    /// The daemon's socket, for wrapped runs in a shell that does not know it.
    fn set_socket(&mut self, _socket: &std::path::Path) {}
    /// How to read this agent's session transcripts (`belay retro`). None: it keeps none
    /// that belay can read.
    fn transcript(&self) -> Option<Box<dyn TranscriptSource>> {
        None
    }
    /// The directory this agent loads skills from (`belay skill`), one subdirectory per
    /// skill. `repo` is the project's root, for `Scope::Project`. None: no such place.
    fn skill_dir(
        &self,
        _scope: belay_core::skills::Scope,
        _repo: Option<&Path>,
    ) -> Option<PathBuf> {
        None
    }
    /// What this agent needs inside a sandbox (`belay sandbox`). The sandbox code never
    /// names an agent's files: it asks here.
    fn sandbox_files(&self) -> SandboxFiles {
        SandboxFiles::default()
    }
    /// The conversation the agent's arguments resume, if they resume one (U8).
    fn resume_target(&self, _args: &[String], _cwd: &Path) -> Option<ResumeTarget> {
        None
    }
    /// The id of the newest conversation in `cwd`, for "continue the latest".
    fn latest_conversation(&self, _cwd: &Path) -> Option<String> {
        None
    }
    /// The agent's id for the conversation its hooks are about, once a hook said it.
    fn conversation(&self) -> Option<String> {
        None
    }
    /// The agent's arguments (after its program) for one headless run of `prompt`
    /// (`belay sandbox task`). `resume`: start from a copy of this conversation's
    /// context, leaving the original as it is. Err: the agent has no headless mode.
    fn headless_argv(
        &self,
        _prompt: &str,
        _resume: Option<&str>,
        _model: Option<&str>,
    ) -> Result<Vec<String>> {
        bail!("belay cannot run `{}` headless", self.agent())
    }
    /// What one line of the agent's headless output says, if anything.
    fn headless_line(&self, _line: &str) -> Option<HeadlessLine> {
        None
    }
    /// Files to copy into a sandbox's private home so that a headless run with
    /// `resume = conversation` finds it from `cwd` (inside the sandbox): host path →
    /// path relative to the home. Empty: there is nothing to copy, or it is not found.
    fn conversation_files(&self, _conversation: &str, _cwd: &Path) -> Vec<(PathBuf, PathBuf)> {
        Vec::new()
    }
}

/// One line of an agent's headless output (`Connector::headless_line`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HeadlessLine {
    /// The model call this line reports on; its tokens count once however many lines
    /// repeat it.
    pub call: Option<String>,
    /// Tokens that call used: input, output, and cache writes (cache reads are left out).
    pub tokens: u64,
    /// The agent's final message, on the line that ends the run.
    pub result: Option<String>,
}

/// The agent belay supervises when a command does not name one.
pub const DEFAULT_AGENT: &str = "claude-code";

/// An agent's config inside a sandbox. Paths are relative to the user's home.
#[derive(Debug, Default, Clone)]
pub struct SandboxFiles {
    /// Files copied into the sandbox's private home, read-only.
    pub copy: Vec<PathBuf>,
    /// Directories bound read-only at the same place in the private home.
    pub ro_dirs: Vec<PathBuf>,
    /// The agent's API and auth environment variables, passed through when set.
    pub env: Vec<&'static str>,
}

/// What a resuming launch continues (U8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeTarget {
    /// This conversation id.
    Conversation(String),
    /// The newest conversation in the working directory.
    Latest,
    /// One the user picks in the agent: known at its first hook.
    Picker,
}

/// An agent's transcripts, read as the core events its hooks would have produced.
pub trait TranscriptSource {
    /// The transcript's steps, in order. A line that does not parse is skipped.
    fn read(&self, path: &Path) -> Result<Vec<TranscriptStep>>;
    /// The model's token usage, if the transcript records it.
    fn usage(&self, path: &Path) -> Option<Usage>;
    /// Every transcript under `root` (the agent's transcript dir), sorted.
    fn list(&self, root: &Path) -> Vec<TranscriptFile>;
}

/// One transcript line's worth: when, where, and what happened.
#[derive(Debug, Default)]
pub struct TranscriptStep {
    /// Unix ms, when the line says. Not checked: the reader keeps time sane.
    pub at_ms: Option<u64>,
    /// The working directory, when the line says.
    pub cwd: Option<String>,
    pub events: Vec<Event>,
}

/// A transcript found by `TranscriptSource::list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptFile {
    pub path: PathBuf,
    /// A subagent's transcript, not a main session's.
    pub subagent: bool,
    /// Scratch work (a project under `/tmp`), such as belay's own test runs.
    pub scratch: bool,
}

pub fn for_agent(name: &str) -> Result<Box<dyn Connector>> {
    match name {
        "claude-code" => Ok(Box::new(claude_code::ClaudeCode::default())),
        other => bail!("no connector for agent `{other}`"),
    }
}
