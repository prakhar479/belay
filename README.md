<h1 align="center">belay</h1>

<p align="center"><b>A safety net and a memory for your coding agent.</b><br>Install it once, then keep using Claude Code exactly the way you do now.</p>

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

If you work with an AI coding agent like [Claude Code](https://claude.com/claude-code), you've probably seen at least one of these:
- **A new session starts from zero.** Yesterday the agent tracked down the bug and ran out of room. Today it reads all the same code again.
- **A test fails, so the agent "fixes" the test.** It changes what the test expects instead of fixing the code.
- **The same fix, over and over.** Edit, run, fail, five times, with the same error each time.
- **It says it's done, but it never re-ran the tests** after its last change.

belay sits quietly next to your agent and steps in at exactly those moments. The rest of the time you won't notice it's there. When it does step in, it tells you what it did and why, in one line.

## Install

```sh
curl -fsSL https://prakhar479.github.io/belay/install.sh | sh
```

That's all. The script:
- downloads belay and checks the download is genuine;
- installs it for your user only (no `sudo`);
- connects it to Claude Code if you have Claude Code installed.

Then run `belay doctor` to check everything's in place. Changed your mind? `belay uninstall` takes it out again.

Like to look before you run something? [Here's the script](site/install.sh). If you have Rust, `cargo install --git https://github.com/prakhar479/belay belay-cli` followed by `belay install` works too.

## What it's like to use

There's nothing new to learn. Open Claude Code the way you always do, and belay speaks up only when there's something worth saying. Each message tells you the one command you might want next, so there's nothing to memorize.

Here's what that looks like in practice.

### The next morning, your agent remembers

Start a new session, and the agent already knows where things stand: what it was working on, which files it touched, and which test was still failing.

```text
belay: memory for this project (`belay memory list` shows it all; check that each still fits):
- The last session's handoff (m-12, 9 h ago): Task: fix date parsing. Changed: shop/dates.py,
  tests/test_dates.py. Last test: `pytest` failed (1 failing). Open failure: test_leap_year …
```

belay writes this from what actually happened, not from what the agent says happened. It also works alongside your `CLAUDE.md` and Claude Code's own memory, never on top of them.

### Your tests stay honest

When the agent tries to make a failing test pass by changing the test itself, belay holds the edit, and the agent goes back and fixes the real problem:

```text
belay: this edit may weaken tests in tests/test_discount.py: expected value changed to the
failing run's actual value (19.99 → 19.98). If the user wants it anyway, they can run
`! belay allow` and you can retry the same edit.
```

If you look at it and the change is actually right, type `! belay allow` in Claude Code and it goes through. If the agent wraps up without running the tests after its last change, belay asks it to run them first.

### Before you commit, ask what really happened

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

It's a quick, honest summary for you to review: what changed, what was actually tested, and what deserves a second look. Add `--markdown` to paste it straight into a pull request.

### Step away; belay will let you know

You'll get a desktop notification when the agent is stuck, going in circles, or done. Want it on your phone instead? Point `notify_command` at whatever you use ([how](docs/guide.md#getting-told-when-you-are-needed)).

### Leaving for the day? Let it keep going

Tell the agent you're heading out, and it will offer to carry on without you. Or start it yourself:

```sh
belay bg start "finish the parser and make the tests pass"
```

It keeps working after you close the terminal, walled off from the rest of your machine when possible. It keeps your computer awake, and lets you know when it's finished or needs you. When you're back, `belay bg attach` brings the conversation back to your screen.

One honest caveat: closing a laptop lid still puts the machine to sleep if it's set up that way. `belay doctor` tells you how yours behaves, and how to change it if you want to.

### Try risky things somewhere safe

```sh
belay sandbox claude
```

This runs the whole session in a walled-off copy of your project. It can't see your SSH keys, cloud logins, or other projects. When it's done, the work comes back to you as a git branch: read it, and merge it if you like it.

### And a few more things

- **Where did my tokens go?** `belay usage` breaks it down by day, project, or session, including how much went into loops and failing runs.
- **Got it wrong?** Every warning ends with `Wrong? belay flag a-N`. Run it, and the mistake is recorded so belay gets better.
- **Rewind.** `belay undo` puts your files back the way they were at the end of an earlier step.

## You're always in charge

| If you want to | Run |
|---|---|
| Check that everything is working | `belay doctor` |
| See what belay did, and why | `belay report` or `belay brief` |
| Let a held edit through | `! belay allow` (inside Claude Code) |
| Roll your files back | `belay undo` |
| Switch belay off for this session | `belay pause` (`belay resume` turns it back on) |
| See or change settings | `belay config` |
| Remove belay from Claude Code | `belay uninstall` |

The agent can't do any of the things that lift a safeguard. Letting an edit through, pausing belay or approving something are yours alone.

## Your data stays yours

- **Everything stays on your computer.** Passwords and keys are blanked out before anything is saved.
- **No telemetry, no AI calls.** belay only goes online when you ask it to.
- **It never gets in the way.** If belay hits a problem, Claude Code just carries on without it. Most of the time it adds about 2 milliseconds to each step.

## Where it stands

belay 0.2 is young, and here's exactly where it is:

- **Works with Claude Code.** It's tested against real sessions on Claude Code 2.1.303 to 2.1.316, and `belay doctor` checks your version. Support for other agents is planned, and the design allows for it.
- **Platforms:** tested on Linux and on Windows through WSL. Mac builds exist but haven't been tried on a real Mac yet. Sandboxes need Linux.

## Learn more

- **[The guide](docs/guide.md):** every feature and setting, explained, plus troubleshooting.
- **[How sandboxes protect you](docs/sandbox.md):** what they keep safe, and what they don't.
- **[Design notes](docs/belay-reference.md):** for the curious.

## Contributing

Contributions are welcome. Run the full check before sending a change:

```sh
make check
```

[`CLAUDE.md`](CLAUDE.md) explains how the code is organized and the rules it follows.

## License

Free and open source, under your choice of [MIT](LICENSE-MIT) or [Apache 2.0](LICENSE-APACHE).
