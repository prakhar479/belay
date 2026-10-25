<h1 align="center">belay</h1>

<p align="center"><b>A flight recorder and a seatbelt for Claude Code.</b></p>

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

- a test fails, so the agent changes the expected value to whatever the code returned;
- it tries the same fix five times, and each time the same test fails the same way;
- 300 passing tests scroll by, and the one failure is buried in them.

**belay** sits on Claude Code's hooks and catches those moments. It sees every tool call, and its output before the agent reads it. It can summarize the noise, block the edit that weakens a test, and tell the agent it is going in circles. It writes down every step it takes, with the reason.

The agent tries to make a failing test pass by changing what it expects. This is what it reads back, and the edit does not land:

```text
belay: this edit may weaken tests in tests/test_discount.py: expected value changed to the
failing run's actual value (19.99 → 19.98). If the user wants it anyway, they can run
`! belay allow` and you can retry the same edit.
```

## Try it in a minute

```sh
cargo install --git https://github.com/prakhar479/belay belay-cli   # or grab a binary from Releases

belay retro --all        # what belay would have caught in sessions you already ran
belay claude             # your next session, recorded (look mode: nothing changes for the agent)
belay report             # what happened, and what belay would have done
belay claude --belay-mode optimize   # let belay act
```

`belay claude` takes every `claude` argument. If belay cannot start, Claude Code runs without it.

## What it does

| | |
|---|---|
| **Test guard** | Before an edit lands in a test file, belay checks it. It flags assertions removed, skips added, expected values changed to the failing actual, tolerances raised, and golden files rewritten. In optimize mode the edit is denied until *you* run `! belay allow`. |
| **Loop detection** | The same failure after 3 rounds of edit-then-run, or an edit that reverts the last one. belay tells the agent, with the failure it keeps hitting. |
| **Faithful summaries** | Test output with hundreds of passing tests is cut to what matters, and every failure line is kept. One pytest run went from 6,407 to 1,576 tokens. `belay show o-3` prints the original. |
| **Every action explained** | Each session is a ledger. `belay why a-2` shows the cause chain of anything belay did. `belay flag` notes where it got something wrong. |
| **Sessions that see each other** | Run several sessions and each one knows what the others are doing. `belay sessions` shows who works on what. Sessions can message each other (`belay msg`), share findings (`belay share`, `belay ask`), and leave a handoff for the next session. |
| **Learned fixes** | When a failure is fixed and the full suite passes, belay remembers the change. The next session that hits the same failure is told how it was fixed. |
| **Fleets** | `belay fleet up --preset lead` runs a team of headless sessions. Each gets its own worktree, tasks come from a board, and a task counts as done only when its check passes. |

## Built to stay out of the way

- **Look before you act.** The default mode only records. Switch to optimize when the report convinces you.
- **Fails open.** A hook that errors or takes longer than 250 ms prints nothing, and the agent goes on. A hook costs about 2 ms at p95.
- **Local.** No network calls, no model calls. Data stays in `~/.local/share/belay`, with secrets redacted before anything is written.
- **Never edits the past.** belay acts only on what the agent has not seen yet, so the prompt cache stays intact.

## Status

belay is a working prototype. It supports Claude Code only, and its hook conformance is checked on Claude Code 2.1.303, 2.1.304, and 2.1.308 (`belay doctor` checks yours). It is tested on Linux and WSL; macOS builds exist but are not yet tested, and Windows is not supported. The audit (`belay audit`) is experimental.

## Learn more

- **[Usage guide](docs/guide.md)**: install, the first session, every feature, config, and fleets.
- **Design:** [reference](docs/belay-reference.md), [fleets](docs/fleet-topologies-2026-10-21.md), [commons](docs/commons-2026-10-22.md).
- **Plugin:** `claude plugin marketplace add prakhar479/belay && claude plugin install belay@belay`, if you would rather keep typing `claude`.

## Contributing

```sh
make check     # fmt, clippy -D warnings, tests (unit, end-to-end with a fake agent, property)
make e2e       # end-to-end only
```

`belay-core` is pure logic with no I/O; `belay-cli` does all the I/O. [`CLAUDE.md`](CLAUDE.md) lists the rules the code keeps, and it is also what Claude Code reads when it works on belay.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
