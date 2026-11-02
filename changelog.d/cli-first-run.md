kind: changed
- `belay --help` opens with the tagline and a "Start here" line. One-liners are short, and advanced commands (fleet, task, msg, inbox, card, mcp, retro, audit, launch) are out of the short list but still documented in their own `--help`.
- Plainer help and report wording: brief, pause, mcp, skill, report lines, and `Fingerprint:` in a skill preview.
- Empty states say what to do: "Nothing recorded yet. Run `belay claude` in a project, then try again.", `belay fleet`, and `belay usage` (no more `$-0.00`).
- `belay watch` with no live session says so and exits 1 instead of waiting forever.
- `belay doctor` is calmer: shorter lines, `~` for your home folder, one "Not set up with Claude Code yet" line on a fresh machine, and a short sleep warning (exact fix in `belay keep-awake --help`).
- `belay memory add` from an agent says "Kept m-1. It waits for your OK: `belay memory approve m-1`."
- Fleet control refusals say "yours to run: use your own terminal".
