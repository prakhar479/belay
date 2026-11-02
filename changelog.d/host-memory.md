kind: changed

- **Memory respects the agent's own:** belay reads the CLAUDE.md files Claude Code loads and its auto-memory index, and never writes them. A memory they already state is not recalled at the start or in the carry pack; the `memory_recalled` reason says how many were left out. `belay memory add` refuses one with `already in CLAUDE.md (line N)`. The plugin skill says conventions go in CLAUDE.md, and belay memory is for what happened.
