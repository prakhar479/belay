kind: changed

- **Ledger format 2.** A pause or resume is a `feature_changed` entry; `mode_changed` and the session's `mode` are no longer written. 0.1 ledgers still read, and their reports say "recorded in look mode (0.1)".
