# Changelog

## Unreleased

- `belay launch <agent> [args…]` starts any agent belay has a connector for. `belay launch claude-code` is the same as `belay claude`, which stays.

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
