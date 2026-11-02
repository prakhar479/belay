kind: added

- **`belay keep-awake [--for D | --pid P] [-- <cmd>]`** keeps the machine awake: `systemd-inhibit` on Linux, `caffeinate` on macOS, Windows' `SetThreadExecutionState` on WSL. It says first what it can hold off here, idle sleep and a closed lid, and never claims more. belay never runs `sudo` or changes a setting; it prints the fix (logind's `HandleLidSwitch`, `pmset disablesleep`, Windows' `LIDACTION`). `belay doctor` has a new `background` line with the same.
