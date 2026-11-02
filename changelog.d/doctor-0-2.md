kind: changed

- **`belay doctor` for 0.2:** one command says whether belay will work here. Each line is `ok`, `warn`, or `fail`, with one sentence and the fix: Claude Code's version against the conformance records, the plugin's version against the binary's, the launcher alias, the status line, sandbox readiness (bubblewrap, user namespaces, WSL interop hidden, systemd scope or watchdog), skill drift and pending skills, 0.1 config names, and data size and age. `belay doctor --json` prints the same as JSON. It exits 1 only on a `fail`; the old `info` lines are now `ok` or `warn`.
