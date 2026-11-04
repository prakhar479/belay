kind: fixed
- `kill` sent to `belay claude` just as Claude Code was starting could be lost, leaving Claude Code running without the launcher's stop. The signal now reaches Claude Code as soon as it has started.
