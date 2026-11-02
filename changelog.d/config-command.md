kind: added

- **`belay config`** prints every config key, its value, and where it came from (the file and line, or the default). **`belay config migrate [--yes]`** rewrites 0.1 keys and `look` values in place, keeping comments and order: it shows a diff, asks, and keeps a backup (`config.toml.bak-<unix time>`). The 0.1-key warning is now one line that names `belay config migrate`, at most once a day per config file (again when the file changes), and never from `belay hook` or `belay status`. When a file sets both an old and a new name (`fleet` and `awareness`), the new one now wins.
