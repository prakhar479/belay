kind: fixed

- **macOS build:** belay compiles on macOS again. The pidfd calls in the sandbox watchdog are Linux-only; other systems signal with `kill`.
- **Sandbox doctor:** the fork bomb probe counts a refusal by the systemd scope's task limit as contained, as well as a watchdog stop. A bomb that runs to the end is still reported as escaped.
