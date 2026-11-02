kind: added

- **One-command install:** `curl -fsSL https://prakhar479.github.io/belay/install.sh | sh` downloads the binary for your machine from the latest release, checks its SHA-256, puts it in `~/.local/bin` (no sudo), and adds the Claude Code plugin when Claude Code is installed. With no release for your machine it builds belay with cargo. `BELAY_VERSION`, `BELAY_INSTALL_DIR`, `BELAY_NO_SETUP`, and `BELAY_FROM_SOURCE` change what it does. CI tests it offline against a fake release (good install, bad checksum, missing release).
