# belay usage guide

<!-- Contributors: each section is owned by its area (guard, memory, sandboxes, fleets, …).
A change adds to its own section only, so parallel branches do not conflict here. Keep it one
guide: do not split it into files. Changelog entries go in changelog.d/, not CHANGELOG.md. -->

belay runs next to Claude Code and watches every tool call through its hooks. It keeps a record of each session. When you ask it to, it also steps in:

- it summarizes long test output;
- it blocks edits that weaken tests;
- it tells the agent when it is stuck in a loop.

Every step it takes is written down with a one-sentence reason.

This guide goes from install to fleets. Each section stands alone, so skip to what you need.

- [Install](#install)
- [Five minutes with belay](#five-minutes-with-belay)
- [What belay does for one session](#what-belay-does-for-one-session)
- [Seeing what belay does](#seeing-what-belay-does)
- [Sandboxes](#sandboxes)
- [Skills](#skills)
- [Sessions that know about each other](#sessions-that-know-about-each-other)
- [Fleets](#fleets)
- [Repo policy](#repo-policy)
- [Audit (experimental)](#audit-experimental)
- [Config](#config)
- [Data and privacy](#data-and-privacy)
- [Troubleshooting](#troubleshooting)
- [Command reference](#command-reference)

## Install

belay is one binary with no runtime dependencies. It supports Claude Code on Linux and WSL; macOS builds exist but are not yet tested. Windows is not supported.

**Prebuilt** (Linux x86_64 and aarch64, macOS arm64 and x86_64), from the [releases](https://github.com/prakhar479/belay/releases):

```sh
curl -L https://github.com/prakhar479/belay/releases/latest/download/belay-<version>-<target>.tar.gz | tar xz
mv belay-*/belay ~/.local/bin/        # any directory on PATH
```

With [cargo-binstall](https://github.com/cargo-bins/cargo-binstall), the same tarball in one step:

```sh
cargo binstall --git https://github.com/prakhar479/belay belay-cli
```

**From source** (Rust 1.87 or newer):

```sh
git clone https://github.com/prakhar479/belay && cd belay
cargo install --path crates/belay-cli    # installs `belay` into ~/.cargo/bin
```

Then check the install:

```sh
belay doctor     # will belay work here? one line per check: ok, warn, or fail, with the fix
```

`belay doctor` also says whether your Claude Code version is one belay was checked against. See [Troubleshooting](#troubleshooting) for what it checks.

### Into Claude Code

`belay install` sets up the rest in one step. It shows each change and asks before it makes it (`--yes` skips the question).

```sh
belay install                  # the Claude Code plugin: belay's hooks, its skill, and slash commands
belay install --statusline     # `belay status` as the status line, in ~/.claude/settings.json
belay install --launcher       # prints an alias so `claude` starts under belay
```

- `--plugin` (the default) runs `claude plugin marketplace add prakhar479/belay` and `claude plugin install belay@belay`. `--from <path>` uses a local clone instead. `BELAY_CLAUDE_BIN` picks the `claude` binary.
- `--statusline` keeps every other key in `settings.json` and saves the old file as `settings.json.belay-backup`. It refuses to replace a status line that is not belay's unless you add `--force`.
- `--launcher` prints the line to add to your shell config. belay does not edit it for you.

`belay uninstall` removes the plugin and belay's status line. A status line that is not belay's stays. `belay doctor` says whether the plugin is installed and whether the status line is belay's.

The plugin also gives the agent a `belay` skill (when to check handoffs and findings, what a denied edit means, how to read a summarized output) and five slash commands: `/belay:report`, `/belay:brief`, `/belay:handoff`, `/belay:recall`, and `/belay:remember`.

For one session from a clone, use `claude --plugin-dir ./plugin`.

The first hook of each conversation starts belay's session for it, and `SessionEnd` stops it. Plugin sessions use the same [features](#features) as `belay claude`. When you start with `belay claude`, the plugin's hooks step aside, so nothing is recorded twice.

## Five minutes with belay

**1. Look at sessions you already ran.** belay can read Claude Code's transcripts without starting anything:

```sh
belay retro --all          # every past session and subagent, ranked by what belay would have flagged
belay retro ~/.claude/projects/<project>/<session>.jsonl   # one session in detail
```

On a session that went in circles, the report looks like this:

```text
Session loop_session.jsonl · claude-code (retro) · 14 s
Needs attention: 1 loops
Loops (13 s in loops): 1
  - [would have steered] after 13 s: belay: 2 failures are unchanged after 3 fix attempts (edit, then run):
    tests/test_api.py::test_get_order: assert 500 == 200; tests/test_api.py::test_create_order: KeyError: 'total' …
Model tokens: 180.5k prompt (169.3k cache read, 93%) · 1.1k output · 8 messages
Tool calls: 8 (Bash 4) · Bash output: 892 tokens (est.) · noise 0% (est.)
Reads: 1 (0 repeated) · 0 searches · 20 tokens (est.)
Test runs: 4 · test runs are not wrapped (wrap_tests = off)
Edits: 3 through Edit/Write (guarded before they run)
```

**2. Start a session under belay.** Use `belay claude` in place of `claude`. Every other argument goes to Claude Code unchanged:

```sh
belay claude                 # belay acts within what Claude Code supports, and records it
belay claude --resume        # resume, as you would with claude
```

**3. See what happened.** In another terminal, or after you quit:

```sh
belay report                 # the newest session started in this directory
belay status                 # one line, for Claude Code's status line
belay watch                  # what belay does, as it happens
```

**4. Pause belay if it is in your way.** From another terminal, or with `!` in Claude Code:

```sh
belay pause guard            # one feature
belay pause                  # everything
belay resume                 # back on
```

**5. Put belay in your status line (optional).** Run `belay install --statusline`, or add this to `~/.claude/settings.json` yourself. It replaces any status line you have. Outside a belay session it prints nothing.

```json
"statusLine": { "type": "command", "command": "belay status" }
```

Short aliases help:

```sh
alias bl='belay claude'
```

## What belay does for one session

### Features

belay always acts within what the agent supports, and records every action with its reason. Each thing it does is a feature with its own [config](#config) key:

| Key | Default | What it does |
|---|---|---|
| `guard` | `deny` | Test guard: `deny` blocks an edit that may weaken tests until you allow it, `warn` only tells the agent, `off` leaves it |
| `loops` | `on` | Tells the agent when a failure is unchanged after several fixes, or an edit cycles |
| `shape` | `on` | Summarizes large test and build output (above `shape_threshold_tokens`) |
| `wrap_tests` | `false` | Runs test commands through `belay run`, so a failed run can be summarized too |
| `verify` | `on` | At the end of a turn with files changed since the last passing test run, asks the agent to run it before it finishes |
| `memory` | `on` | Handoffs between sessions, recall at the start and after a compaction, learned fixes, and the project's notes |
| `awareness` | `on` | Tells the agent what other belay sessions here did that matters to it |
| `commons` | `off` | Passes other sessions' messages, handoffs, and who is here to the agent. Your own messages always reach it |
| `report` | `quiet` | How much belay tells you about what it did: `quiet` (warnings), `normal` (+ what it did), `verbose` (+ minor notes). See "Seeing what belay does" |

A feature that is off takes no action. An action the agent cannot take is recorded as not applied.

#### Pause and resume

`belay pause` is the kill switch for a live session. `belay pause guard` pauses one feature; `belay pause` with no feature pauses all of them. `belay resume [feature]` turns them back on. Each pause and resume is in the ledger, and `belay status` shows what is paused. Run these yourself, in another terminal or with `!` in Claude Code: belay denies the agent's own `belay pause` and `belay resume`.

0.1 had a look mode and an optimize mode. They are gone: `belay mode` says so, and `--belay-mode` is ignored with a warning. Reports of 0.1 sessions say "recorded in look mode (0.1)" when belay acted on nothing.

### The test guard

Before an Edit or Write lands in a test file, belay compares the old and new text. It flags changes that make tests easier to pass:

- assertions removed;
- skip markers added to an existing test (`@pytest.mark.skip`, `it.skip`, `#[ignore]`, …);
- focus markers added (`it.only`, `describe.only`, `fit`, `fdescribe`; pytest's `@pytest.mark.only` plugin);
- an expected value changed to the failing run's actual value;
- tolerances raised tenfold or more, or a relative tolerance raised to 1 or more;
- assertions that always pass;
- commands that rewrite snapshot or golden files (`jest -u`, `--update-snapshots`, `cargo insta accept`, `UPDATE_GOLDEN=1`, …).

It does not flag:

- a new file, or one that was empty: it had no tests to weaken;
- files outside the project (the worktree's top), such as a draft in `/tmp`;
- a skip under a runtime condition: `@pytest.mark.skipif`, `unittest.skipIf` and `skipUnless`, `it.skipIf`, and `pytest.skip` or `t.Skip` inside an `if`, `else`, `except`, or `try` block;
- a skip on a test the same edit adds;
- a loosened tolerance with a comment on its line or the line above that says why (`atol=1e-6  # cast from float32`), or one raised less than tenfold;
- tests moved to another file: within one shell command, or within one turn (between two of your prompts), a test that leaves one file and appears in another with the same body counts as moved.

Comments and strings never count, and focus markers match each framework's exact syntax: `model.fit(` is not `fit(`.

An edit that only removes whole tests is not denied, since the tests may be on their way to another file. belay warns instead. If a later edit in the same turn adds them elsewhere, belay withdraws the warning (a `guard_withdrawn` judgment in the ledger). A shell command's removals wait for the command to end, so a move within one command raises nothing.

With `guard = deny` (the default) the edit is denied, and the agent reads why:

```text
belay: this edit may weaken tests in tests/test_discount.py: expected value changed to the failing
run's actual value (19.99 → 19.98). If the user wants it anyway, they can run `! belay allow` and
you can retry the same edit.
```

Only you can lift a block: type `! belay allow` in Claude Code, or run `belay allow` in a terminal. belay tells an agent's own `belay allow` apart from yours and refuses it. A shell command that writes a test file cannot be stopped before it runs, so belay checks the file after the command and tells the agent if it may have weakened tests.

If you allow blocks of the same rule twice in a session (say, two removed assertions), that rule only warns for the rest of the session: the agent still hears about it, but the edit goes through. `belay allow` tells you when this happens, and the ledger records it as a `feature_changed` entry on `guard:<rule>`. `belay resume guard` makes the rule deny again.

### Loops

When the same failure comes back after `loop_threshold` (default 3) rounds of edit-then-run, belay says so once:

```text
belay: 2 failures are unchanged after 3 fix attempts (edit, then run): tests/test_api.py::test_get_order:
assert 500 == 200; … A different approach may be needed.
```

It compares failures by a normalized signature, so line numbers, addresses, and timings do not hide a repeat. It also notices edit cycles (`edit, revert, edit`). You also get a notification when belay steers a loop (see [Getting told when you are needed](#getting-told-when-you-are-needed)).

### Verify before done

When the agent ends its turn with files changed since the last passing run of its tests, belay keeps it going once:

```text
belay: files changed since the last passing run of `pytest`. Run it before you finish.
```

The command is the last test or check command the session ran. belay asks at most once per turn, and never twice for the same unchanged state, so it cannot loop with the agent. Changes to docs (`.md`, `.txt`, `.rst`) do not count. A fleet session is left alone: the hub runs its task's check. Turn it off with `verify = off`.

### Test output, summarized

Agents run test suites again and again, and a suite with hundreds of passing tests floods the context. With `wrap_tests = true`, belay runs test commands through `belay run`, which keeps their exit code, so a failed run can be summarized too. Output above `shape_threshold_tokens` (default 2000) is replaced by a summary that keeps every failure line. The agent sees a footer like this:

```text
[… 242 lines omitted (242 passing) …]
=================================== FAILURES ===================================
________________________________ test_get_order ________________________________
>       assert resp["status_code"] == 200
E       assert 500 == 200
tests/test_api.py:11: AssertionError
…
======================== 2 failed, 300 passed in 0.28s =========================
[belay: 334 lines → 93 lines · full output: belay show o-3]
```

That pytest run went from 6,407 to 1,576 tokens. belay uses a summary only when it is at most `max_summary_ratio` (default 0.7) of the original. When the agent never re-opens the full output with `belay show`, later summaries get tighter. Re-opens loosen them again.

The full original is always kept: `belay show o-3` prints it, for you or for the agent.

### Memory

Memory is one store for what sessions and you keep: facts, decisions, gotchas, fixes, handoffs, and notes. Each memory says where it came from: belay saw it, an agent said it, or you did. It lives in `<home>/memory/`: `user.jsonl` for yours across projects, and one `project-<key>.jsonl` per project.

```sh
belay memory                                  # this project's memories and yours, newest first
belay memory search "pytest asyncio"          # the best matches
belay memory add "the cache lives in tmp" --kind gotcha --tag cache --file src/cache.rs
belay memory add "I review on Fridays" --scope user
belay memory show m-3                         # every field
belay memory approve m-3                      # vouch for an agent's memory (yours to run)
belay memory drop m-3                         # retire one (yours to run)
belay memory migrate                          # copy notes, fixes, findings, and handoffs in, once
```

Kinds are `fact`, `decision`, `gotcha`, `fix`, `handoff`, and `note` (the default). A memory you add in your terminal is yours. One added from an agent's shell is the agent's until you approve it. belay denies an agent's `belay memory approve` and `drop`.

Search ranks a matching failure signature highest, then a word in a tag, a file path, and the text. This project and your approval add a little, and so does a recent memory. The same text twice in a project and kind keeps only the newer one. A project keeps at most 500 memories; past that, the least used are retired.

#### Handoffs and recall

When a session ends, and before each compaction, belay keeps a handoff in the project's memory. It says what belay saw: the task from `belay card --task`, the files changed, the last test and its result, the failure still open (its test and first line), and whether the session was looping or blocked. The agent's own `belay handoff` text comes last, labelled as its own. A session keeps one handoff: a later one replaces the earlier.

When a session starts, the agent gets the newest handoff another session left in this project, and then the memories that rank best for the project (yours and approved ones first, then the newest). All of it fits in `recall_tokens` (600 by default). Fixes are left out here: a fix comes up when its failure does. The ledger records a `memory_recalled` action with the ids, so `belay why` can tell what the agent was given.

A failure whose signature matches a `fix` memory gets that memory, once per failure, like a learned fix.

After a compaction, the agent gets a carry pack: its task, the open failure, the guard's blocks and your decisions on them, and the memories it was given in this session. The carry pack only adds to the context; it changes nothing the agent already has. It also fits in `recall_tokens`.

Notes, learned fixes, findings, and the handoffs of ended sessions show up in `belay memory` as they are, with ids `note-N`, `fix-…`, `f-N`, and `h-…`. `belay memory migrate` copies them into the store and backs up the old files first. Running it again copies nothing new. The old commands keep working.

#### Learning across sessions

With `memory = on` (the default), belay remembers fixes on this machine. A failure counts as fixed when it goes away after edits and a complete run passes. A complete run uses no `-x`, `-k`, or `--lf`; belay reads its exit code, or the runner's summary when the run is piped through `tail` or `tee`. The session then keeps the failure, the command, the files changed, and a short diff of the edits (at most 30 lines, secrets redacted).

When another session of the same project hits that failure, belay tells it how it was fixed, with the diff, on the first hit. All worktrees of a repo are one project. A fix from another project is told only on a second hit, and without the diff: code does not leave its project.

```sh
belay fixes                 # this project's learned fixes
belay fixes forget          # drop them (yours to run)
```

**Notes** are what you want every session of a project to know:

```sh
belay note "integration tests need DB_URL; use make test-int"
belay notes                 # list; `belay notes approve n-2` or `drop n-2`
```

A note you type in your terminal is kept. A note an agent writes waits until you approve it.

#### Memory over MCP

`belay mcp` is an MCP server over stdio. The plugin declares it, so an agent in Claude Code gets these tools without a shell call:

| Tool | Does |
|---|---|
| `memory_search {query, limit?}` | The best matches, as `belay memory search` shows them (10 by default, at most 50) |
| `memory_add {text, kind?, tags?, files?}` | Keeps a project memory as the agent's, until you approve it |
| `handoff_get {}` | The newest handoff for this project, as `belay handoff` shows it |
| `output_show {id}` | The original of `o-N` in this session, as `belay show` |
| `why {id}` | The cause chain of `a-N`, `o-N`, or an entry, as `belay why` |
| `skill_preview {url}` | Fetches a skill and shows it, as `belay skill preview`. Installs nothing |
| `skill_stage {url, scope?, name?}` | Stages a skill for you to approve, as an agent's `belay skill add` |

| `sandbox_run {argv, id?}` | Runs a command in a sandbox, as `belay sandbox run` (see [Sandboxes](#sandboxes)) |
| `sandbox_task {prompt, id?, resume?, model?}` | A headless follow-up in a sandbox, as `belay sandbox task`; its result |

The session is `BELAY_SESSION_DIR` when set. Otherwise it is the newest session started in this directory, then in this project. The sandbox tools run as the agent: the same limits and refusals apply. The two skill tools fetch the URL they are given. Everything else stays on this machine. To add the server to another MCP client, run `belay mcp` as a stdio server from the project's directory.

### Resuming

When you resume a conversation, belay goes on where it left off. Loop counts, the open failure, blocked edits, paused features, shape levels, and the card's task carry over. So the third failure of the same test is a loop, even when the first two were in the run before.

```sh
belay claude --resume <id>    # or -r <id>: that conversation
belay claude --continue       # or -c: the newest conversation in this directory
belay claude --resume         # Claude Code's picker: belay follows the one you pick
belay claude --belay-fresh -c # resume the conversation, but start belay clean
```

`/resume` inside Claude Code works the same way. The plugin needs nothing: a plugin daemon that stops and starts again picks up its own record.

A resumed run is a new belay session that names the one it continues. Ids go on from it, so `a-N`, `o-N`, and `t-N` each name one thing across the runs. `belay report` shows every run, with a header each. `belay why`, `belay show`, and `belay undo` find ids in earlier runs. The resumed agent does not get a fresh start's recall: it gets the carry pack (its task, open failure, guard history, memories), as after a compaction.

belay finds the earlier run through `<home>/by-session/<conversation id>`, which every session writes. If that run was pruned, the resumed one starts clean. `resume = fresh` in the config makes every resume start clean.

### Undo

In a git repo, belay keeps the working tree as it is when the session starts (`t-0`) and at the end of each agent turn (`t-1`, `t-2`, …). A snapshot holds tracked and untracked files. Ignored files are left out. Each one is a private ref, `refs/belay/<session>/t-N`, so it is not on any branch and `git log` does not show it. belay takes it after the agent has its reply, so the agent does not wait.

To roll the files back, run `belay undo` in your own terminal:

```sh
belay undo                 # list the snapshots, and how many files differ from now
belay undo t-2             # show the files that would change, ask, then write them back
belay undo t-2 --yes       # the same, without asking
```

Files the snapshot does not have are deleted. Your index, HEAD, stash, and branches are not touched: what you staged stays staged. Before it writes, belay keeps the files as they were as `u-N`, so `belay undo u-1` brings them back. `--session` picks the session, as for `belay report`. Each snapshot and each undo is in the ledger.

Claude Code's `/rewind` restores only what its own edit tools wrote. `belay undo` restores files changed in any way: by a shell command, a formatter, or a code generator. It works the same for any agent belay runs.

Only you can undo. belay refuses `belay undo` from an agent's shell, and denies it when the agent tries. `belay purge` and the cleanup of old sessions delete their refs.

## Seeing what belay does

belay tells you what it does. It never tells the model: these notes go only to you.

- **In Claude Code.** When belay acts, a one-line note appears under the hook, for example `belay: blocked an edit that may weaken tests in tests/test_api.py. To let it through once: belay allow a-3. Wrong? belay flag a-3`. Every warning ends with that `Wrong?` hint (see [When belay is wrong](#when-belay-is-wrong)). A hook gets at most one note. If belay did more in that hook, the note counts the rest: `(+2 more summaries)`.
- **`report` decides what you see there.** `quiet` (the default) shows warnings: blocked edits and commands, guard warnings, and loops. `normal` adds what belay did for the agent: summaries, recalled fixes, notes, handoffs, and fleet notices. `verbose` adds minor notes: wrapped test runs, retries, and the tokens each summary kept out.
- **The status line.** `belay status` prints `belay · 2 blocked · 14k tokens kept out · 1 memory`. Memories are fixes the session learned or was told about.
- **`belay watch`** prints each note as it happens, in another terminal. It follows the newest live session, or the one you name with `--session`, and stops when the session ends or you press Ctrl-C. It shows every note, whatever `report` says; `--verbose` adds the minor ones. It reads only the ledger, so it works with any agent.
- **When the agent exits**, `belay claude` prints a short summary:

```text
belay: session 1793507424-19739 ended.
  Changed: 3 files (+48 −6)
  Checked: ⚠ pytest (not run since the last edit)
  Look at: 2 items
  kept out: 14.2k tokens of tool output
  handoff: done: the order API; next: the cart tests
  2 warnings · flag any that were wrong: `belay flag a-7`
  `belay brief` for the rest · `belay report` for more
```

The first three lines are the session brief's (see [The session brief](#the-session-brief)).

Every note is in the ledger, shown or not. `belay report` says "blocked" or "steered" for what belay did, and "would have" only for what the agent could not take, and for `belay retro`.

### Getting told when you are needed

In a long run it is easy to miss the moment the agent needs you. belay knows these moments and tells you:

- **blocked**: belay blocked an edit, and it waits for `belay allow`.
- **loop**: belay told the agent it is in a loop.
- **verify unmet**: the agent stopped, and its tests have not passed since its last edits. belay asked it once to run them.
- **done**: the agent ended its turn, and no new prompt came for a minute.
- **waiting**: Claude Code shows a permission prompt or asks you a question (its `Notification` hook).
- **message**: a session wrote to you with `belay msg user`.

Each moment goes to a desktop notification (`notify-send` on Linux, `osascript` on macOS), a terminal bell under `belay claude`, and your own command, if you set one. You get at most one notification of each kind per session every 2 minutes.

```toml
notify = attention              # off, attention (the default), or all (adds every warning)
notify_command = ~/bin/ping-me  # run with each notification as JSON on stdin
notify_quiet = 22:00-07:00      # no notifications in this window, local time
```

`notify_command` runs with `sh -c`. It reads one JSON line on stdin:

```json
{"session":"1793507424-19739","repo":"/home/me/shop","kind":"blocked","text":"Blocked an edit of tests/test_api.py. If it is intended, run `! belay allow`.","action":"a-3","ts_ms":1793507500000}
```

`kind` is `blocked`, `loop`, `verify_unmet`, `done`, `waiting`, `message`, or `warning`. `action` names the action, or is `null`. belay stops the command after 5 seconds. It never delays the agent: the command runs on its own thread. belay itself makes no network call here. Your command may, for example to send a phone push.

The agent never sees these notifications. The old `notify = true` reads as `all`, and `notify = false` as `off`.

### The session brief

After a session, the question is what the agent actually did, and whether you can trust it before you commit or merge. The agent's own summary can be wrong. `belay brief` answers from belay's ledger and git only:

```sh
belay brief                      # the newest session started here
belay brief --session -2         # the one before it
belay brief --markdown           # a "## Summary by belay" section for a pull request
belay brief --json               # stable keys, for scripts
```

```text
Session 1793507424-19739 · 12 min · 340.0k prompt tokens, 12.0k output
Changed: 3 files (+48 −6)
Checked: ⚠ pytest (not run since the last edit) · ✓ ruff check .
Look at: 2 items

Changed
  Cargo.toml  +1 −0
  src/app.py  +40 −5
  tests/test_app.py  +7 −1

Checked
  ⚠ pytest: not run since the last edit (3 runs, o-7)
  ✓ ruff check .: passed after the last edit (1 run, o-9)

Look at
  1. dependency manifest changed: Cargo.toml (added leftpad) [41]
  2. src/app.py changed after the last passing run [44]

belay: 2 summaries (9.2k tokens kept out) · 0 guard blocks · 0 loop nudges · 1 memory recalled
```

- **Changed:** the files the session edited, by tool or by shell, and, in a git repo, the diff from the session's start snapshot (`t-0`, see [Undo](#undo)) to the working tree, with lines added and removed. That diff includes any change you made yourself meanwhile.
- **Checked:** each test or check command the session ran, with its last result. ✓ passed after the last edit. ⚠ passed once, but source files changed after it. ✗ its last run failed. `?` its result is not known. "After the last edit" means what it means for [verify before done](#verify-before-done): a docs edit does not count. With more than three commands, the first lines count them by state.
- **Look at**, most important first, each with its evidence: an action `a-N`, an entry number, or `git` (`belay why <id>` follows it). Guard actions, and whether you allowed them; verify-before-done nudges; test changes that may weaken tests; CI and check config changed; dependency manifests changed, with the names added (Cargo.toml, package.json, pyproject.toml, requirements*.txt, go.mod, Gemfile); suppressions added (`# noqa`, `#[allow]`, …); and source files changed after the last passing run. These are the [audit](#audit-experimental)'s rules, over the session's changes.
- **belay:** what belay did: summaries and the tokens they kept out, guard blocks, loop nudges, and memories recalled.

The exit summary shows the first three lines. The agent can ask for the brief through the MCP tool `brief`, and the plugin's `/belay:brief` asks it to summarize the session for review.

### Reading the record

Each session is a ledger: numbered entries, outputs `o-N`, and actions `a-N`. Every action has a reason and the entries that caused it.

```sh
belay report                     # the newest session started here
belay report --session -2 --full # the one before it, every section
belay why a-2                    # the cause chain of an action
belay show o-3                   # the full original of a summarized output
belay sessions --history         # every recorded session
```

`--session` takes an id, a unique prefix, `-1` for the newest, `-2`, and so on, or a path.

belay also sees what the agent reads: each Read, Grep, and Glob is recorded after it runs, as an output `o-N`, and is never changed. The report counts them on a line like `Reads: 12 (3 repeated) · 4 searches · 8.1k tokens (est.)`. A read is repeated when the agent reads the same content of the same file again with no compaction in between: it already had that text. When a subagent finishes, the ledger records a `subagent_ended` event.

"Tokens kept out" (in the report, `belay status`, and the exit summary) adds up, for each summary that reached the agent, the original's tokens minus the summary's. A summary the agent could not take, or whose reply came too late, counts nothing. Tokens are estimated at 4 characters each.

If belay got something wrong, flag it: see [When belay is wrong](#when-belay-is-wrong).

To report a bug, `belay export --session -1 > s.json` writes one session with paths, your user name, and secrets replaced.

### When belay is wrong

belay learns its false positives only from flags. A flag takes one command, and the note is optional:

```sh
belay flag                       # the newest warning of the newest session here
belay flag a-3                   # that action
belay flag a-3 "the test name changed, not the test"
belay flag "too eager"           # the newest warning, with a note
belay flag o-4 "summary hid the error"   # also an output, an entry number, or `session`
```

`belay report` lists the session's flags and counts the warnings nobody flagged.

`belay review` lists every flag across sessions. Flags are grouped by rule: the guard rule (`guard: tests_removed`, `guard: skip`, …), `loop`, `cycle`, `verify`, or the kind of action. The newest come first. Each flag shows the action's reason and two commands: one to look closer, and one to save it as a fixture:

```sh
belay export --action a-3 --fixture flagged-a-3 --session <id>
```

That writes what led to the action into a new directory: the edit's before and after (`before.py`, `after.py`, `path.txt`), or the outputs (`output-o-N.txt`), with `action.json` and a `README.md`. The text is cleaned like `belay export`: paths, your home, your user name, and secrets are replaced. It still comes from your project, so review it before you commit it anywhere public.

**The agent can flag too.** When it thinks a belay warning is wrong, the plugin's skill tells it to say so in one line with `belay flag a-N "why"`. A flag run anywhere but your own terminal is recorded as the agent's. `belay review` shows the agent's flags apart from yours, and `belay report` marks them `[agent]`. A flag changes nothing: it lifts no guard and allows no edit. Only `belay allow` does that, from your terminal.

## Sandboxes

A sandbox runs a command, a whole agent session, or a headless follow-up where it cannot reach your other files. It works on its own clone of the repo, on branch `belay/sandbox/<id>`, with a private home and limits. git works inside. At the end, belay commits what changed and brings the branch into your repo. You review and merge it.

```sh
belay sandbox run -- make test       # a command; the clone goes, the branch stays
belay sandbox run --id fix --keep -- cargo build   # name it, keep it for the next run
belay sandbox claude                 # a whole Claude Code session in a sandbox
belay sandbox claude --id fix --resume <conversation>   # go on with it, in the same sandbox
belay sandbox task "add tests for parse_size"   # a headless follow-up; prints its result
belay sandbox task "try the other fix" --resume <conversation> --model haiku
belay sandbox list                   # every sandbox: running, kept, or removed
belay sandbox show fix               # branch, commits, the audit of the branch, limit stops
belay sandbox diff fix               # what the branch changed since it started
belay sandbox stop fix               # stop a running one
belay sandbox rm fix [--branch]      # remove its clone and home (yours to run)
belay sandbox doctor                 # try to escape a fresh sandbox; exit 1 if one gets out
git merge belay/sandbox/fix          # take the work, once you have read it
```

The command starts in the same subdirectory of the clone as yours in the repo. Its output streams, and belay returns its exit code. The sandbox starts from your HEAD commit: uncommitted changes are not in it. A sandbox whose branch is already there (from an earlier run with the same id) starts from that branch.

**A whole session.** `belay sandbox claude [--id S] [claude args…]` runs Claude Code inside a sandbox. belay's daemon runs outside and supervises it: the agent's hooks reach it through the session socket, so the guard, loops, and the ledger work as in `belay claude`. The agent cannot see or signal belay. The card shows `sandbox: <id>`. At the end belay prints the session's summary and the branch. The sandbox is kept, so `--id S --resume <conversation>` goes on in it, with belay's state (see [Resuming](#resuming)). Only you start one. Claude Code's own files inside the sandbox (`~/.claude.json`, settings, credentials) are read-only copies.

**A follow-up task.** `belay sandbox task "<prompt>" [--id S] [--resume <conversation>] [--model m] [--keep]` runs the agent headless (`claude -p`) in a sandbox, supervised by a belay session of its own, and prints a short result: the agent's final message, the branch and the files changed, the audit, the tokens used, and any limit that stopped it. The agent runs without permission prompts there: the sandbox is the boundary, and belay's guard still applies. `--resume` gives it a copy of that conversation, so it starts with its context; the original is not changed. You or an agent may start one. An agent may start at most `sandbox_tasks_per_hour` (4) per session. A task that uses more than `sandbox_task_tokens` (2M: input, output, and cache writes) is stopped.

Inside, the command sees the system dirs read-only, your toolchains read-only (`sandbox_ro`), the repo's object store read-only (so git finds the history), the clone, and a private home. It does not see the rest of your home (`~/.ssh`, cloud credentials, other repos), `/mnt`, the Docker socket, your ssh agent, or your display. On WSL it cannot run Windows programs. The network is the host's. The environment is cleared, then a short list is passed: `PATH`, locale, `TERM`, `USER`, `SHELL`, `TZ`, `BELAY_*`, and the agent's API keys. `BELAY_SANDBOX` is set to the sandbox id, and `belay sandbox` refuses to nest.

The sandbox owns its clone's `.git`, hooks and config included, so belay runs nothing of it on your machine: it commits inside the sandbox, and fetches the branch with git's sending side also inside. belay's undo snapshots and audits skip a sandbox's clone. How, and why, is in [sandbox.md](sandbox.md#getting-the-work-out).

belay watches each sandbox from outside: memory (`sandbox_memory`), processes (`sandbox_procs`), disk use of the clone and home (`sandbox_disk`), and wall time (`sandbox_wall`). When one is passed, belay stops the sandbox and records why. `belay sandbox show` lists these stops. Run from a belay session, the start and stop are also in the session's ledger, and the card shows the sandbox.

**Build cache.** A sandbox builds in its own clone, and the build goes when the clone goes. With `sandbox_build_cache = shared`, belay binds `<belay home>/cache/<repo>/sandbox/target` read-write into each sandbox of the repo and sets `CARGO_TARGET_DIR` to it, so builds start warm. It is off by default: a sandbox could leave build output there (a build script, a proc macro) that the next sandbox of the repo runs. Fleet sessions never use it. See [sandbox.md](sandbox.md#what-it-does-not-protect).

There is a sandbox only where it can be built as asked. Without bubblewrap (`bwrap`) or user namespaces, belay refuses and runs nothing. It is Linux only for now. Agents may run `belay sandbox run` and `belay sandbox task`, also as the MCP tools `sandbox_run` and `sandbox_task` (`sandbox_agent = off` stops that). Only you may run `belay sandbox claude` and `belay sandbox rm`: belay refuses them outside your terminal, and denies `rm` to the agent. What a sandbox protects, and what it does not, is in [sandbox.md](sandbox.md).

## Skills

A skill is a folder of instructions (`SKILL.md` and its files) that the agent loads in every session. That makes it a lasting way into the agent's instructions. belay makes installing one deliberate: it fetches, checks, pins, and records it, and only you approve it.

```sh
belay skill preview https://github.com/o/r/tree/main/skills/pdf   # look; installs nothing
belay skill add https://github.com/o/r/tree/main/skills/pdf      # preview, confirm, install
belay skill add ./my-skill --scope project --name pdf --yes
belay skill list                                                 # installed, waiting, drift
belay skill update [name]                                        # fetch again; a change waits for approval
belay skill approve pdf | reject pdf | remove pdf                # yours to run
```

Sources:

- A GitHub tree or blob URL (`github.com/<owner>/<repo>/tree/<ref>/<path>`). belay makes a shallow, sparse clone of that directory and pins the commit. A ref with a `/` in it is not supported: use a commit or a tag.
- A raw URL to a `SKILL.md` (a skill of one file). The server's ETag is kept.
- A `.zip` or `.tar.gz` archive. belay lists it first and refuses absolute paths, `..`, symlinks, and links before anything is extracted.
- A local path or a `file://` URL.

belay uses `git`, `curl`, `tar`, and `unzip`, with a 60 s limit for each download. A skill has at most 200 files and 1 MB. It needs a `SKILL.md` whose frontmatter has `name` and `description`; the name is cut to `a-z`, `0-9`, and `-`.

Preview shows the name, the description, the files, the warnings, and a digest (SHA-256 over the files). Warnings do not block. belay warns about a download piped to a shell (`curl … | sh`), a base64 blob over 200 characters, an executable script, text that asks to disable belay, hooks, permissions, or a sandbox, "ignore previous instructions", and hidden Unicode (bidi controls, zero-width characters).

Where skills go (Claude Code): `--scope user` (the default) is `~/.claude/skills/<name>` (under `CLAUDE_CONFIG_DIR` if set). `--scope project` is `<repo>/.claude/skills/<name>`. Each of these dirs has a `.belay-skills.lock`: for each skill, its URL, commit or ETag, digest, who approved it, and when. Commit the project's lock with the skills. `belay skill list` and `belay doctor` say when a skill's files no longer match the digest you approved (drift).

**Agents stage; you approve.** From your terminal, `add` previews, asks, and installs. From an agent (a shell in a belay session, or the `skill_stage` MCP tool), `add` only stages the skill and tells the agent to ask you. `belay skill approve <name>` shows it again (with what changed, for an update) and installs it. `approve`, `reject`, and `remove` refuse to run outside your own terminal, and belay denies the agent's run of them. `update` stages a change the same way.

belay keeps a record of each stage, approval, install, rejection, and removal in `<data dir>/skills/log.jsonl`, with the session when there is one.

A skill added while Claude Code runs may not be seen until the session restarts. [Verify]

## Sessions that know about each other

When you run several sessions on one machine, belay lets each one see the others. These features are called the commons. Cards are always kept, for your board. The rest is off for the agent unless you set `commons = on`; with the default `off`, only your own messages reach agents.

### Cards and the board

Every belay session keeps a card. belay fills in what it observes: the repo and worktree, the status (working, waiting, blocked, looping, idle, ended), the last test result, and the files changed lately. The agent adds its own words with `belay card --task "…"`.

```text
$ belay sessions
3 live belay sessions
/home/you/shop
  shop-1  working  "move checkout to the new payments API"
      changed shop/payments.py · active 1 min ago
  shop-2  working  "add tests for refunds"  (you)
      last test `pytest tests/test_refunds.py` failed (2) 59 s ago · active 1 min ago
/home/you/site
  docs-1  working  "rewrite the install page"
      active 1 min ago
```

`--all` adds sessions that ended in the last day, and `--json` is for scripts and agents. With `commons = on`, a session that starts while others run is told how many there are and how to see them.

### Messages

```sh
belay msg shop-1 "will the charge() signature change?"   # one session, by its card name
belay msg topic:payments "the sandbox key rotated"        # sessions that ran `belay card --join payments`
belay msg repo "I am rebasing main in 5 minutes"         # every session in this repo
belay msg user "done; see the PR"                         # your `belay inbox`
```

You send from your terminal the same way. A message reaches the receiver at its next hook, labelled as another session's words, with how to reply. Without `--kind`, a reply to whoever asked is an answer, text that asks is a question, and anything else is a notice. A session sends at most 10 messages a minute to one session and 30 in all.

The sender is named by its daemon. Each request also says which session's shell it came from, and a daemon refuses a request from another session's shell. This is identity, not authentication: an agent with a shell could still write into another session's inbox file. Treat messages as information, as belay labels them.

### Findings: share and ask

Sessions publish what they learn and search before they dig in:

```sh
belay share "pytest-asyncio needs asyncio_mode = auto in pyproject" --kind gotcha --tag pytest,asyncio
belay ask "asyncio tests hang"      # findings and learned fixes, best first, plus who might know
belay findings                      # this project's findings; `belay findings drop f-3` removes one
```

A finding's text reaches every project. A code snippet (`--code-file`) shows only in its own project.

### Handoffs

Before it stops, a session can leave a note for whoever comes next:

```sh
belay handoff "done: parser; open: the date test fails on leap years; next: fix parse_date"
```

The next session in that repo (any worktree) gets the newest card that ended there in the last week:
- what it left;
- its task;
- the files it changed;
- its last test;
- whether it was looping or blocked.

`belay handoff` with no text shows what the next session would get. With `commons = on` the agent hears it at start. With `memory = on` (the default), belay also keeps the handoff in memory, and the next session hears that one instead (see [Memory](#memory)).

### Awareness notices

Each daemon reads the other live sessions' ledgers once a second and notices four things:

- **Peer edit.** In the same checkout, another session edited a file after this one did.
- **Peer git.** In the same checkout, another session ran a git command that changes files or HEAD (`stash`, `checkout`, `reset`, `commit`, …).
- **Peer failure.** A failed run names a file another session in this checkout edited in the last 30 minutes.
- **Worktree overlap.** In another worktree of the same repo, both sessions edited one file, so a merge will conflict there. Not once the other session's version of the file is in your HEAD (its branch was merged), and not once that session has ended and its worktree is removed. For a session that has ended, the notice says so, once per file.

With `awareness = on` (the default), the agent gets these at its next hook. For example: "belay: shop-1 edited shop/payments.py after you did. Read it again before you change it." They are also shown in `belay report`.

## Fleets

A fleet is a team of headless Claude Code sessions that belay starts and manages for you. A hub process:

- starts each session in its own git worktree, on a `belay/<fleet>/<slot>` branch;
- gives out tasks from a board;
- runs a check before it counts a task as done;
- restarts sessions that crash;
- records every step.

### Quick start

```toml
# fleet.toml
preset = "lead"                    # one lead splits the work; workers do the parts
check = "make test"                # a task is done only when this passes in its worktree
permission_mode = "acceptEdits"    # headless sessions cannot answer permission prompts
```

```sh
belay fleet up fleet.toml
belay task add "add CSV export to the reports page"   # goes to the lead
belay fleet status                  # sessions: state, turns, restarts, worktree
belay task list                     # the board: state, session, dependencies, outcome
belay inbox                         # what the fleet tells you
belay fleet report                  # tasks, sessions, tokens, cost, and the merge plan
belay fleet merge --yes             # after `belay fleet down`: commit each worktree to its branch
```

`permission_mode` decides what the agents may do without asking. `bypassPermissions` lets them run any command, so use it only in a sandbox. `belay fleet up` warns when a headless role has no `permission_mode`.

### Presets

| Preset | Roles |
|---|---|
| `solo` | One agent that reports to you |
| `peers` | Equal agents that can message each other |
| `lead` | A lead splits your task; a fresh worker does each part (up to 4) |
| `specialists` | A lead, a tester who may edit only tests, and an implementer who may not |
| `tree` | A lead with two sub-leads, each running its own workers |
| `pipeline` | Plan, implement, verify: each stage hands its summary and diff to the next |
| `review` | An author writes the change; a reviewer reads the diff, audit, and check result |
| `best-of` | Three candidates do the same task their own way; a judge picks one |
| `pool` | Workers take tasks from the board and report to you |

`belay fleet up --preset lead` runs a preset without a file. A file can change any role:

```toml
preset = "lead"
check  = "make check"

[role.lead]
model = "opus"

[role.worker]
model         = "sonnet"
replicas      = "1..3"
may_not_write = ["tests/**"]
budget        = { tokens = "2M", wall = "45m", attempts = 3 }

[limits]
sessions = 5
tokens   = "20M"
```

### How work flows

- A task waits for the tasks it depends on (`--depends t-1,t-2`), and its worktree gets their work.
- A task counts as done only when its check passes: the fleet file's `check`, or the task's own `--check`. A failure goes back to the worker that made the change.
- With `experimental_audit = true`, a change that passes but weakens tests waits for its creator. `belay task done` accepts it, and `belay task reject` sends it back.
- Roles can be limited to paths (`may_write`, `may_not_write`). A task's `--scope` is a claim that other sessions in the same worktree may not edit.
- Token and wall-time budgets stop a session. Token counts include cache reads, so set budgets from `belay fleet report`. The fleet pauses at its token limit or the plan's usage limit; `belay fleet resume` goes on.
- belay tells sessions apart by their process tree, so one agent cannot act as another, or as you.
- When a task is done, belay keeps its worktree as it was then. The tasks that depend on it get that work, even if the session goes on editing. Each move between worktrees is in the fleet's ledger, and `belay fleet report` lists them.
- Your own terminal session can join a fleet as an attached role: `belay claude --belay-fleet --belay-name rev`.

Other commands: `belay fleet scale <role> <n>` (also a ceiling: belay starts no more sessions of that role on demand), `pause`, `resume`, `stop <slot>`, `down`, `restart` (after the hub stopped; stream sessions resume their conversations), and `prune`.

`belay fleet prune` lists the worktrees of fleets that are down; `--yes` removes them. It never removes a worktree with unsaved work. With `--branches`, it also deletes those fleets' `belay/<fleet>/*` branches that are merged into your current branch, with `git branch -d`. A branch with commits your branch lacks is listed with their count and kept. Sandbox branches (`belay/sandbox/*`) are never touched. Run `prune --yes` yourself, in your own terminal: belay refuses it elsewhere and denies it to agents.

`belay fleet down` cleans up after itself. Once the sessions have stopped, it removes each worktree that `prune --yes` would remove, unless a task in it is not finished (done or dropped). It lists what it removed and what it kept. Merge the work that is left, then run `belay fleet prune --yes`.

`belay fleet` alone lists fleets and live sessions. A fleet that is down, with no running session and no worktree left, is not listed; `belay fleet --all` shows it. The last line is the disk belay's data takes (fleets, sandboxes, sessions, build cache) and its largest item. Above 10 GB it is a warning. `belay doctor` shows the same line.

### Build cache

In a Rust repo (a `Cargo.toml` at the top), each fleet session builds into `<belay home>/cache/<repo>/<slot>/target`, not into its worktree: belay sets `CARGO_TARGET_DIR`, for the session and for its checks. The next fleet's `worker-1` starts from the last one's build. Disk use stays at one build per slot, however many fleets you run.

Two running sessions never share a build dir. cargo lets one build at a time use a dir: two cold builds of belay took 80 s on one dir and 34 s on two. Tests also run binaries from it, so one worker's tests could run another's binary.

A `CARGO_TARGET_DIR` you set, or a `target-dir` in the repo's `.cargo/config.toml`, wins. `build_cache = separate` keeps a `target/` in each worktree, and sets `RUSTC_WRAPPER=sccache` when `sccache` is on PATH and you set no wrapper. `build_cache = off` sets nothing. Delete `<belay home>/cache` at any time; the next build fills it again.

## Repo policy

A `.belay.toml` at the repo root, committed with the project, adjusts the guard for that repo:

```toml
test_paths = ["e2e/**", "checks/*.py"]           # also test files
protected = ["migrations/**", "tests/golden/**"]  # edits are guarded like test weakening
audit_ignore = ["vendor/**"]                      # left out of `belay audit`
```

An edit of a protected path is denied (with `guard = deny`) until you run `! belay allow`. Globs are relative to the repo root. `*` stays within a directory, `**` crosses them, and a pattern without `/` matches a file name anywhere. When `belay claude` starts, it skips a bad line with a warning; the other lines still apply.

## Audit (experimental)

`belay audit` reads a git diff and lists what changed in the project's checks:

- test files: assertions and tests before and after, and anything that may weaken them;
- snapshot and golden files that were rewritten;
- suppressions added (`# noqa`, `# type: ignore`, `eslint-disable`, `#[allow]`, …), and whether each one gives a reason;
- check config (CI workflows, coverage thresholds, strictness) and dependency files.

```sh
belay audit main...HEAD             # a branch
belay audit                         # HEAD to the working tree
belay audit --format markdown       # for a PR comment
```

It only reports; it never blocks. It reads the working tree through a temporary index, so your index, branch, and files are untouched. In CI, use it as a GitHub Action that writes to the job summary:

```yaml
- uses: actions/checkout@v5
  with: { fetch-depth: 0 }
- uses: prakhar479/belay@main
```

The feature is under test, so its output may change.

## Config

`~/.config/belay/config.toml` (or the file `BELAY_CONFIG` names) holds flat `key = value` lines. Every key is optional.

```toml
shape_threshold_tokens = 2000   # summarize output above this size
max_summary_ratio = 0.7         # use a summary only if it is at most this share of the original
loop_threshold = 3              # fix attempts before an unchanged failure is a loop
tighten_after = 5               # summaries without a re-open before belay summarizes harder
idle_gap_minutes = 10           # loop time leaves out gaps longer than this
keep_days = 30                  # 0 keeps every session
notify = attention              # tell you when you are needed: off, attention, all
notify_command = ~/bin/ping-me  # also run this with each notification as JSON on stdin (unset by default)
notify_quiet = 22:00-07:00      # no notifications in this window, local time (unset by default)
guard = deny                    # test guard: deny, warn, off
loops = on                      # tell the agent about loops and edit cycles
shape = on                      # summarize large test output
wrap_tests = false              # run tests through `belay run` (~1-2 s each in auto mode)
verify = on                     # ask the agent to run its tests before it finishes a turn
memory = on                     # handoffs, recall at start, learned fixes, project notes
recall_tokens = 600             # most tokens the start recall and the carry pack take
awareness = on                  # what other belay sessions here did
commons = off                   # other sessions' messages and handoffs reach the agent
report = quiet                  # how much belay tells you: quiet, normal, verbose
resume = continue               # a resumed conversation keeps belay's state; fresh starts clean
experimental_audit = false      # audit each session's changes to tests and checks
sandbox_memory = 4G             # sandbox: most memory its processes use together
sandbox_procs = 512             # sandbox: most processes
sandbox_disk = 10G              # sandbox: most disk its clone and private home use
sandbox_wall = 4h               # sandbox: longest a command runs (also 30m, 90s)
sandbox_agent = on              # agents may run `belay sandbox run` and `task`
sandbox_tasks_per_hour = 4      # most `belay sandbox task` runs an agent starts per hour
sandbox_task_tokens = 2M        # a sandbox task is stopped past this many tokens
sandbox_ro = ~/.cargo, ~/.rustup, ~/.nvm, ~/.pyenv, ~/.local/bin   # read-only in a sandbox, when they exist
build_cache = shared            # fleet cargo builds: a dir per repo and slot in belay's cache; separate, off
sandbox_build_cache = off       # sandbox cargo builds: shared binds one cache per repo in, read-write
```

`belay config` prints the config belay runs with: every key, its value, and where it came from (`file, line N`, or `default`). It also lists any line belay could not use.

### Keys from 0.1

0.2 renamed some keys: `fleet` is now `awareness`, and `learn` is now `memory`. `plugin_mode` is ignored, a `look` value reads as `on`, and `notify = true|false` reads as `notify = all|off`. Old files still load. belay says so in one line, at most once a day per config file, and again when the file changes. It never says it from a hook or the status line.

`belay config migrate` rewrites the file for you. It changes only those lines, and keeps comments, order, and keys it does not know. If the file sets both the old and the new name, the new one stays and the old line goes (it already won). It shows the diff and asks first (`--yes` skips the question). The old file is kept as `config.toml.bak-<unix time>`. A file with a line that is not `key = value` is refused, and nothing changes. Running it again changes nothing.

Fleet sessions always run with `guard = deny` and `commons = on`.

| Variable | Effect |
|---|---|
| `BELAY_HOME` | Data directory (default `~/.local/share/belay`) |
| `XDG_DATA_HOME` | Data directory base when `BELAY_HOME` is unset |
| `BELAY_CONFIG` | Config file |
| `BELAY_CLAUDE_BIN` | Agent binary (default `claude`) |
| `BELAY_HOOK_TIMEOUT_MS` | How long a hook waits for the daemon (default 250) |
| `BELAY_FILTER_TIMEOUT_MS` | How long `belay run` waits for a summary before it prints the full output |
| `BELAY_CAPTURE=1` | Also keep raw hook payloads in `hooks.jsonl` (they contain code; off by default) |

## Data and privacy

Everything stays on your machine. belay makes no model calls. It goes to the network only when you ask it to fetch a skill (`belay skill`, or the agent's `skill_preview` and `skill_stage` MCP tools).

- Session data lives in `~/.local/share/belay/sessions/<id>/`: the ledger, stored outputs, and the card.
- Stored outputs keep a copy of what tools printed. Before it writes, belay replaces secrets it recognizes with `[redacted]`: API keys and tokens, private keys, passwords in URLs, and values of variables named like secrets.
- `belay claude` deletes sessions older than `keep_days` (default 30). `belay purge --older-than 7`, `--session ID`, or `--all` deletes them by hand.
- `belay export` and `belay retro --all --summary` produce output that is safe to share.

## Troubleshooting

**belay fails open.** If belay cannot start (a bad data directory, a daemon that does not come up), `belay claude` says so and runs Claude Code without it. A hook that errors or takes longer than 250 ms prints nothing, and the agent goes on as if belay were not there. A hook costs about 2 ms at p95.

**Nothing shows in `belay report`.** It reports the newest session started in the current directory. Use `belay sessions --history` to find the one you mean, then `--session <prefix>`.

**The agent was blocked and you agree with the edit.** Run `! belay allow` in Claude Code, and the agent can retry the same edit.

**A summary hid something.** `belay show o-N` prints the original. `belay flag o-N "…"` records the miss, so it shows up in `belay review`.

**Will belay work here?** Run `belay doctor`. Each line is `ok`, `warn`, or `fail`, with one sentence and the fix. It exits 1 only on a `fail`. `belay doctor --json` prints the same checks as JSON: `{"ok": bool, "checks": [{"status", "area", "text", "fix"}]}`. It changes nothing. It checks:

- `belay`: the `belay` on PATH is this binary.
- `claude`: Claude Code runs, and its version has a conformance record (`docs/conformance/<version>.md`, baked into belay at build time). An unchecked version warns: run `scripts/conformance.py --record` from belay's source tree. No Claude Code is a `fail`.
- `home`: the data dir is a real directory, private to you.
- `config`: 0.1 names (`learn`, `fleet`, `plugin_mode`, a `look` value) and bad lines. Rename them as [Config](#config) says.
- `plugin`, `alias`, `statusline`: the plugin is installed and its version matches the binary's; `claude` starts under belay (the alias, or the plugin); the status line is belay's.
- `sandbox`: bubblewrap is installed and can make a sandbox (user namespaces work); on WSL, the layout hides `/init`, `/mnt`, and `WSL_INTEROP`; limits use a systemd user scope, or belay's watchdog alone. It runs no escape probe: `belay sandbox doctor` does. The sandbox is optional, so these only warn.
- `skills`: skills whose files differ from the lock, and skills waiting for `belay skill approve`.
- `data`: the size of belay's home, the sessions, and whether the oldest is past `keep_days`.
- `disk`: what fleets, sandboxes, sessions, and the build cache take, and the largest item. It warns above 10 GB.

**Something else.** Run `belay doctor`, then open an issue with `belay export --session -1`.

## Command reference

| Command | What it does |
|---|---|
| `belay claude [args…]` | Start Claude Code under belay. A resumed conversation (`--resume`, `-c`) continues its belay state; `--belay-fresh` starts clean |
| `belay launch <agent> [args…]` | Start an agent under belay. `belay launch claude-code` is `belay claude` |
| `belay pause [feature]` · `belay resume [feature]` | Pause belay in the live session, or resume it (yours to run) |
| `belay allow [a-N]` | Lift one guard block (yours to run) |
| `belay undo [t-N] [--yes]` | List snapshots, or roll the files back to one (yours to run) |
| `belay sandbox run [--id S] [--keep] -- <cmd…>` | Run a command in a sandbox; its changes land on `belay/sandbox/<id>` |
| `belay sandbox list \| show S \| diff S \| stop S` | The sandboxes, one in detail, its changes; stop a running one |
| `belay sandbox claude [--id S] [args…]` | A whole Claude Code session in a sandbox, supervised from outside |
| `belay sandbox task "<prompt>" [--id S] [--resume C] [--model M]` | A headless follow-up in a sandbox; prints its result and branch |
| `belay sandbox rm S [--branch]` | Remove a sandbox's clone and home (yours to run) |
| `belay sandbox doctor` | Try to escape a fresh sandbox; exit 1 if a probe gets out |
| `belay report [--session S] [--full]` | What happened in a session |
| `belay brief [--session S] [--markdown\|--json]` | What a session changed, checked, and what to look at |
| `belay why a-N` | The cause chain of an action |
| `belay show o-N` | The full original of a summarized output |
| `belay status` | One line for the status line |
| `belay watch [--session S] [--verbose]` | Print what belay does as it happens |
| `belay retro <transcript> \| --all [--summary]` | What belay would have done in past transcripts |
| `belay flag [target] ["note"]` · `belay review` | Note and list where belay got it wrong |
| `belay export --action a-N --fixture <dir>` | Save what led to an action as fixture files |
| `belay sessions [--all\|--json\|--history]` | The board of live sessions |
| `belay card [--task …] [--name …] [--join …]` | This session's card |
| `belay msg <to> "text"` · `belay inbox` | Messages between sessions and you |
| `belay share "…"` · `belay ask "…"` · `belay findings` | Findings |
| `belay handoff ["text"]` | Leave or read a handoff |
| `belay note "…"` · `belay notes` · `belay fixes` | Project notes and learned fixes |
| `belay memory [list\|search\|add\|show\|approve\|drop\|migrate]` | Memory: one store with provenance |
| `belay skill [preview\|add\|list\|update\|approve\|reject\|remove]` | Skills from URLs: checked, pinned, approved by you |
| `belay mcp` | MCP server over stdio: memory, skills, the handoff, outputs, why, and the brief |
| `belay fleet up\|status\|report\|merge\|down` | Start a fleet, watch it, merge its work, stop it |
| `belay fleet pause\|resume\|restart\|replay` | Hold or go on handing out work; start a stopped hub again; replay its records |
| `belay fleet stop\|spawn\|scale\|prune` | Stop or add sessions; remove worktrees of fleets that are down (`--branches`: also their merged branches) |
| `belay task add\|list\|show\|assign\|claim` | Put tasks on a fleet's board, read them, give them out |
| `belay task done\|blocked\|reject\|drop` | Finish a task, say it is blocked, send it back, or withdraw it |
| `belay audit [range]` | (Experimental) what a diff did to tests and checks |
| `belay install [--plugin] [--statusline] [--launcher]` · `belay uninstall` | Set belay up in Claude Code, or remove it |
| `belay config [migrate]` | Show each setting and where it comes from; rewrite 0.1 keys |
| `belay doctor` · `belay export` · `belay purge` | Install check, bug bundle, cleanup |

Every command has `--help`.
