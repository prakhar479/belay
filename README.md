<h1 align="center">belay</h1>

<p align="center"><b>A safety net and a memory for your coding agent.</b><br>Install it once. Keep using Claude Code the way you do now.</p>

<p align="center">
  <a href="https://github.com/prakhar479/belay/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/prakhar479/belay/actions/workflows/ci.yml/badge.svg"></a>
  <a href="#license"><img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue"></a>
</p>

<p align="center">
  <a href="https://prakhar479.github.io/belay/">Website</a> ·
  <a href="docs/guide.md">Guide</a> ·
  <a href="https://github.com/prakhar479/belay/releases">Releases</a>
</p>

---

belay runs next to [Claude Code](https://claude.com/claude-code). It sees what the agent does, and it steps in at the few moments that matter:
- when the agent tries to make a failing test pass by weakening it;
- when it goes in circles;
- when it stops with tests it never ran;
- when a new session starts with no idea what the last one did.

It tells you what it did in one line, and it stays out of the way the rest of the time.

## Install

```sh
curl -fsSL https://prakhar479.github.io/belay/install.sh | sh
```

That's it:
- The script puts `belay` in `~/.local/bin` after checking the download's checksum.
- If Claude Code is installed, it adds belay's plugin.
- Run `belay doctor` any time to check your setup.
- `belay uninstall` takes the plugin out again.

[Read the script first](site/install.sh) if you like. With Rust installed you can also run `cargo install --git https://github.com/prakhar479/belay belay-cli`, then `belay install`.

## Use it: nothing new to learn

Start Claude Code as you always do. belay shows up only when it has something to say. Each message names the one command you might want next, so there is nothing to memorize.

**A new day, a new session.** The agent starts from where the last session stopped: what it was doing, which files it changed, which test was still failing.

```text
belay: memory for this project (`belay memory list` shows it all; check that each still fits):
- The last session's handoff (m-12, 9 h ago): Task: fix date parsing. Changed: shop/dates.py,
  tests/test_dates.py. Last test: `pytest` failed (1 failing). Open failure: test_leap_year …
```

**The agent tries to cheat a test.** belay stops the edit, and the agent fixes the code instead. If you really want the edit, type `! belay allow` in Claude Code.

```text
belay: this edit may weaken tests in tests/test_discount.py: expected value changed to the
failing run's actual value (19.99 → 19.98). If the user wants it anyway, they can run
`! belay allow` and you can retry the same edit.
```

**It says it's done, but the tests never ran.** belay asks it, once, to run them before it stops.

**Before you commit, ask what really happened.** The answer comes from belay's own record, not from the agent's summary.

```console
$ belay brief
Changed: 3 files (+48 −6)
Checked: ⚠ pytest (not run since the last edit) · ✓ ruff check .
Look at: 2 items
…
Look at
  1. dependency manifest changed: Cargo.toml (added leftpad) [41]
  2. src/app.py changed after the last passing run [44]
```

Add `--markdown` to paste it into a pull request.

**You stepped away.** You get a desktop notification when the agent is blocked, looping, or done. To get it on your phone, set `notify_command` to any command you like ([guide](docs/guide.md#getting-told-when-you-are-needed)).

**You're leaving, and the work isn't done.** Tell the agent you're heading out, and it offers to keep going in the background. Or start that yourself:

```sh
belay bg start "finish the parser and make the tests pass"
```

It keeps working after you close the terminal, in a sandbox when it can. It keeps the machine awake and notifies you when it's done or needs you. Later, `belay bg attach` brings the conversation back to your terminal. A closed laptop lid still sleeps if your system is set to; `belay doctor` says whether yours is, and how to change it.

**You want to try something risky.** `belay sandbox claude` runs the whole session in a contained copy of your repo. The work comes back as a git branch for you to read and merge.

**Where did my tokens go?** `belay usage` shows them by day, project, or session, including how much went into loops and failing runs.

**belay got it wrong.** Every warning ends with `Wrong? belay flag a-N`. Run it, and belay records the mistake for review.

## Your controls

| You want to | Run |
|---|---|
| Check that everything works | `belay doctor` |
| See what belay did in a session | `belay report` · `belay brief` |
| Let a blocked edit through | `! belay allow` (in Claude Code) |
| Roll files back to the end of a turn | `belay undo` |
| Turn belay off for this session | `belay pause` (and `belay resume`) |
| Change a setting | `belay config` |
| Remove belay from Claude Code | `belay uninstall` |

## Private by design

- **Everything stays on your machine,** in `~/.local/share/belay`. Secrets are redacted before anything is written.
- **No telemetry. No model calls.** belay goes on the network only when you ask it to fetch something (a skill).
- **If belay breaks or is slow, Claude Code carries on without it.** A hook gives up after 250 ms; it usually takes 2–3 ms.

## Status

| | |
|---|---|
| Version | belay 0.2 |
| Agent | Claude Code (checked against 2.1.303 to 2.1.316; `belay doctor` checks yours). The core is built to support more agents. |
| Platforms | Tested on Linux and WSL. macOS builds exist but are not yet tested. No Windows outside WSL. Sandboxes need Linux. |

## Learn more

- **[The guide](docs/guide.md):** every feature, every setting, troubleshooting.
- **[Sandbox threat model](docs/sandbox.md):** what a sandbox protects, and what it does not.
- **Design notes:** [reference](docs/belay-reference.md).

## Contributing

```sh
make check     # format, lint, and every test
```

`belay-core` is pure logic with no I/O, and `belay-cli` does the I/O. [`CLAUDE.md`](CLAUDE.md) lists the rules the code keeps.

## License

MIT or Apache-2.0, at your option: [LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE).
