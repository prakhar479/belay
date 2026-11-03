kind: fixed
- `belay fleet prune` now recognizes work you took another way (cherry-pick, patch) on a machine where belay has not stored anything yet. Before, the check needed belay's home folder to exist, and kept the worktree.
