# Sandboxes: threat model

`belay sandbox` runs a command, or later an agent, so that a mistake or a bad
instruction cannot reach past the work it was given. This page says what a sandbox
protects, what it does not, and how it is built. The usage is in the
[guide](guide.md#sandboxes).

## The rule: fail closed

A sandbox is built as specified, or nothing runs. Most of belay fails open: a hook that
breaks lets the agent go on. A sandbox is the exception. If belay cannot build it, it
refuses with the reason and runs nothing. Reasons include no bubblewrap, user
namespaces turned off, no git repo, a worktree that cannot be made, a limit that cannot
be set, or a `sandbox_ro` entry that would show the home.

## What it protects

- **Host files outside the sandbox.** The command sees its worktree
  (`<belay home>/sandboxes/<id>/work`) and its private home
  (`<belay home>/sandboxes/<id>/home`), both read-write. It also sees the system dirs
  and the configured toolchain dirs, read-only. Nothing else of the host's file system
  is there. `/tmp` and `/run` are empty and private.
- **Other processes.** The command runs in its own pid, IPC, UTS, and user namespaces
  (and cgroup namespace, where possible). It cannot see or signal host processes. That
  includes belay's daemon, which supervises from outside and is reached only through
  the session socket bound into the sandbox.
- **The Windows host, on WSL.** `/mnt` (the Windows drives) and `/init` (WSL's
  interop, which runs `.exe` files on Windows) are not in the sandbox. Neither is
  `/run/WSL`, and `WSL_INTEROP` is not passed. A Windows program copied into the
  worktree does not run.
- **Home secrets.** `~/.ssh`, cloud credentials, other repos, and the rest of the home
  are not visible. The home is a private directory. It holds read-only copies of the
  agent's config, the files the agent's connector names. For Claude Code that is
  `~/.claude.json`, `~/.claude/.credentials.json`, and `~/.claude/settings.json`, with
  the plugins directory bound read-only. The environment is cleared, then an allowlist
  is passed: `PATH`, `LANG`, `LC_*`, `TERM`, `USER`, `SHELL`, `TZ`, `BELAY_*`, and the
  agent's API and auth variables. These are never passed: `SSH_AUTH_SOCK`, `DISPLAY`,
  `WAYLAND_DISPLAY`, `DBUS_SESSION_BUS_ADDRESS`, `WSL_INTEROP`.
- **Sockets that lead out.** The Docker socket, the ssh agent, X11 and Wayland, and
  D-Bus are not in the sandbox: `/var` is not bound, and `/tmp` and `/run` are empty.
- **Resource exhaustion.** belay's watchdog runs outside the sandbox. It follows the
  sandbox's process tree through `/proc` and stops it when one of these limits is
  passed: total memory (`sandbox_memory`, 4G), processes (`sandbox_procs`, 512), disk
  use of the worktree and home (`sandbox_disk`, 10G), or wall time (`sandbox_wall`, 4h).
  It sends SIGTERM, then SIGKILL after 2 s, to the sandbox's outer process. The pid
  namespace then takes down everything inside. The stop and its reason are recorded.
  Inside, core files are off and no single file may grow past the disk cap (rlimits set
  before exec). Where `systemd-run --user --scope` works, the sandbox also runs in a
  scope with `MemoryMax` and `TasksMax`.
- **The host's git.** belay commits the sandbox's work from outside. It uses the
  worktree's git dir from its own record, not the worktree's `.git` file, which the
  sandbox can rewrite. Hooks and fsmonitor are off for these commands. belay then writes
  the `.git` file back as it made it.

## What it does not protect

- **The network.** The sandbox shares the host's network (owner decision, 2026-11-01).
  Anything the host reaches, the command reaches. That includes services on localhost:
  databases, a Docker daemon on a TCP port, dev servers, cloud metadata endpoints.
- **Abstract Unix sockets.** Linux ties abstract-namespace sockets (names that start
  with a NUL byte, shown as `@name`) to the network namespace, not to the filesystem.
  The sandbox shares the host's network, so it can reach them although `/tmp` and
  `/run` are private. Some X11 servers, D-Bus setups, and agents listen on one. `ss -xl
  | grep ' @'` lists them on the host.
- **The agent's own credentials.** The agent needs its API key or login to work, so
  they are inside, readable by any command the agent runs.
- **The kernel.** A kernel exploit escapes any namespace sandbox. Keep the kernel
  patched.
- **What you merge.** The work lands on branch `belay/sandbox/<id>`. Read it before you
  merge it (`belay sandbox show` runs `belay audit` on it, and `belay sandbox diff`
  prints it). Code that runs at build or test time on the host after a merge runs with
  your rights.
- **The repo's committed files.** The worktree starts from HEAD, so the sandbox reads
  everything committed in the repo, including other branches' objects that HEAD shares.
- **git filters.** `.gitattributes` in the worktree can name a filter that your git
  config defines (such as Git LFS). belay's commit from outside runs that filter on the
  sandbox's files.

## Layout (bubblewrap)

```
bwrap --unshare-user --unshare-pid --unshare-ipc --unshare-uts --unshare-cgroup-try
      --die-with-parent --new-session
      --ro-bind /usr /bin /sbin /lib /lib64 /etc     (each when it exists)
      --tmpfs /tmp --tmpfs /run --proc /proc --dev /dev
      --bind <home> <home> --bind <work> <work>
      --ro-bind <each sandbox_ro dir>, <the agent's files and dirs in home>,
                <the belay binary>, <the session socket>
      --chdir <work>/<same subdir> --clearenv --setenv … -- <cmd…>
```

No overlay is used. There is no `--unshare-net`. `/mnt`, `/init`, `/var`, `/home`, and
`/root` are not bound. The toolchain dirs (`~/.cargo`, `~/.rustup`, `~/.nvm`, `~/.pyenv`,
`~/.local/bin` by default) are bound read-only at their own paths. `RUSTUP_HOME`,
`NVM_DIR`, and `PYENV_ROOT` point at them, so the tools find them although `HOME` has
moved. `sandbox_ro` may not name `/`, the home or a dir above it, or belay's data.

Only Linux with bubblewrap is supported. A sandbox for macOS (`sandbox-exec`) comes
later. Anywhere else belay refuses.

## Checking a machine

`belay sandbox doctor` makes a fresh sandbox of a scratch repo and tries to escape it.
Each try must be contained:

| Probe | Contained when |
|---|---|
| write a file next to the worktree, in the real home, and in `/tmp` | no file appears on the host |
| read `~/.ssh`, `~/.aws`, `~/.config/gcloud`, `~/.gnupg`, `~/.netrc` | none is visible |
| `kill -0` the shell that ran belay | the pid is not reachable |
| on WSL: run `cmd.exe /c ver`, and a copy of `cmd.exe` in the worktree | Windows does not answer |
| find a Docker socket | there is none |
| a fork bomb (bounded at 2000) | the watchdog stops it at the process limit (64 here) |
| a memory hog | the watchdog stops it at the memory limit (256M here) |

The doctor exits 1 if any probe escapes.

## Observed facts

- On WSL 2 (kernel 5.15), the `WSLInterop` binfmt entry has flags `PF`, not `F`. The
  kernel opens its interpreter, `/init`, when the program runs. A `.exe` run inside the
  sandbox still reaches WSL's `/init`, which prints an error and exits 1 without
  starting the Windows program. It cannot reach the interop socket in `/run/WSL`, and
  `WSL_INTEROP` is unset. `[Verify]` on newer WSL builds with `belay sandbox doctor`. A
  binfmt entry with flag `F` would keep `/init` open across namespaces.
- `systemd-run --user --scope` fails on WSL without systemd ("Failed to connect to
  bus"). The `/proc` watchdog is then the only limit on memory and processes.
