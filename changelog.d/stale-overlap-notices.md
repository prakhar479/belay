kind: fixed

- **No stale worktree-overlap notices.** A peer's edit no longer draws "merging both branches will conflict" once its version of the file is in this session's HEAD, or once the peer has ended and its worktree is removed. A notice about a peer that has ended says so, once per file. belay checks this off the hook path, with one git call per new peer edit or HEAD move. New peer activities `merged` and `removed` in the ledger (older belay versions skip those lines).
