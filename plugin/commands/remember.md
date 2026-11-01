---
description: Save a fact to belay's memory for later sessions
argument-hint: "<fact to remember>"
allowed-tools: Bash(belay memory add:*)
---

Save this fact for later sessions: $ARGUMENTS

If the text is empty, ask the user what to remember and stop.

Save it with the `memory_add` tool if you have it, else run `belay memory add "<the fact>"`.

Then tell the user, in one sentence, what you saved.
