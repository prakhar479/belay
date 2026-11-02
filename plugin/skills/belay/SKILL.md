---
name: belay
description: How to work with belay, the layer outside the conversation that remembers, checks, and records this session. Use at the start of work in a repo, when a test failure repeats, when belay denies an edit or summarizes an output, and before you stop.
---

# Working with belay

belay records what you do in this session. It may summarize long outputs, warn you about loops, and deny edits that weaken tests. Every action it takes has a reason you can look up. Run the commands below with your shell tool.

belay also has MCP tools: `memory_search`, `memory_add`, `handoff_get`, `output_show`, `why`, `brief`, `skill_preview`, `skill_stage`, `sandbox_run`, `sandbox_task`, and `bg_start`. When they are present, use them instead of the matching commands.

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

## When a belay warning is wrong

- belay can be wrong. If you think a warning or a block is wrong, say so in one line: `belay flag a-N "why"`, with the id from the warning.
- This only records your view for the user to review. It does not lift the block or allow the edit. Do not retry the edit because you flagged it.

## When an output is summarized

- belay keeps every failure line, but may cut passing lines. The summary names an id like `o-3`.
- To read the full original: `output_show` with `o-3`, or `belay show o-3`.
- To see why belay did something (an action `a-N`): `why` with `a-N`, or `belay why a-N`.

## When you summarize your work

- Before you tell the user what you did, read `brief` (or `belay brief`). It lists the files changed, each test command and whether it passed after your last edit, and what a reviewer should look at.
- Say what it says. If a check is ⚠ (not run since the last edit) or ✗ (failing), say so, or run it first.

## When you learn something useful

- Share a fact other sessions can use: `belay share "<fact>"`.
- Save it for later sessions: `memory_add`, or `belay memory add "<fact>"`.

Project conventions and instructions belong in CLAUDE.md, the user's file. Do not copy them into belay memory. belay memory is for what happened: handoffs, fixes, and gotchas you found while working. `belay memory add` refuses a line CLAUDE.md already has.

## When a skill would help

- Look at it first: `skill_preview`, or `belay skill preview <url>`. Read its warnings.
- Stage it: `skill_stage`, or `belay skill add <url>`. This installs nothing.
- Ask the user to run `belay skill approve <name>` in their own terminal. Only the user can approve, reject, or remove a skill.
## Sandboxes

A sandbox is a clone of this repo with a private home and limits. Nothing in it reaches the user's other files. Its work comes back as branch `belay/sandbox/<id>`, which the user reads and merges. Use one:

- for a risky command or an experiment you would not run in the user's checkout: `sandbox_run`, or `belay sandbox run -- <cmd>`;
- for a follow-up the user wants contained: `sandbox_task` with a prompt, or `belay sandbox task "<prompt>"`. It returns the result, the branch, and the files changed.

Tasks are limited per hour and by tokens. Never merge a sandbox branch yourself; tell the user its name and `belay sandbox show <id>`.

## When the user leaves

- If the user says they are leaving, or asks you to keep going without them, offer `bg_start` with a short prompt that restates the task in full. It runs in a sandbox, with a budget, and tells the user when it is done or needs them.
- Tell the user its id. Stopping it or taking it over is theirs: `belay bg status <id>`, `belay bg attach <id>`.

## Before you stop

Leave a handoff for the next session: `belay handoff "done: …; open: …; next: …"`.
