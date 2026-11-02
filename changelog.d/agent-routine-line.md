kind: added

- **Routine line:** a fresh session (not a resume or a compaction) gets one line at its start that points the agent at belay: read the last handoff, say what it works on, run `belay brief` before it is done, leave a handoff before it stops. It is recorded as a `routine` action. New config key `agent_skill = on|off` (default on) turns it off.
