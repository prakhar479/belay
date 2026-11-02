kind: changed

- **No more modes.** Look mode is gone: belay always acts within what the agent supports, and records why. Each feature has its own config key: `guard = deny|warn|off` (default `deny`), `loops`, `shape`, `memory` (was `learn`), `awareness` (was `fleet`), all `on` by default; `commons` (`off` by default: other sessions' messages and handoffs do not reach the agent, yours always do); `wrap_tests` (now `false` by default); and `report = quiet|normal|verbose`. Session cards are kept whatever `commons` says.
