#!/bin/sh
# Test site/install.sh offline: a fake release of the binary built here, served from a
# local directory. Usage: scripts/test_install.sh [path/to/belay]   (CI runs it)
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
bin="${1:-$root/target/debug/belay}"
[ -x "$bin" ] || { echo "build belay first (cargo build)"; exit 1; }
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) t=x86_64-unknown-linux-gnu ;;
    Linux-aarch64) t=aarch64-unknown-linux-gnu ;;
    Darwin-arm64) t=aarch64-apple-darwin ;;
    Darwin-x86_64) t=x86_64-apple-darwin ;;
    *) echo "skip: no release target for this machine"; exit 0 ;;
esac
w=$(mktemp -d)
trap 'rm -rf "${w:?}"' EXIT
tag=v9.9.9
name="belay-$tag-$t"
mkdir -p "$w/rel/$tag" "$w/pkg/$name"
cp "$bin" "$w/pkg/$name/belay"
tar czf "$w/rel/$tag/$name.tar.gz" -C "$w/pkg" "$name"
( cd "$w/rel/$tag" && { sha256sum "$name.tar.gz" 2>/dev/null || shasum -a 256 "$name.tar.gz"; } > "$name.tar.gz.sha256" )

# Each case installs into its own directory, so no case cleans up after another.
run() { # $1: install dir, $2: version
    env BELAY_DOWNLOAD_BASE="file://$w/rel" BELAY_VERSION="$2" BELAY_INSTALL_DIR="$1" \
        BELAY_NO_SETUP=1 sh "$root/site/install.sh"
}

# 1. A good release installs, and the binary runs.
run "$w/bin1" "$tag" > "$w/out1.txt" 2>&1 || { cat "$w/out1.txt"; echo "FAIL: install failed"; exit 1; }
"$w/bin1/belay" --version | grep -q belay || { echo "FAIL: installed binary does not run"; exit 1; }
grep -q "installed" "$w/out1.txt" || { cat "$w/out1.txt"; echo "FAIL: no install line"; exit 1; }

# 2. A bad checksum installs nothing.
echo "0000000000000000000000000000000000000000000000000000000000000000  $name.tar.gz" > "$w/rel/$tag/$name.tar.gz.sha256"
if run "$w/bin2" "$tag" > "$w/out2.txt" 2>&1; then cat "$w/out2.txt"; echo "FAIL: a bad checksum installed"; exit 1; fi
grep -q "checksum mismatch" "$w/out2.txt" || { cat "$w/out2.txt"; echo "FAIL: no checksum message"; exit 1; }
[ ! -e "$w/bin2/belay" ] || { echo "FAIL: a binary was left after a bad checksum"; exit 1; }

# 3. A release that does not exist, and no cargo on PATH: a clear failure.
if env PATH=/usr/bin:/bin sh -c "$(command -v env) BELAY_DOWNLOAD_BASE='file://$w/rel' BELAY_VERSION=v0.0.0 BELAY_INSTALL_DIR='$w/bin3' BELAY_NO_SETUP=1 sh '$root/site/install.sh'" > "$w/out3.txt" 2>&1; then
    cat "$w/out3.txt"; echo "FAIL: a missing release installed"; exit 1
fi
grep -q "no cargo" "$w/out3.txt" || { cat "$w/out3.txt"; echo "FAIL: no cargo message"; exit 1; }

# 4. A version with odd characters is rejected before any download.
for bad in 'v1/../x' 'v1;id' 'latest'; do
    if run "$w/bin4" "$bad" > "$w/out4.txt" 2>&1; then cat "$w/out4.txt"; echo "FAIL: bad version '$bad' installed"; exit 1; fi
    grep -q "BELAY_VERSION" "$w/out4.txt" || { cat "$w/out4.txt"; echo "FAIL: no version message for '$bad'"; exit 1; }
done

echo "install.sh: ok (release, bad checksum, missing release, bad version)"
