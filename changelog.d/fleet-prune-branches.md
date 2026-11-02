kind: changed

- **`belay fleet prune --branches`** also deletes the merged `belay/<fleet>/*` branches of fleets that are down (`git branch -d`, never `-D`). Unmerged branches are listed with how many commits HEAD lacks, and kept. Sandbox branches are not touched. `prune --yes` is now the user's: refused outside your own terminal and denied to agents. `belay fleet` no longer lists fleets that are down and pruned; `belay fleet --all` does.
