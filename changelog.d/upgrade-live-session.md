kind: fixed

- **Upgrading belay during a live session no longer breaks test runs.** A session started before the upgrade wrapped test commands through its own binary, which the upgrade had replaced, so every wrapped command failed to start. belay now wraps only through a binary that is still on disk, and otherwise lets the command run as written.
- `belay doctor` points to `belay config migrate` for 0.1 config names.
