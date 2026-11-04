# Contributing to belay

Thanks for helping. belay is a small project with strict rules. This page explains them.

## Set up

You need Rust 1.87 or newer, git, and Python 3 for a few tests.

```sh
git clone https://github.com/prakhar479/belay && cd belay
make check          # fmt --check, clippy -D warnings, every test
make check-clean    # the same with an empty HOME and a minimal PATH, as CI runs it
```

Other useful commands:

```sh
cargo test -p belay-core     # the fast loop for core logic
make e2e                     # end-to-end tests with a fake agent
cargo run -- retro fixtures/transcripts/loop_session.jsonl
cargo test --release -p belay-cli --test e2e -- --ignored shim_latency   # the hook budget
```

## How the code is laid out

- `crates/belay-core` is pure logic. It does no I/O at all: no files, sockets, environment, clocks, threads, or model calls. Time comes in as `now_ms`.
- `crates/belay-cli` does all the I/O. It holds the CLI, the daemon, the hook shim, the store, and the fleet hub.
- [`docs/design.md`](docs/design.md) explains how the pieces fit. [`CLAUDE.md`](CLAUDE.md) has the full layout and the data flow. Claude Code reads it when it works on belay, so it is kept exact.

## The rules

These are in `CLAUDE.md`. A change that needs to break one should say so in its pull request.

1. **Fail open.** `belay hook` always exits 0. On any error or timeout it prints nothing. The one exception is a sandbox: it **fails closed**. If it cannot be built as asked, nothing runs.
2. **belay-core has no I/O.**
3. **Never edit the past.** Act only on content before the agent sees it.
4. **Every action is recorded** with a one-sentence reason and its causes.
5. **Reporting never reaches the model.** Notes for the user go through channels only the user sees. Only actions reach the agent.
6. **Deterministic.** Use `sha256_hex` for hashes. Do not let `HashMap` order reach output or the ledger.
7. **The shim stays thin.** p95 under 20 ms (measured: about 2 ms at the median, 3 ms at p95).
8. **Capability-honest.** Plan only with `Capabilities`. Downgrade; do not fail.
9. **No new dependency** in belay-core without a reason in the commit message.
10. **Parsers need fixtures.** Every parser change adds a case in `fixtures/` and a test.
11. **Network only on request.** Only `belay skill` makes a network call, through `git` and `curl`. The hook path, the daemon, and the core never touch the network.
12. **Users approve what lasts or escapes.** `allow`, `pause|resume`, `undo`, `memory|notes approve|drop`, `findings drop`, `fixes forget`, `skill approve|reject|remove`, `sandbox rm`, `bg stop|attach`, and `fleet prune --yes` refuse outside the user's terminal, and the agent's run of them is denied (`controller/belay_cmd.rs`).

## Making a change

- **Tests first:** write the test from the behavior you want, then the code.
- **Formats:** keep ledger formats backward-compatible, or bump the format and say so.
- **Errors:** use `anyhow` in the binary, and no `unwrap` on external data.
- **Regexes:** use the `re!` macro.
- **User-facing text:** short, plain sentences, one idea per sentence. Text the agent reads starts with `belay:`.
- **Docs:** update [`docs/guide.md`](docs/guide.md) when a command or behavior changes, in the same commit.
- **Changelog:** add a fragment `changelog.d/<short-name>.md`: a first line `kind: added|changed|fixed|removed`, then bullets. Don't edit `CHANGELOG.md`. `python3 scripts/changelog.py --check` validates it.
- **Live checks** (they call a model, so they're not in `make check`): `scripts/conformance.py` checks the Claude Code behaviors belay relies on, and `scripts/skill_live.py` checks that an agent uses belay unasked.

## Reporting bugs

Open an issue with the output of `belay doctor`. If a session went wrong, attach `belay export --session -1 > session.json`. It replaces paths, your user name, and secrets. Read it before you attach it anyway.

For a security problem, see [SECURITY.md](SECURITY.md) instead.

## License

By contributing, you agree that your contribution is licensed under the MIT license, like the rest of belay.
