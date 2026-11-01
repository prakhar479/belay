# Sandboxes: threat model

`belay sandbox` runs a command, a whole agent session, or a headless agent task, so that a mistake or a bad
instruction cannot reach past the work it was given. This page says what a sandbox
protects, what it does not, and how it is built. The usage is in the
[guide](guide.md#sandboxes).

## The rule: fail closed

A sandbox is built as specified, or nothing runs. Most of belay fails open: a hook that
breaks lets the agent go on. A sandbox is the exception. If belay cannot build it, it
refuses with the reason and runs nothing. Reasons include no bubblewrap, user
namespaces turned off, no git repo, a clone that cannot be made, a limit that cannot
be set, a `sandbox_ro` entry that would show the home, or (for an agent session) a
supervising daemon that does not start.

## What it protects

- **Host files outside the sandbox.** The command sees its clone of the repo
  (`<belay home>/sandboxes/<id>/work`) and its private home
  (`<belay home>/sandboxes/<id>/home`), both read-write. It also sees the system dirs,
  the configured toolchain dirs, and the repo's object store (`.git/objects`),
  read-only. Nothing else of the host's file system is there. `/tmp` and `/run` are
  empty and private.
- **Other processes.** The command runs in its own pid, IPC, UTS, and user namespaces
  (and cgroup namespace, where possible). It cannot see or signal host processes. That
  includes belay's daemon, which supervises from outside and is reached only through
  the session socket bound into the sandbox.
- **The Windows host, on WSL.** `/mnt` (the Windows drives) and `/init` (WSL's
  interop, which runs `.exe` files on Windows) are not in the sandbox. Neither is
  `/run/WSL`, and `WSL_INTEROP` is not passed. A Windows program copied into the
  clone does not run.
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
  use of the clone and home (`sandbox_disk`, 10G), or wall time (`sandbox_wall`, 4h).
  It sends SIGTERM, then SIGKILL after 2 s, to the sandbox's outer process. The pid
  namespace then takes down everything inside. The stop and its reason are recorded.
  Inside, core files are off and no single file may grow past the disk cap (rlimits set
  before exec). Where `systemd-run --user --scope` works, the sandbox also runs in a
  scope with `MemoryMax` and `TasksMax`.
- **The host's git.** The clone's `.git` is the sandbox's: it can write hooks, config,
  and attributes there. belay never runs git on the host in a way that reads them with
  effect. See "Getting the work out".
- **The supervisor.** In `belay sandbox claude` and `belay sandbox task`, belay's
  daemon and launcher run outside. The agent reaches the daemon only through the
  session socket. The pid namespace keeps it from signalling them.
- **Runaway follow-ups.** An agent may start at most `sandbox_tasks_per_hour` (4)
  sandbox tasks per session, and a task is stopped past `sandbox_task_tokens` (2M
  tokens: input, output, and cache writes; cache reads are not counted).

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
- **The repo's committed files.** The clone borrows the repo's object store, read-only,
  so the sandbox can read every object in the repo, including other branches' commits.
  It does not see the repo's refs, config, or hooks.
- **The agent's conversation.** `belay sandbox task --resume` copies that conversation's
  transcript into the private home: the agent inside reads all of it.
- **Permission prompts in a task.** A headless task runs its agent without permission
  prompts (for Claude Code, `bypassPermissions`). The sandbox is the boundary; belay's
  guard still runs through the hooks.

## Layout (bubblewrap)

```
bwrap --unshare-user --unshare-pid --unshare-ipc --unshare-uts --unshare-cgroup-try
      --die-with-parent --new-session
      --ro-bind /usr /bin /sbin /lib /lib64 /etc     (each when it exists)
      --tmpfs /tmp --tmpfs /run --proc /proc --dev /dev
      --bind <home> <home> --bind <work> <work>
      --ro-bind <the repo's objects dir>, <each sandbox_ro dir>,
                <the agent's files and dirs in home>, <the belay binary>,
                <the session socket>
      [agent sessions: --ro-bind <the agent's program> <the belay session dir>]
      --chdir <work>/<same subdir> --clearenv --setenv … -- <cmd…>
```

`<work>` is `git clone --shared --no-checkout` of the repo, checked out at the base
commit on branch `belay/sandbox/<id>`, with no remote. Its `.git` is a whole repo of its
own; `--shared` makes it borrow the repo's objects through
`.git/objects/info/alternates`. belay reads that path once, when it makes the clone,
and binds that one dir read-only at its own path. A later change to the alternates
file inside does not change what is bound. If you `git gc --prune` the repo while a
sandbox's base commit is on no branch of yours, the clone can lose objects; merge or
remove the sandbox first.

**A whole session** (`belay sandbox claude`, `belay sandbox task`):

```
host:     belay (launcher) ── belay daemon ── <session dir>/belayd.sock
             │ watchdog, ledger, card
             └─ bwrap … -- <agent program> --settings <session dir>/claude-settings.json …
sandbox:        agent ── belay hook <Event> ──(bound socket)──▶ daemon outside
```

The agent's hooks run `belay hook` inside; the shim only forwards, so the daemon outside
decides as in any session. The session dir is bound read-only (the agent reads its
settings there). The agent's program is resolved on the host and bound at its real
path. The hooks' `cwd` is the clone's path, which is the same inside and out, so the
ledger and `belay why` read as in any session. belay runs no git in the clone from
outside: undo snapshots and the session audit skip it (`belay sandbox show` audits the
branch instead). `[Verify]` Claude Code's terminal UI under `--new-session`: bwrap
puts the sandbox in a new session, so it has no controlling terminal (this stops
`TIOCSTI` input injection into your shell). The UI reads keys from stdin in raw mode,
which works without one, but a resize signal may not reach it.

## Getting the work out

When the command or agent ends, belay brings its work into the repo in two steps.

1. **Commit, inside.** belay runs `git add -A` and `git commit` in the sandbox, with
   `core.hooksPath=/dev/null`, `core.fsmonitor=false`, `commit.gpgSign=false`, and
   `--no-verify`. Filters named in the clone's config or `.gitattributes` run there,
   inside, where they reach nothing. The author is the repo's `user.name` and
   `user.email` (read from the repo, which is yours), else `belay`.
2. **Fetch, from outside.** belay runs, in the repo:

   ```
   git -c core.hooksPath=/dev/null -c core.fsmonitor=false -c protocol.file.allow=always \
       fetch --no-tags --no-recurse-submodules --no-write-fetch-head \
       --upload-pack '<bwrap … the clone, home, objects …> git upload-pack' \
       <work> +refs/heads/belay/sandbox/<id>:refs/heads/belay/sandbox/<id>
   ```

   The fetch's receiving side runs on the host and reads only the repo's own config.
   Its sending side, `upload-pack`, would read the clone's config, so it runs inside
   the sandbox. A `.git` that is a symlink or a `gitdir:` file resolves inside too.
   Only the one branch is updated.

Why not run `upload-pack` on the host? Checked with git 2.43: `upload-pack` and the
`pack-objects` it starts read `uploadpack.*`, `pack.*`, and `core.*` keys from the
clone. `uploadpack.packObjectsHook`, the one that names a program, is honored only from
system, global, or command-line config, not from the repo's. `core.fsmonitor`,
`core.alternateRefsCommand`, `core.pager`, `core.sshCommand`, filters, and hooks are not
run by either. A fetch with all of these planted in the clone ran none of them on the
host. That holds for this git; a later git may read more. Running `upload-pack` inside
keeps the answer the same whatever it reads.

Committing from outside is not safe: `git add` on the host runs the clean filter that
the clone's config names (checked: `filter.<x>.clean` and `filter.<x>.process` ran on
the host even with `-c core.hooksPath=/dev/null`). That is why the commit runs inside.
For the same reason, belay's own git helpers refuse any path in a sandbox's clone.

The files listed as changed come from the repo, after the fetch (`git diff` between the
branch before and after), not from anything the sandbox says.

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
| write a file next to the clone, in the real home, and in `/tmp` | no file appears on the host |
| read `~/.ssh`, `~/.aws`, `~/.config/gcloud`, `~/.gnupg`, `~/.netrc` | none is visible |
| `kill -0` the shell that ran belay | the pid is not reachable |
| on WSL: run `cmd.exe /c ver`, and a copy of `cmd.exe` in the worktree | Windows does not answer |
| find a Docker socket | there is none |
| plant hooks and config that name programs (`core.fsmonitor`, `core.hooksPath`, `uploadpack.packObjectsHook`, `core.alternateRefsCommand`, `gpg.program` with `commit.gpgSign`, a clean filter) in the clone's `.git`, then let belay bring the work out | none runs on the host, and the work reaches the branch |
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
