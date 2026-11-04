kind: fixed
- Verify before done names the build or check itself (`cargo build`, `make check`), not the whole line it was part of. Before, after `cargo build -q | tail; ls; grep …` it asked the agent to re-run all of it. Loops also count `cargo build` and `cargo build -q | tail` as the same run.
