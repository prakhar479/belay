<h1 align="center">belay</h1>

<p align="center"><b>One install that upgrades Claude Code.</b></p>

<p align="center">
  <a href="https://github.com/prakhar479/belay/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/prakhar479/belay/actions/workflows/ci.yml/badge.svg"></a>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue"></a>
</p>

<p align="center">
  <a href="https://prakhar479.github.io/belay/">Website</a> ·
  <a href="docs/guide.md">Usage guide</a> ·
  <a href="https://github.com/prakhar479/belay/releases">Releases</a>
</p>

---

Coding agents are good. Then, one afternoon:

- a new session starts from zero, and reads the code again to find what the last one already knew;
- a test fails, so the agent changes the expected value to whatever the code returned;
- it tries the same fix five times, and each time the same test fails the same way.

**belay** sits on Claude Code's hooks and sees every tool call, and its output, before the agent reads it. It hands each session what the last one left. It keeps the agent honest about its tests. It runs risky work in a sandbox. It tells you what it did, and why.

## Try it in a minute

```sh
cargo install --git https://github.com/prakhar479/belay belay-cli   # or grab a binary from Releases
belay install            # the Claude Code plugin: hooks, a skill, slash commands, MCP tools (asks first)
belay install --statusline   # optional: `belay status` as Claude Code's status line

belay retro --all        # what belay would have caught in sessions you already ran
claude                   # your next session runs with belay (or `belay claude`, with every claude argument)
belay report             # what happened, and why belay did what it did
belay pause              # the kill switch, from your terminal (`belay resume` undoes it)
```

If belay cannot start, Claude Code runs without it.

## What it does

**Memory and handoffs.** When a session ends, and before each compaction, belay keeps a handoff: the task, the files changed, the last test, the failure still open, and the agent's own note. The next session in the project starts with it, and with the memories that rank best. After a compaction, or a `belay claude --resume`, the agent gets a carry pack: its task, the open failure, the guard's blocks, and the memories it was given. `belay memory` lists, searches, and adds memories. Each says who wrote it, and an agent's memory waits for you to approve it.

```text
belay: memory for this project (`belay memory list` shows it all; check that each still fits):
- The last session's handoff (m-12, 2 h ago): From shop-1. Task: fix date parsing. Changed: shop/dates.py,
  tests/test_dates.py. Last test: `pytest` failed (1 failing). Open failure: tests/test_dates.py::test_leap_year: …
```

**Honesty.**

| | |
|---|---|
| **Test guard** | Before an edit lands in a test file, belay checks it. It flags assertions removed, skips added, expected values changed to the failing actual, tolerances raised, and golden files rewritten. The edit is denied until *you* run `! belay allow` (or set `guard = warn`). |
| **Loop detection** | The same failure after 3 rounds of edit-then-run, or an edit that reverts the last one. belay tells the agent, with the failure it keeps hitting. |
| **Faithful summaries** | Long test output is cut to what matters, and every failure line is kept. One pytest run went from 6,407 to 1,576 tokens. `belay show o-3` prints the original. |
| **Verify before done** | When the agent ends its turn with files changed since the last passing test run, belay asks it once to run the tests first. It asks at most once per turn, so it cannot loop. |
| **Learned fixes** | When a failure is fixed and the full suite passes, belay remembers the change. The next session that hits the same failure is told how it was fixed. |

The agent tries to make a failing test pass by changing what it expects. This is what it reads back, and the edit does not land:

```text
belay: this edit may weaken tests in tests/test_discount.py: expected value changed to the
failing run's actual value (19.99 → 19.98). If the user wants it anyway, they can run
`! belay allow` and you can retry the same edit.
```

**Sandboxes.** `belay sandbox run -- <cmd>`, `belay sandbox claude`, and `belay sandbox task "<prompt>"` run a command, a whole session, or a headless follow-up on the repo's own clone, with a private home and limits. It does not see the rest of your home, the Docker socket, or your ssh agent. The network is the host's. The work comes back as branch `belay/sandbox/<id>` for you to read and merge. `belay sandbox doctor` tries to escape a fresh one. Without bubblewrap, belay refuses and runs nothing. The threat model is in [docs/sandbox.md](docs/sandbox.md).

**Skills from URLs.** `belay skill preview <url>` fetches a skill and shows its files, warnings, and digest. `belay skill add` installs it after a preview, pinned to its commit, with a lock file. An agent can only stage a skill; you run `belay skill approve`.

**Seeing what belay does.** When belay acts, Claude Code shows you a one-line note; the model never sees it. `belay status` is a status line, `belay watch` prints notes as they happen, and `belay claude` prints a summary when the agent exits. `belay report` tells the session's story, and `belay why a-2` shows the cause chain of one action. `belay pause [feature]` stops belay in a live session, and `belay undo t-N` rolls the files back to the end of a turn.

**Sessions that see each other.** Each session knows what the others on this machine are doing: `belay sessions`, `belay msg`, `belay share`, and `belay ask`. Fleets (`belay fleet up`) run a team of headless sessions, each in its own worktree. Fleets keep working, but get no new features for now.

0.1 had a look mode and an optimize mode; 0.2 has no modes. Each feature has its own config key.

## Built to stay out of the way

- **Acts within what the agent supports, and says why.** Every action is in the ledger with its reason. Each feature has its own switch, and `belay pause` stops them all.
- **Fails open.** A hook that errors or takes longer than 250 ms prints nothing, and the agent goes on. A hook costs about 2 ms at p95. A sandbox is the exception: it fails closed.
- **Local.** No model calls. belay goes on the network only when you ask: to fetch a skill, or the plugin at install. Data stays in `~/.local/share/belay`, with secrets redacted before anything is written.
- **Yours to approve.** `allow`, `pause`, `undo`, `memory approve`, `skill approve`, and `sandbox rm` refuse to run outside your terminal.
- **Never edits the past.** belay acts only on what the agent has not seen yet, so the prompt cache stays intact.

## Status

belay 0.2 is a working prototype. It supports Claude Code only. Its hook conformance is checked on Claude Code 2.1.303, 2.1.304, 2.1.308, and 2.1.316 (`belay doctor` checks yours). It is tested on Linux and WSL. macOS builds exist but are not yet tested, and Windows is not supported. Sandboxes are Linux only. Fleets are frozen, and the audit (`belay audit`) is experimental.

## Learn more

- **[Usage guide](docs/guide.md)**: install, the first session, every feature, config, and fleets.
- **Design:** [reference](docs/belay-reference.md), [sandbox threat model](docs/sandbox.md), [fleets](docs/fleet-topologies-2026-10-21.md), [commons](docs/commons-2026-10-22.md).

## Contributing

```sh
make check     # fmt, clippy -D warnings, tests (unit, end-to-end with a fake agent, property)
make e2e       # end-to-end only
```

`belay-core` is pure logic with no I/O; `belay-cli` does all the I/O. [`CLAUDE.md`](CLAUDE.md) lists the rules the code keeps, and it is also what Claude Code reads when it works on belay.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
