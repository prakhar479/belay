---
description: Summarize this session for review, from belay's record
allowed-tools: Bash(belay brief:*)
---

Run `belay brief` and read its output. It comes from belay's ledger and git, not from what you said you did.

Then tell the user, in a few plain sentences:

- what changed;
- which tests and checks passed after the last edit, and which did not run since (⚠) or fail (✗);
- what to look at first, with its id (`a-N`, an entry number, or `git`).

If a check is ⚠ or ✗, say so first. Do not claim more than the brief shows.

For a pull request description, run `belay brief --markdown` and give the user its section as is.
