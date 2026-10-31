---
description: Search belay's memory and findings for a topic
argument-hint: "[topic]"
allowed-tools: Bash(belay memory search:*), Bash(belay ask:*), Bash(belay handoff), Bash(belay card)
---

Find what belay already knows that helps with the current work.

Topic: $ARGUMENTS

If the topic is empty, use the current task: run `belay card` and take the task from it.

1. Run `belay ask "<topic>"` for findings other sessions shared.
2. If this belay has `belay memory`, run `belay memory search "<topic>"`. If the command does not exist, skip it.
3. Run `belay handoff` for the last handoff in this repo.

Tell the user what you found that matters for the topic, in short sentences. Say when nothing was found.
