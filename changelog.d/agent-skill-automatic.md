kind: added
- `belay claude` gives the agent belay's skill, slash commands, and MCP tools with no plugin install (`--plugin-dir`, hooks left out; skipped when the plugin is installed). Turn off with `agent_skill = off`.
- `belay install --permissions` (in the default install) allows belay's read-only and note-taking commands and MCP tools in Claude Code's settings, so the agent is not asked each time. `belay uninstall` removes exactly those rules, and `belay doctor` reports them.
