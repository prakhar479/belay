kind: added

- **Packaging:** `cargo binstall` finds the release tarballs (`[package.metadata.binstall]`), and `packaging/homebrew/belay.rb` is a formula template for a tap. A release fails when the tag, the workspace version, and the plugin version disagree. CI runs the sandbox tests with bubblewrap.
