---
name: Release Notes
description: Write release notes from the git log since the last tag.
---

# Release notes

1. Run `git log --oneline $(git describe --tags --abbrev=0)..HEAD`.
2. Group the commits by kind: features, fixes, other.
3. Write one plain sentence per change.
