---
name: belay
description: How to work with belay, the local supervisor that records this session. Use at the start of work in a repo, when a test failure repeats, when belay denies an edit or summarizes an output, and before you stop.
---

# Working with belay

belay records what you do in this session. It may summarize long outputs, warn you about loops, and deny edits that weaken tests. Every action it takes has a reason you can look up. Run the commands below with your shell tool.

belay also has MCP tools: `memory_search`, `memory_add`, `handoff_get`, `output_show`, and `why`. When they are present, use them instead of the matching commands.

## When you start

- Read the last handoff for this repo: `handoff_get`, or `belay handoff`.
- Look for what is already known about your task: `memory_search`, or `belay memory search "<topic>"`. Also run `belay ask "<topic>"`.
- Say what you work on: `belay card --task "<short task>"`. Update it when the task changes.

## When a test failure repeats

- If the same failure comes back after a fix, stop and read it again. Do not repeat the same edit.
- Search for a known fix with the error text: `memory_search`, or `belay memory search "<error text>"`. Also run `belay ask "<error text>"`.
- If belay says you are in a loop, change your approach or ask the user.

## When belay denies an edit

- belay denied it because the edit may weaken a test: an assertion removed, a test skipped, or an expected value changed to the failing value.
- Never try to get around it. Do not write the same change another way, through the shell, or in another file.
- Fix the code under test instead. If you think the test itself is wrong, explain why to the user.
- If the user agrees, ask them to run `! belay allow`. Then retry the same edit.

## When an output is summarized

- belay keeps every failure line, but may cut passing lines. The summary names an id like `o-3`.
- To read the full original: `output_show` with `o-3`, or `belay show o-3`.
- To see why belay did something (an action `a-N`): `why` with `a-N`, or `belay why a-N`.

## When you learn something useful

- Share a fact other sessions can use: `belay share "<fact>"`.
- Save it for later sessions: `memory_add`, or `belay memory add "<fact>"`.

## Before you stop

Leave a handoff for the next session: `belay handoff "done: …; open: …; next: …"`.
