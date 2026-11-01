# Homebrew formula for a tap (for example prakhar479/homebrew-tap, as Formula/belay.rb).
# A template: packaging/README.md says how to fill VERSION and the sha256 values at
# release time. The URLs are the tarballs .github/workflows/release.yml attaches.
class Belay < Formula
  desc "Local supervisor for command-line coding agents"
  homepage "https://prakhar479.github.io/belay/"
  version "VERSION"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "https://github.com/prakhar479/belay/releases/download/v#{version}/belay-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "SHA256_AARCH64_APPLE_DARWIN"
    end
    on_intel do
      url "https://github.com/prakhar479/belay/releases/download/v#{version}/belay-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "SHA256_X86_64_APPLE_DARWIN"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/prakhar479/belay/releases/download/v#{version}/belay-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "SHA256_AARCH64_UNKNOWN_LINUX_GNU"
    end
    on_intel do
      url "https://github.com/prakhar479/belay/releases/download/v#{version}/belay-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "SHA256_X86_64_UNKNOWN_LINUX_GNU"
    end
  end

  def install
    # Homebrew enters the tarball's one top-level directory before install runs.
    bin.install "belay"
    # The tarball carries a copy of the Claude Code plugin; keep it for reference.
    # `belay install` adds the plugin from its marketplace, not from this copy.
    pkgshare.install "plugin"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/belay --version")
  end
end
