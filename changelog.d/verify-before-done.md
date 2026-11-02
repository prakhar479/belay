kind: added

- **Verify before done** (`verify = on`, new key, default `on`): when the agent ends its turn with files changed since the last passing run of its tests, belay keeps it going once: "Run it before you finish." Once per turn, never twice for the same state, never in a fleet session (the hub runs the check). Claude Code gets a Stop `decision: block`. New action kind `verify` (older belay versions skip those lines with a warning).
