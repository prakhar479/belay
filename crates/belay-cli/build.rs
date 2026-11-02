//! Bake the Claude Code versions with a conformance record (`docs/conformance/<v>.md`)
//! into the binary, so `belay claude` and `belay doctor` can say when a version has not
//! been checked.

fn main() {
    // At run time, not `env!`: a build script compiled before the repo moved would
    // otherwise keep looking in the old place and bake in an empty list (seen after the
    // tend → belay rename).
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let dir = std::path::Path::new(&manifest).join("../../docs/conformance");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut versions: Vec<String> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".md"))
                .map(str::to_string)
        })
        .collect();
    versions.sort();
    if versions.is_empty() {
        // A packaged crate has no docs/: every version then reads as unchecked.
        println!(
            "cargo:warning=no conformance records in {}: belay will call every Claude Code version unchecked",
            dir.display()
        );
    }
    println!("cargo:rustc-env=BELAY_CONFORMANT={}", versions.join(","));
}
