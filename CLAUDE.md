# CLAUDE.md — belay

belay is a local supervisor for command-line coding agents.
Design: `docs/design.md` (public overview); the full design history is in `docs/dev/design-history/`. Work items: `docs/dev/TASKS.md`. `docs/dev/` is kept locally, not in the public repo.
Current scope: 0.2 and 0.3 (plan: `docs/dev/plan-0.2-2026-10-31.md`, tasks U0–U10). belay always acts within each agent's capabilities; there is no look mode. What belay does reaches the user through reporting. Claude Code first, with a harness-neutral core. Fleets (design: `docs/dev/design-history/fleet-*.md`) are frozen: kept working, no new features. Codex waits for S10.

## Rules

Do not break these. If a task needs you to, stop and ask.

1. **Fail open.** `belay hook` always exits 0. On any error or timeout it prints nothing. The one exception is a sandbox (`belay sandbox`): it **fails closed**. If it cannot be built as specified, nothing runs.
2. **belay-core has no I/O.** No files, sockets, env vars, clocks, threads, or model calls. Time comes in as `now_ms`.
3. **Never edit the past.** Act only on content before the agent sees it.
4. **Every action is recorded** with a one-sentence reason and its causes.
5. **Reporting never reaches the model.** Notices go to the user through user-only channels (status line, `systemMessage`, `belay watch`, the exit summary). Only actions reach the agent.
6. **Deterministic.** Use `sha256_hex` for hashes. Do not let `HashMap` order reach output or the ledger.
7. **Shim stays thin.** It only forwards. Target p95 < 20 ms (measured 2026-11-02: p50 ~2 ms, p95 ~3 ms).
8. **Capability-honest.** Plan only with `Capabilities`. Downgrade; do not fail.
9. **No new dependency** in belay-core without a reason in the commit message.
10. **Parsers need fixtures.** Every parser change adds a file in `fixtures/` and a test.
11. **Network only on request.** belay makes a network call only for an explicit `belay skill` command. It uses `git` and `curl` subprocesses. The hook path, the daemon, and the core never touch the network.
12. **Users approve what persists or escapes.** An approval that outlives the session, or that brings work out of a sandbox, is the user's: `allow`, `pause|resume`, `undo`, `memory|notes approve|drop`, `findings drop`, `fixes forget`, `skill approve|reject|remove`, `sandbox rm`, `bg stop|attach`, `fleet prune --yes` (see `controller/belay_cmd.rs`). Each refuses outside the user's terminal, and PreToolUse denies the agent's run of it.

## Unverified facts

Agent behavior marked `[Verify]` in code is an assumption. Before you build on one, check `docs/dev/TASKS.md` Phase 0. If a spike shows a fact is wrong, record it in `docs/dev/TASKS.md` under "Spike results" and fix the code. Do not work around it silently.

## Layout

```
crates/belay-core/src/        pure logic, no I/O
  event.rs                     common event vocabulary (connector → core)
  record.rs                    ledger entries (core output)
  capability.rs                what each agent supports
  perception/classify.rs       command → output kind, test runner, `belay show` re-opens
  perception/signature.rs      failure extraction + error-signature normalizer
  perception/shell.rs          shell parsing: segments, program words, write targets
  shape.rs                     faithful summaries (levels 0–3)
  session.rs                   session model, adaptation knobs
  loops.rs                     loop + edit-cycle detection
  guard.rs                     test-weakening rules
  broker.rs                    question catalog, rule answers, Decider trait
  controller.rs                sense → judge → decide → act → record
  controller/features.rs       per-feature settings (guard, loops, shape, memory, …), pause/resume
  controller/notice.rs         notices for the user (rule 5), report levels
  controller/memory.rs         recall at start, carry pack after compaction, handoffs
  controller/snapshot.rs       per-turn snapshot and undo records
  controller/config.rs         Config, Response, Permission (re-exported from controller)
  controller/shaping.rs        prepare, observe, shaping outputs, end of turn, filter
  controller/tools.rs          tool requested/denied, wrapping, file edits
  controller/commons.rs        card, send/share/ask, fleet and learning setters
  controller/belay_cmd.rs      spots the agent running belay's user-only commands (rule 12)
  memory.rs                    memories: scopes, kinds, provenance, rank, consolidate, handoff_from
  fleet/awareness.rs           F1: what other sessions did that matters here
  fleet/spec.rs                fleet files: roles, channels, limits, presets
  fleet/model.rs, model/tasks.rs  fleet state machine: slots, tasks, checks, messages
  fleet/scope.rs               a fleet role's write scope and others' claims
  learn.rs                     F2: learned fixes (fail → edits → full pass) and recall
  commons.rs                   C1–C4: cards, message limits, findings, search, handoffs
  audit.rs                     experimental audit of changes to tests and checks
  policy.rs                    `.belay.toml`: test paths, protected paths
  text.rs                      ANSI stripping, redaction, truncation, sha256
crates/belay-cli/src/         binary `belay`: all I/O
  main.rs                      entry: hook/reap fast paths, parse, exit code
  cli.rs                       clap types: every subcommand and its arguments
  dispatch.rs                  runs the parsed subcommand
  launcher.rs                  `belay claude`: daemon + agent with hooks
  daemon.rs                    per-session state behind a Unix socket
  daemon/commons.rs            commons requests (msg, share, ask, card, handoff)
  daemon/inbox.rs              fleet claims, inbox lines, learned fixes read at hooks
  daemon/watch.rs              watchers: done, fleet, idle, parent exit
  shim.rs                      `belay hook <Event>` (fail open)
  run.rs                       `belay run`: the wrapper around test commands (exit code kept)
  plugin.rs                    the Claude Code plugin's session start and mapping
  ipc.rs                       one JSON line each way
  connector/mod.rs             Connector trait, TranscriptSource, contract tests (every agent passes them)
  connector/claude_code.rs     hook JSON ↔ core events/responses, tool kinds, settings file
  connector/claude_code/transcript.rs  Claude Code transcripts (for retro)
  store.rs                     JSONL ledger + blobs (SQLite later; keep the API)
  report.rs                    report, why, output lookup
  retro.rs                     replay old transcripts without delivering (one, or --all)
  config.rs                    ~/.config/belay/config.toml → Config
  sessions.rs                  prune old sessions, remove stale sockets
  paths.rs                     session dirs, socket path, Claude projects dir
  fleet.rs                     F1 watcher (tails peers' ledgers), `belay fleet` view
  hub.rs                       fleet hub: meta, `up`, the hub loop, request handling
  hub/slots.rs                 starting sessions, workspaces, checks, output readers, process helpers
  hub/work.rs                  the hub's work on worktrees: snapshots, moving work in and out
  hub/client.rs                client side: send, status, task, msg, inbox, list views
  hub/prune.rs                 `belay fleet prune`
  hub/merge.rs                 `belay fleet report/merge/replay/restart/down`
  hub/supervise.rs, hub/tests.rs  detached hub start; hub tests
  learnstore.rs                <home>/learn: fixes.jsonl, notes; `belay note/notes`
  commons.rs                   card.json per session; `belay sessions` board, `belay card`, messages
  findings.rs                  <home>/commons/findings.jsonl; `belay share/ask/findings`
  audit.rs                     `belay audit`: worktree snapshots, diffs
  doctor.rs, export.rs         `belay doctor`, `belay export`
  memorystore.rs               <home>/memory: native memories + adapters over notes, fixes, findings, handoffs
  mcp.rs                       `belay mcp`: stdio MCP server (memory, handoff, show, why)
  install.rs                   `belay install/uninstall`: plugin, status line, alias
  undo.rs                      per-turn snapshots under refs/belay/<session>/, `belay undo`
  watch.rs                     `belay watch`: live notices for any harness
plugin/                        Claude Code plugin: hooks, skill, slash commands, .mcp.json
crates/belay-cli/tests/e2e.rs launcher → daemon → shim with a fake agent (mcp.rs: the MCP server over stdio)
fixtures/                      outputs, hook payloads, transcripts
docs/guide.md                  the usage guide (keep it in step with the CLI)
site/                          the GitHub Pages site (.github/workflows/pages.yml)
docs/dev/                      internal notes and TASKS.md: local only, not in git
```

## Data flow

agent hook → `belay hook` → socket → daemon → `Connector::map` → `Controller::handle` → `Decision { entries, blobs, response }` → `Store::persist` → `Connector::encode` → shim stdout → agent.

## Commands

```sh
make check                          # fmt --check, clippy -D warnings, test
make check-clean                    # the same with an empty HOME and minimal PATH, as CI runs it
cargo test -p belay-core           # fast loop for core logic
make e2e                            # end-to-end with fake agent
cargo run -- retro fixtures/transcripts/loop_session.jsonl
cargo run -- retro --all            # every transcript in ~/.claude/projects
cargo build && ./target/debug/belay claude      # live session under the dev build
BELAY_CAPTURE=1 ./target/debug/belay claude          # also keep raw hook payloads
jq . ~/.local/share/belay/sessions/<id>/hooks.jsonl   # raw payloads (with BELAY_CAPTURE=1)
scripts/conformance.py --record     # live check of the Claude Code behaviors belay needs (one short session)
scripts/conformance.py --replay <dir> # re-check a run kept with --keep (no model call)
scripts/conformance.py --plugin      # also plain `claude` with the plugin: env file, MCP names (one Haiku session)
scripts/skill_live.py               # does the agent use belay unasked? two Haiku sessions, scored (~$0.05)
scripts/learn_live.py ./target/debug/belay <dir>   # live fleet + learning check on Haiku (~$0.04)
cargo test --release -p belay-cli --test e2e -- --ignored shim_latency   # rule 7 budget
(cd fuzz && cargo +nightly fuzz run shell -- -max_total_time=60)       # also: output, guard
scripts/capture_fixtures.py hooks <hooks.jsonl> --event PostToolUse --match pytest --out fixtures/claude_code/x.json
```

## How to work

1. One task per session. A task comes from `docs/dev/TASKS.md`, or from a belay fleet task. In a worktree `docs/dev/` is absent, so the task body is the spec. Plan first for a task that changes behavior or touches more than 3 files. A small fix needs no plan.
2. Say what you work on: `belay card --task "U1.1 tool kinds"`. When you stop, leave a handoff: `belay handoff "done: …; open: …; next: …"`.
3. Write the test from the acceptance criteria first.
4. Iterate on the narrowest loop, and widen it only at the end (see "Fast loop").
5. Run `make check` before you say a task is done. It is the only gate.
6. Commit each task on its own, with a message that says what changed and why. Tick it in `docs/dev/TASKS.md` when that file exists. Add a short note if behavior changed.
7. Keep ledger formats backward-compatible, or bump the format and say so.
8. Keep `docs/guide.md` and the changelog in step with any change a user can see, in the same commit. For the changelog, add a fragment `changelog.d/<short-name>.md` (first line `kind: added|changed|fixed|removed`, then bullets); do not edit `CHANGELOG.md`. `scripts/changelog.py --check` validates it. In the guide, add to your area's section only.

### Fast loop

Pick the narrowest command that covers your change. Run `make check` once at the end.

```sh
cargo check -p belay-core                                  # seconds: does it compile
cargo test -p belay-core <filter>                          # one core test or module (controller::, memory::)
cargo test -p belay-cli --lib <filter>                     # CLI unit tests, no e2e
cargo test -p belay-cli --test e2e <name>                  # one end-to-end test
cargo clippy -p belay-core --all-targets -- -D warnings    # lint one crate
```

`cargo test -p belay-core` has no I/O and runs in seconds, so put logic there and test it there. e2e tests start real daemons, so add one per user-visible behavior, not one per branch.

### Keep the code clean

- **New behavior goes in a new module** when its file is over ~1,500 lines of code, not counting tests. `controller.rs` and `hub.rs` are past that: add next to them (`controller/<topic>.rs`), and don't grow them.
- **Tests go in `<module>/tests.rs`** once a module's tests pass ~300 lines (see `controller/tests.rs`, `fleet/model/tests.rs`).
- **Reuse before you write.** Check `text.rs` (hashing, redaction, truncation), `perception/` (commands and failures), `commons::search` (ranking), and `audit.rs` (git snapshots) first.
- **Delete what you replace** in the same change: no dead code, no `_old` copies, no commented-out code. Old ledger and config formats stay *readable* through serde defaults, not through parallel code paths.
- **Harness-neutral core.** `belay-core` never names an agent's tools or hook events. It works with `ToolKind` and `Capabilities`. Agent words belong in `connector/<agent>.rs`.
- **One feature, one config key**, with the default in one place (`Config::default`). Every key is in `docs/guide.md` § Config.
- **Comments say why**, not what. A `[Verify]` mark names the spike that would confirm it.
- No `#[allow(...)]` for clippy without a one-line reason next to it.

### Parallel work (belay fleets, on belay itself)

Independent tasks run as a belay fleet. Each worker gets its own worktree and branch, and a task is done only when its check passes.

- The coordinator (a terminal session) writes `fleet.toml` (`preset = "pool"`, `check = "make check"`, `permission_mode = "auto"`). It adds each task with `belay task add --body-file`, `--scope`, and `--depends`. Then it follows progress with `belay task list`, `belay inbox`, and `belay fleet report`.
- **Disjoint scopes.** Two parallel tasks never edit the same file, apart from `main.rs` subcommand lists and `docs/guide.md`. Those merge by hand. Changes to `controller.rs`, `event.rs`, and `record.rs` go in one chain of dependent tasks.
- A task body is self-contained: goal, files, acceptance tests, and what not to touch. Workers can't see `docs/dev/`.
- Workers talk through belay: a question goes to `belay msg user "…"`. A useful fact for other workers goes to `belay share "…"`, and they look before they start with `belay ask "…"`.
- **After `belay fleet down`:** `belay fleet merge --yes`, then merge each branch into the working branch in dependency order. Run `make check` after each merge.

## Style

- Errors: `anyhow` in the binary. No `unwrap` on external data.
- IDs: entries are `seq` numbers; outputs `o-N`; actions `a-N`.
- Regexes: use the `re!` macro (compiled once per call site).
- User-facing text: short, plain sentences. One idea per sentence.
