#!/bin/sh
# belay installer: https://github.com/prakhar479/belay
#
#   curl -fsSL https://prakhar479.github.io/belay/install.sh | sh
#
# What it does, in order:
#   1. Downloads the belay binary for this machine from the latest GitHub release and
#      checks its SHA-256. With no release for this machine, it builds belay with cargo.
#   2. Puts it in ~/.local/bin (no sudo).
#   3. If Claude Code is installed, adds belay's Claude Code plugin (`belay install`).
#      Undo with `belay uninstall`.
#
# Settings (environment variables):
#   BELAY_VERSION=v0.2.2      a release tag (default: the latest release)
#   BELAY_INSTALL_DIR=<dir>   where the binary goes (default: ~/.local/bin)
#   BELAY_NO_SETUP=1          install the binary only; leave Claude Code alone
#   BELAY_FROM_SOURCE=1       build with cargo even when a release binary exists
#
# Read it before you run it: curl -fsSL https://prakhar479.github.io/belay/install.sh | less

set -eu

REPO="prakhar479/belay"
# For tests: a directory or URL that holds release assets as <tag>/<asset>.
BASE="${BELAY_DOWNLOAD_BASE:-https://github.com/$REPO/releases/download}"
DIR="${BELAY_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf 'belay: %s\n' "$*"; }
fail() { printf 'belay: %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# The tag goes into a URL and a cargo argument: letters, digits, dots and dashes only.
if [ -n "${BELAY_VERSION:-}" ]; then
    case "$BELAY_VERSION" in
        v[0-9]* ) ;;
        *) fail "BELAY_VERSION must look like v0.2.2 (got: $BELAY_VERSION)" ;;
    esac
    case "$BELAY_VERSION" in
        *[!A-Za-z0-9.-]*) fail "BELAY_VERSION may hold only letters, digits, dots and dashes" ;;
    esac
fi

target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) return 1 ;;
    esac
    case "$os" in
        Linux) echo "$arch-unknown-linux-gnu" ;;
        Darwin) echo "$arch-apple-darwin" ;;
        *) return 1 ;;
    esac
}

# The latest release's tag, from GitHub's redirect (no API token, no rate limit).
latest() {
    url=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" 2>/dev/null) || return 1
    tag=${url##*/}
    case "$tag" in v[0-9]*) echo "$tag" ;; *) return 1 ;; esac
}

sha256() {
    if have sha256sum; then sha256sum "$1" | cut -d' ' -f1
    elif have shasum; then shasum -a 256 "$1" | cut -d' ' -f1
    else fail "need sha256sum or shasum to check the download"
    fi
}

from_release() {
    t=$(target) || return 1
    tag="${BELAY_VERSION:-}"
    [ -n "$tag" ] || tag=$(latest) || return 1
    name="belay-$tag-$t"
    tmp=$(mktemp -d)
    trap 'rm -rf "${tmp:?}"' EXIT
    say "downloading belay $tag for $t"
    curl -fsSL "$BASE/$tag/$name.tar.gz" -o "$tmp/$name.tar.gz" || { rm -rf "${tmp:?}"; return 1; }
    curl -fsSL "$BASE/$tag/$name.tar.gz.sha256" -o "$tmp/$name.tar.gz.sha256" || fail "the release has no checksum for $name"
    want=$(cut -d' ' -f1 "$tmp/$name.tar.gz.sha256")
    got=$(sha256 "$tmp/$name.tar.gz")
    [ "$want" = "$got" ] || fail "checksum mismatch for $name.tar.gz: refusing to install"
    tar xzf "$tmp/$name.tar.gz" -C "$tmp"
    mkdir -p "$DIR"
    # Replace, not overwrite in place: a running belay keeps its old file.
    cp "$tmp/$name/belay" "$DIR/belay.new"
    chmod 755 "$DIR/belay.new"
    mv -f "$DIR/belay.new" "$DIR/belay"
    rm -rf "${tmp:?}"
    trap - EXIT
}

from_source() {
    have cargo || fail "no release binary for this machine, and no cargo to build one. Install Rust (https://rustup.rs), then run this again."
    say "building belay from source with cargo (a few minutes)"
    ref=""
    [ -n "${BELAY_VERSION:-}" ] && ref="--tag $BELAY_VERSION"
    root=$(mktemp -d)
    # shellcheck disable=SC2086
    cargo install --locked --git "https://github.com/$REPO" $ref --root "$root" belay-cli
    mkdir -p "$DIR"
    cp "$root/bin/belay" "$DIR/belay.new"
    chmod 755 "$DIR/belay.new"
    mv -f "$DIR/belay.new" "$DIR/belay"
    rm -rf "${root:?}"
}

have curl || fail "need curl"

if [ "${BELAY_FROM_SOURCE:-}" = 1 ] || ! from_release; then
    from_source
fi

BELAY="$DIR/belay"
[ -x "$BELAY" ] || fail "installation did not produce $BELAY"
say "installed $("$BELAY" --version) to $BELAY"

case ":$PATH:" in
    *":$DIR:"*) ;;
    *) say "add $DIR to your PATH, for example: echo 'export PATH=\"$DIR:\$PATH\"' >> ~/.bashrc" ;;
esac

if [ "${BELAY_NO_SETUP:-}" = 1 ]; then
    say "skipped Claude Code setup (BELAY_NO_SETUP=1). Later: belay install"
elif have claude; then
    say "adding the belay plugin to Claude Code"
    if "$BELAY" install --yes; then
        say "done. Use claude as usual: belay works alongside it. Undo: belay uninstall"
    else
        say "the plugin setup did not finish. Run it yourself: belay install"
    fi
else
    say "Claude Code not found. When you have it: belay install"
fi

say "check your setup any time: belay doctor"
