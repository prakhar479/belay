#!/usr/bin/env python3
"""CHANGELOG fragments: one file per change in changelog.d/, joined at release.

Parallel tasks all appended under "Unreleased" in CHANGELOG.md and conflicted there.
Now each change adds its own `changelog.d/<short-name>.md`:

    kind: added

    - **Feature:** what a user sees, in short plain sentences.

The first line is `kind: added|changed|fixed|removed`. The rest is Markdown bullets.

    scripts/changelog.py --check              validate every fragment (CI runs this)
    scripts/changelog.py --release 0.2.0      join them into CHANGELOG.md and delete them
    scripts/changelog.py --preview            print the joined section, change nothing

The join is deterministic: by kind (in the order above), then by file name.
"""

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
KINDS = ["added", "changed", "fixed", "removed"]
NAME = re.compile(r"^[a-z0-9][a-z0-9-]*\.md$")
UNRELEASED = "## Unreleased"
PLACEHOLDER = "See `changelog.d/` (joined at release by `scripts/changelog.py --release`)."
SKIP = {"README.md"}


class FragmentError(Exception):
    pass


def parse(path: Path):
    """Return (kind, body) of one fragment, or raise FragmentError."""
    if not NAME.match(path.name):
        raise FragmentError(f"{path.name}: name it like `short-name.md` (a-z, 0-9, -).")
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines()
    if not lines:
        raise FragmentError(f"{path.name}: the file is empty.")
    m = re.fullmatch(r"kind:\s*(\S+)\s*", lines[0])
    if not m:
        raise FragmentError(f"{path.name}: the first line must be `kind: {'|'.join(KINDS)}`.")
    kind = m.group(1)
    if kind not in KINDS:
        raise FragmentError(f"{path.name}: unknown kind `{kind}`; use one of {', '.join(KINDS)}.")
    body = "\n".join(lines[1:]).strip("\n")
    if not body.strip():
        raise FragmentError(f"{path.name}: no bullet after the kind line.")
    first = body.lstrip().splitlines()[0]
    if not first.startswith("- "):
        raise FragmentError(f"{path.name}: the text must be Markdown bullets starting with `- `.")
    if re.search(r"^#", body, re.M):
        raise FragmentError(f"{path.name}: no headings; the release adds them by kind.")
    return kind, body.strip()


def fragments(dir_: Path):
    """All fragments as (kind, name, body), sorted. Errors are collected, not raised."""
    out, errors = [], []
    if not dir_.is_dir():
        return out, errors
    for path in sorted(dir_.iterdir()):
        if path.name in SKIP or path.name.startswith("."):
            continue
        if not path.is_file():
            errors.append(f"{path.name}: not a file.")
            continue
        try:
            kind, body = parse(path)
        except FragmentError as e:
            errors.append(str(e))
            continue
        out.append((kind, path.name, body))
    out.sort(key=lambda f: (KINDS.index(f[0]), f[1]))
    return out, errors


def join(frags, version: str) -> str:
    """The released section for `version`, headed by kind."""
    parts = [f"## {version}"]
    for kind in KINDS:
        bodies = [body for k, _, body in frags if k == kind]
        if bodies:
            parts.append(f"### {kind.capitalize()}")
            parts.append("\n".join(bodies))
    return "\n\n".join(parts) + "\n"


def release(changelog: Path, frags, version: str) -> str:
    """CHANGELOG.md's new text: Unreleased keeps the placeholder, the version goes under it."""
    text = changelog.read_text(encoding="utf-8")
    lines = text.split("\n")
    try:
        start = lines.index(UNRELEASED)
    except ValueError:
        raise FragmentError(f"{changelog.name}: no `{UNRELEASED}` heading.")
    end = next((i for i in range(start + 1, len(lines)) if lines[i].startswith("## ")), len(lines))
    if any(lines[i] == f"## {version}" for i in range(len(lines))):
        raise FragmentError(f"{changelog.name}: {version} is already released.")
    leftover = [l for l in lines[start + 1:end] if l.strip() and l.strip() != PLACEHOLDER]
    if leftover:
        raise FragmentError(
            f"{changelog.name}: text under Unreleased besides the placeholder; move it to changelog.d/."
        )
    head = "\n".join(lines[:start])
    tail = "\n".join(lines[end:])
    new = f"{head}\n{UNRELEASED}\n\n{PLACEHOLDER}\n\n{join(frags, version)}"
    if tail.strip():
        new += "\n" + tail
    return new.rstrip("\n") + "\n"


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    g = ap.add_mutually_exclusive_group(required=True)
    g.add_argument("--check", action="store_true", help="validate the fragments")
    g.add_argument("--release", metavar="VERSION", help="join the fragments under VERSION and delete them")
    g.add_argument("--preview", action="store_true", help="print the joined section")
    ap.add_argument("--root", type=Path, default=ROOT, help=argparse.SUPPRESS)
    args = ap.parse_args(argv)

    dir_ = args.root / "changelog.d"
    changelog = args.root / "CHANGELOG.md"
    frags, errors = fragments(dir_)
    for e in errors:
        print(f"changelog.d/{e}", file=sys.stderr)
    if errors:
        return 1

    if args.check:
        # The placeholder check keeps CHANGELOG.md's Unreleased from growing again.
        try:
            release(changelog, frags, "0.0.0-check")
        except FragmentError as e:
            print(e, file=sys.stderr)
            return 1
        print(f"{len(frags)} fragment(s) ok.")
        return 0

    if args.preview:
        sys.stdout.write(join(frags, "Unreleased"))
        return 0

    version = args.release.removeprefix("v")
    if not frags:
        print("No fragments in changelog.d/: nothing to release.", file=sys.stderr)
        return 1
    try:
        new = release(changelog, frags, version)
    except FragmentError as e:
        print(e, file=sys.stderr)
        return 1
    changelog.write_text(new, encoding="utf-8")
    for _, name, _ in frags:
        (dir_ / name).unlink()
    print(f"Joined {len(frags)} fragment(s) into CHANGELOG.md under {version}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
