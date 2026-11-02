kind: added

- **Undo:** belay keeps the working tree at the start and at the end of each agent turn, as private git refs. `belay undo t-N` rolls the files back to one, also what shell commands changed, without touching your index, HEAD, stash, or branches.
