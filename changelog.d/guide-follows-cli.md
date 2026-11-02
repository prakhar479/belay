kind: changed

- **The guide follows the CLI.** A test (`crates/belay-cli/tests/guide.rs`) fails when a visible command or subcommand is missing from the guide's command reference, or a config key from § Config. The guide's sections are in a new order: what belay does for one session (guard, loops, verify, summaries, memory, resume, undo) comes first, then seeing it, sandboxes, skills, sessions, and fleets.
