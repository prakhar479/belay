kind: fixed
- Skills from `.tar.gz` archives are read on macOS (bsdtar's listing differs from GNU tar's); links, devices, and traversal are still refused before extracting.
- Outside git, a project's root and its project key are spelled the same when the path goes through a symlink (macOS `/var`).
