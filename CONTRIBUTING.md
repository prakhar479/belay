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
- [`CLAUDE.md`](CLAUDE.md) has the full layout and the data flow. Claude Code reads it when it works on belay, so it is kept exact.

## The rules

These are in `CLAUDE.md`. A change that needs to break one should say so in its pull request.

1. **Fail open.** `belay hook` always exits 0. On any error or timeout it prints nothing.
2. **belay-core has no I/O.**
3. **Never edit the past.** Act only on content before the agent sees it.
4. **Every action is recorded** with a one-sentence reason and its causes.
5. **Look mode never returns a hook response.**
6. **Deterministic.** Use `sha256_hex` for hashes. Do not let `HashMap` order reach output or the ledger.
7. **The shim stays thin.** p95 under 20 ms (measured: about 2 ms).
8. **Capability-honest.** Plan only with `Capabilities`. Downgrade; do not fail.
9. **No new dependency** in belay-core without a reason in the commit message.
10. **Parsers need fixtures.** Every parser change adds a case in `fixtures/` and a test.

## Making a change

- **Tests first:** write the test from the behavior you want, then the code.
- **Formats:** keep ledger formats backward-compatible, or bump the format and say so.
- **Errors:** use `anyhow` in the binary, and no `unwrap` on external data.
- **Regexes:** use the `re!` macro.
- **User-facing text:** short, plain sentences, one idea per sentence. Text the agent reads starts with `belay:`.
- **Docs:** update [`docs/guide.md`](docs/guide.md) when a command or behavior changes.

## Reporting bugs

Open an issue with the output of `belay doctor`. If a session went wrong, attach `belay export --session -1 > session.json`. It replaces paths, your user name, and secrets. Read it before you attach it anyway.

For a security problem, see [SECURITY.md](SECURITY.md) instead.

## License

By contributing, you agree that your contribution is licensed under MIT OR Apache-2.0, like the rest of belay.
