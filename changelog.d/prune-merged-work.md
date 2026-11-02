kind: fixed
- `belay fleet prune` removes a worktree whose work you took another way (cherry-picked, or applied as a patch) even after your branch moved on: it checks that merging the worktree into HEAD would change nothing. Before, it kept every such worktree as unmerged.
