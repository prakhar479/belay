kind: added

- **Memory in sessions:** at its end and before each compaction, a session keeps a handoff in memory: its task, files changed, last test, open failure, and the agent's own `belay handoff` text. The next session in the project starts with it, and with the best memories, within `recall_tokens` (new key, default 600). After a compaction, the agent gets a carry pack: its task, open failure, guard blocks, and the memories it was given. A failure that matches a `fix` memory gets it. The ledger records a `memory_recalled` action.
