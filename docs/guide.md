# belay usage guide

belay runs next to Claude Code and watches every tool call through its hooks. It keeps a record of each session. When you ask it to, it also steps in:

- it summarizes long test output;
- it blocks edits that weaken tests;
- it tells the agent when it is stuck in a loop.

Every step it takes is written down with a one-sentence reason.

This guide goes from install to fleets. Each section stands alone, so skip to what you need.

- [Install](#install)
- [Five minutes with belay](#five-minutes-with-belay)
- [Features](#features)
- [What belay does for one session](#what-belay-does-for-one-session)
- [Reading the record](#reading-the-record)
- [Sessions that know about each other](#sessions-that-know-about-each-other)
- [Learning across sessions](#learning-across-sessions)
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

**From source** (Rust 1.87 or newer):

```sh
git clone https://github.com/prakhar479/belay && cd belay
cargo install --path crates/belay-cli    # installs `belay` into ~/.cargo/bin
```

Then check the install:

```sh
belay doctor     # which belay runs, your Claude Code version, the data dir, the config
```

`belay doctor` also says whether your Claude Code version is one belay was checked against.

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

The plugin also gives the agent a `belay` skill (when to check handoffs and findings, what a denied edit means, how to read a summarized output) and four slash commands: `/belay:report`, `/belay:handoff`, `/belay:recall`, and `/belay:remember`.

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
  - [would steer] after 13 s: belay: 2 failures are unchanged after 3 fix attempts (edit, then run):
    tests/test_api.py::test_get_order: assert 500 == 200; tests/test_api.py::test_create_order: KeyError: 'total' …
Model tokens: 180.5k prompt (169.3k cache read, 93%) · 1.1k output · 8 messages
Tool calls: 7 (Bash 4) · Bash output: 892 tokens (est.) · noise 0% (est.)
Test runs: 4 · wrapped (or would be, in auto mode): 0 (0%)
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

## Features

belay always acts within what the agent supports, and records every action with its reason. Each thing it does is a feature with its own [config](#config) key:

| Key | Default | What it does |
|---|---|---|
| `guard` | `deny` | Test guard: `deny` blocks an edit that may weaken tests until you allow it, `warn` only tells the agent, `off` leaves it |
| `loops` | `on` | Tells the agent when a failure is unchanged after several fixes, or an edit cycles |
| `shape` | `on` | Summarizes large test and build output (above `shape_threshold_tokens`) |
| `wrap_tests` | `false` | Runs test commands through `belay run`, so a failed run can be summarized too |
| `memory` | `on` | Recalls fixes other sessions learned, and the project's notes |
| `awareness` | `on` | Tells the agent what other belay sessions here did that matters to it |
| `commons` | `off` | Passes other sessions' messages, handoffs, and who is here to the agent. Your own messages always reach it |
| `report` | `quiet` | How much belay tells you about what it did: `quiet`, `normal`, `verbose` |

A feature that is off takes no action. An action the agent cannot take is recorded as not applied.

### Pause and resume

`belay pause` is the kill switch for a live session. `belay pause guard` pauses one feature; `belay pause` with no feature pauses all of them. `belay resume [feature]` turns them back on. Each pause and resume is in the ledger, and `belay status` shows what is paused. Run these yourself, in another terminal or with `!` in Claude Code: belay denies the agent's own `belay pause` and `belay resume`.

0.1 had a look mode and an optimize mode. They are gone: `belay mode` says so, and `--belay-mode` is ignored with a warning. Reports of 0.1 sessions say "recorded in look mode (0.1)" when belay acted on nothing.

## What belay does for one session

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

### The test guard

Before an Edit or Write lands in a test file, belay compares the old and new text. It flags changes that make tests easier to pass:

- assertions removed;
- skip markers added (`@pytest.mark.skip`, `it.skip`, `#[ignore]`, …);
- an expected value changed to the failing run's actual value;
- tolerances raised;
- assertions that always pass;
- commands that rewrite snapshot or golden files (`jest -u`, `--update-snapshots`, `cargo insta accept`, `UPDATE_GOLDEN=1`, …).

With `guard = deny` (the default) the edit is denied, and the agent reads why:

```text
belay: this edit may weaken tests in tests/test_discount.py: expected value changed to the failing
run's actual value (19.99 → 19.98). If the user wants it anyway, they can run `! belay allow` and
you can retry the same edit.
```

Only you can lift a block: type `! belay allow` in Claude Code, or run `belay allow` in a terminal. belay tells an agent's own `belay allow` apart from yours and refuses it. A shell command that writes a test file cannot be stopped before it runs, so belay checks the file after the command and tells the agent if it may have weakened tests.

### Loops

When the same failure comes back after `loop_threshold` (default 3) rounds of edit-then-run, belay says so once:

```text
belay: 2 failures are unchanged after 3 fix attempts (edit, then run): tests/test_api.py::test_get_order:
assert 500 == 200; … A different approach may be needed.
```

It compares failures by a normalized signature, so line numbers, addresses, and timings do not hide a repeat. It also notices edit cycles (`edit, revert, edit`). With `notify = true` you also get a desktop notification when belay steers a loop or blocks an edit, which helps in long unattended runs.

## Reading the record

Each session is a ledger: numbered entries, outputs `o-N`, and actions `a-N`. Every action has a reason and the entries that caused it.

```sh
belay report                     # the newest session started here
belay report --session -2 --full # the one before it, every section
belay why a-2                    # the cause chain of an action
belay show o-3                   # the full original of a summarized output
belay sessions --history         # every recorded session
```

`--session` takes an id, a unique prefix, `-1` for the newest, `-2`, and so on, or a path.

If belay got something wrong, note it where it happened. The notes collect in one list:

```sh
belay flag a-2 "summary hid the error"
belay review                     # every flag, with what it points at
```

To report a bug, `belay export --session -1 > s.json` writes one session with paths, your user name, and secrets replaced.

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

`belay handoff` with no text shows what the next session would get. With `commons = on` the agent hears it at start.

### Awareness notices

Each daemon reads the other live sessions' ledgers once a second and notices four things:

- **Peer edit.** In the same checkout, another session edited a file after this one did.
- **Peer git.** In the same checkout, another session ran a git command that changes files or HEAD (`stash`, `checkout`, `reset`, `commit`, …).
- **Peer failure.** A failed run names a file another session in this checkout edited in the last 30 minutes.
- **Worktree overlap.** In another worktree of the same repo, both sessions edited one file, so a merge will conflict there.

With `awareness = on` (the default), the agent gets these at its next hook. For example: "belay: shop-1 edited shop/payments.py after you did. Read it again before you change it." They are also shown in `belay report`.

## Learning across sessions

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

## Memory

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

Notes, learned fixes, findings, and the handoffs of ended sessions show up in `belay memory` as they are, with ids `note-N`, `fix-…`, `f-N`, and `h-…`. `belay memory migrate` copies them into the store and backs up the old files first. Running it again copies nothing new. The old commands keep working.

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

Other commands: `belay fleet scale <role> <n>` (also a ceiling: belay starts no more sessions of that role on demand), `pause`, `resume`, `stop <slot>`, `down`, `restart` (after the hub stopped; stream sessions resume their conversations), and `prune`. `prune` never removes a worktree with unsaved work.

## Repo policy

A `.belay.toml` at the repo root, committed with the project, adjusts the guard for that repo:

```toml
test_paths = ["e2e/**", "checks/*.py"]           # also test files
protected = ["migrations/**", "tests/golden/**"]  # edits are guarded like test weakening
audit_ignore = ["vendor/**"]                      # left out of `belay audit`
```

An edit of a protected path is denied (with `guard = deny`) until you run `! belay allow`. Globs are relative to the repo root. `*` stays within a directory, `**` crosses them, and a pattern without `/` matches a file name anywhere. When `belay claude` starts, it skips a bad line with a warning; the other lines still apply.

## Undo

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
notify = false                  # desktop notification when belay steers a loop or blocks an edit
guard = deny                    # test guard: deny, warn, off
loops = on                      # tell the agent about loops and edit cycles
shape = on                      # summarize large test output
wrap_tests = false              # run tests through `belay run` (~1-2 s each in auto mode)
memory = on                     # learned fixes and project notes
awareness = on                  # what other belay sessions here did
commons = off                   # other sessions' messages and handoffs reach the agent
report = quiet                  # how much belay tells you: quiet, normal, verbose
experimental_audit = false      # audit each session's changes to tests and checks
```

0.1 config files still load, with a warning for each old key: `look` reads as `on`, `fleet` is now `awareness`, `learn` is now `memory`, and `plugin_mode` is ignored. Fleet sessions always run with `guard = deny` and `commons = on`.

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

Everything stays on your machine. belay makes no network calls and no model calls.

- Session data lives in `~/.local/share/belay/sessions/<id>/`: the ledger, stored outputs, and the card.
- Stored outputs keep a copy of what tools printed. Before it writes, belay replaces secrets it recognizes with `[redacted]`: API keys and tokens, private keys, passwords in URLs, and values of variables named like secrets.
- `belay claude` deletes sessions older than `keep_days` (default 30). `belay purge --older-than 7`, `--session ID`, or `--all` deletes them by hand.
- `belay export` and `belay retro --all --summary` produce output that is safe to share.

## Troubleshooting

**belay fails open.** If belay cannot start (a bad data directory, a daemon that does not come up), `belay claude` says so and runs Claude Code without it. A hook that errors or takes longer than 250 ms prints nothing, and the agent goes on as if belay were not there. A hook costs about 2 ms at p95.

**Nothing shows in `belay report`.** It reports the newest session started in the current directory. Use `belay sessions --history` to find the one you mean, then `--session <prefix>`.

**The agent was blocked and you agree with the edit.** Run `! belay allow` in Claude Code, and the agent can retry the same edit.

**A summary hid something.** `belay show o-N` prints the original. `belay flag o-N "…"` records the miss, so it shows up in `belay review`.

**Something else.** Run `belay doctor`, then open an issue with `belay export --session -1`.

## Command reference

| Command | What it does |
|---|---|
| `belay claude [args…]` | Start Claude Code under belay |
| `belay launch <agent> [args…]` | Start an agent under belay. `belay launch claude-code` is `belay claude` |
| `belay pause [feature]` · `belay resume [feature]` | Pause belay in the live session, or resume it (yours to run) |
| `belay allow [a-N]` | Lift one guard block (yours to run) |
| `belay undo [t-N] [--yes]` | List snapshots, or roll the files back to one (yours to run) |
| `belay report [--session S] [--full]` | What happened in a session |
| `belay why a-N` | The cause chain of an action |
| `belay show o-N` | The full original of a summarized output |
| `belay status` | One line for the status line |
| `belay retro <transcript> \| --all [--summary]` | What belay would have done in past transcripts |
| `belay flag <target> "note"` · `belay review` | Note and list where belay got it wrong |
| `belay sessions [--all\|--json\|--history]` | The board of live sessions |
| `belay card [--task …] [--name …] [--join …]` | This session's card |
| `belay msg <to> "text"` · `belay inbox` | Messages between sessions and you |
| `belay share "…"` · `belay ask "…"` · `belay findings` | Findings |
| `belay handoff ["text"]` | Leave or read a handoff |
| `belay note "…"` · `belay notes` · `belay fixes` | Project notes and learned fixes |
| `belay memory [list\|search\|add\|show\|approve\|drop\|migrate]` | Memory: one store with provenance |
| `belay fleet up\|status\|report\|merge\|down\|…` | Fleets |
| `belay task add\|list\|show\|claim\|done\|blocked\|drop` | A fleet's task board |
| `belay audit [range]` | (Experimental) what a diff did to tests and checks |
| `belay install [--plugin] [--statusline] [--launcher]` · `belay uninstall` | Set belay up in Claude Code, or remove it |
| `belay doctor` · `belay export` · `belay purge` | Install check, bug bundle, cleanup |

Every command has `--help`.
