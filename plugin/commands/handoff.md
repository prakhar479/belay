---
description: Leave a belay handoff for the next session
argument-hint: "[extra notes]"
allowed-tools: Bash(belay handoff:*)
---

Write a handoff for the next session in this repo, from what was done in this conversation.

Use this form, one line, short plain phrases:

`done: <what is finished>; open: <what is not finished or not verified>; next: <the first step to take>`

Include these notes from the user, if any: $ARGUMENTS

Save it with `belay handoff "<the handoff>"`, then show the user the line you saved.
