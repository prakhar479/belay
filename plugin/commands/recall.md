---
description: Search belay's memory and findings for a topic
argument-hint: "[topic]"
allowed-tools: Bash(belay memory search:*), Bash(belay ask:*), Bash(belay handoff), Bash(belay card)
---

Find what belay already knows that helps with the current work.

Topic: $ARGUMENTS

If the topic is empty, use the current task: run `belay card` and take the task from it.

1. Run `belay ask "<topic>"` for findings other sessions shared.
2. Search memory: the `memory_search` tool if you have it, else `belay memory search "<topic>"`.
3. Get the last handoff in this repo: the `handoff_get` tool if you have it, else `belay handoff`.

Tell the user what you found that matters for the topic, in short sentences. Say when nothing was found.
