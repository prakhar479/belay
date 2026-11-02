# Changelog fragments

One file per change a user can see, so parallel branches never conflict in `CHANGELOG.md`.
Name it `<short-name>.md` (a-z, 0-9, `-`). The first line is the kind; the rest is bullets:

```markdown
kind: added

- **Feature:** what a user sees, in short plain sentences.
```

Kinds: `added`, `changed`, `fixed`, `removed`. `scripts/changelog.py --check` validates the
fragments (CI runs it). At release, `scripts/changelog.py --release <version>` joins them into
`CHANGELOG.md`, by kind and then file name, and deletes them.
