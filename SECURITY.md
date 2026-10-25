# Security

## Reporting a problem

Please do not open a public issue for a security problem. Report it privately through GitHub: **Security → Report a vulnerability** on this repository. You get an answer within a week.

Useful things to include: what an attacker can do, the steps to show it, and your belay and Claude Code versions (`belay doctor`).

## What belay stores and trusts

- **Local only.** belay makes no network calls and no model calls. Everything it records stays in `~/.local/share/belay` (or `BELAY_HOME`), in directories only your user can read.
- **Tool output is stored.** belay replaces secrets it recognizes with `[redacted]` before it writes anything: API keys and tokens, private keys, passwords in URLs, and values of variables named like secrets. Recognition is pattern-based, so it can miss a secret. `belay purge` deletes stored sessions.
- **Raw hook payloads** are kept only with `BELAY_CAPTURE=1`. They contain code.
- **Messages between sessions are labelled, not authenticated.** A request says which session's shell it came from, and a daemon refuses one from another session's shell. But an agent with a shell can still write into another session's inbox file. belay presents every message as information, never as an instruction from the user.
- **Some commands are yours alone.** Lifting a guard block, changing the mode, approving project notes, and forgetting learned fixes are refused when an agent runs them. They also need a control token from the session's directory. The agent runs as your user and could read that file, so this raises the bar; it is not a security boundary.
- **Fleet checks run outside the agent's permissions.** A fleet's `check` command, and a task's own `--check`, run in the worker's worktree under your user. Review the fleet file and the tasks an agent adds before you run a fleet on code you do not trust.

## Supported versions

belay is a prototype. Only the latest release gets fixes.
