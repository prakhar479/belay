# Changelog

## Unreleased

- **No more modes.** Look mode is gone: belay always acts within what the agent supports, and records why. Each feature has its own config key: `guard = deny|warn|off` (default `deny`), `loops`, `shape`, `memory` (was `learn`), `awareness` (was `fleet`), all `on` by default; `commons` (`off` by default: other sessions' messages and handoffs do not reach the agent, yours always do); `wrap_tests` (now `false` by default); and `report = quiet|normal|verbose`. Session cards are kept whatever `commons` says.
- **`belay pause [feature]` and `belay resume [feature]`:** the kill switch for a live session, recorded in the ledger and shown by `belay status`. The agent's own `belay pause` and `belay resume` are denied.
- **Old config and flags still work, with a warning:** a `look` value reads as `on`; `learn` and `fleet` map to `memory` and `awareness`; `plugin_mode`, `--belay-mode`, and `belay fleet up --mode` are ignored. `belay mode` says how to pause instead. Fleet sessions run with `guard = deny` and `commons = on`.
- **Ledger format 2.** A pause or resume is a `feature_changed` entry; `mode_changed` and the session's `mode` are no longer written. 0.1 ledgers still read, and their reports say "recorded in look mode (0.1)".

- `belay launch <agent> [args…]` starts any agent belay has a connector for. `belay launch claude-code` is the same as `belay claude`, which stays.
- **Memory in sessions:** at its end and before each compaction, a session keeps a handoff in memory: its task, files changed, last test, open failure, and the agent's own `belay handoff` text. The next session in the project starts with it, and with the best memories, within `recall_tokens` (new key, default 600). After a compaction, the agent gets a carry pack: its task, open failure, guard blocks, and the memories it was given. A failure that matches a `fix` memory gets it. The ledger records a `memory_recalled` action.
- **Memory:** `belay memory` lists, searches, adds, shows, approves, and drops memories in one store, each with its provenance. It reads notes, learned fixes, findings, and handoffs as memories; `belay memory migrate` copies them in once.
- **`belay install` / `belay uninstall`:** the Claude Code plugin, `belay status` as the status line (with a backup, never over another status line without `--force`), and a printed launcher alias, each shown and confirmed first. `belay doctor` says whether the plugin is installed and the status line is belay's.
- **The plugin teaches the agent:** a `belay` skill (handoffs, findings, denied edits, `belay show`, `belay why`) and slash commands `/belay:report`, `/belay:handoff`, `/belay:recall`, `/belay:remember`. Plugin version 0.2.0.
- **Undo:** belay keeps the working tree at the start and at the end of each agent turn, as private git refs. `belay undo t-N` rolls the files back to one, also what shell commands changed, without touching your index, HEAD, stash, or branches.

## 0.1.0

The first public release. belay supervises Claude Code through its hooks. It runs on Linux and WSL; macOS builds exist but are untested.

- **Look mode (the default)** records every tool call, output, and edit, and what belay would do, without changing what the agent sees. `belay report`, `belay why`, `belay show`, and `belay retro --all` (over past transcripts).
- **Optimize mode** lets belay act:
  - **Test guard:** it denies edits that weaken tests (assertions removed, skips added, expected values changed to the failing actual, tolerances raised, always-true asserts, rewritten golden files) until you run `! belay allow`. `.belay.toml` protects more paths.
  - **Loop detection:** it tells the agent when the same failure survives three rounds of edit-then-run, or when an edit undoes the one before.
  - **Faithful summaries:** long test output is cut to what matters, keeping every failure line. The original stays one command away.
- **Sessions that see each other:** a board of live sessions (`belay sessions`), messages (`belay msg`), shared findings (`belay share`, `belay ask`), handoffs to the next session (`belay handoff`), and notices when another session edits the same file or runs git under you.
- **Learned fixes:** a failure that a full run then passes is remembered, and the next session that hits it is told how it was fixed.
- **Fleets:** `belay fleet up` runs a team of headless sessions in their own worktrees. It has a task board, check-gated tasks, budgets, nine presets, and a merge plan.
- **Audit (experimental):** `belay audit` lists what a diff did to tests and checks. It also runs as a GitHub Action.
- **A Claude Code plugin**, for those who keep typing `claude`.
