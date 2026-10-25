//! Connectors translate agent hook traffic into core events, and core responses into hook output.
//! A new agent = a new connector. The core does not change.

pub mod claude_code;

use anyhow::{bail, Result};
use belay_core::{Capabilities, Event, Response};
use serde_json::Value;

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
}

pub fn for_agent(name: &str) -> Result<Box<dyn Connector>> {
    match name {
        "claude-code" => Ok(Box::new(claude_code::ClaudeCode::default())),
        other => bail!("no connector for agent `{other}`"),
    }
}
