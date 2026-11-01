# Packaging

## cargo-binstall

`crates/belay-cli/Cargo.toml` has `[package.metadata.binstall]`. It points at the
tarballs `.github/workflows/release.yml` attaches to each release, so
`cargo binstall --git https://github.com/prakhar479/belay belay-cli` downloads a
binary instead of building one. Keep the two in step if the asset names change.

## Homebrew

`homebrew/belay.rb` is a formula template for a tap. At each release:

1. Wait for the release workflow to attach the four tarballs and their `.sha256` files.
2. Copy `belay.rb` to the tap as `Formula/belay.rb`.
3. Replace `VERSION` with the version without the `v` (for example `0.2.0`).
4. Replace each `SHA256_<TARGET>` with the hash from that target's `.sha256` file:

   ```sh
   v=0.2.0
   for t in aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
     gh release download "v$v" --repo prakhar479/belay --pattern "belay-v$v-$t.tar.gz.sha256" --output - | cut -d' ' -f1
   done
   ```

5. Check it in the tap: `brew install --build-from-source ./Formula/belay.rb && brew test belay`.
