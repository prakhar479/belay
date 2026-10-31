---
description: Save a fact to belay's memory for later sessions
argument-hint: "<fact to remember>"
allowed-tools: Bash(belay memory add:*), Bash(belay share:*)
---

Save this fact for later sessions: $ARGUMENTS

If the text is empty, ask the user what to remember and stop.

Run `belay memory add "<the fact>"`. If this belay has no `belay memory` command, run `belay share "<the fact>"` instead and say so.

Then tell the user, in one sentence, what you saved.
