//! Invalidate the crate's incremental cache whenever any embedded
//! template file changes.
//!
//! The scaffolder pulls the files under `templates/` into the binary
//! via `include_str!`. Cargo's default rebuild tracking only watches
//! Rust sources, so a pure template edit doesn't re-trigger
//! `include_str!` and `cargo moose new` would ship stale bytes.
//!
//! Watching the template directories (recursive) fixes that. We also
//! watch this build script itself so editing the watch list triggers
//! a rebuild.
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    watch_dir(Path::new("templates"));
}

fn watch_dir(dir: &Path) {
    // `rerun-if-changed` on a directory tells cargo to watch the
    // directory itself (entries added/removed). We additionally
    // recurse so every file under the tree is watched for content
    // changes.
    println!("cargo:rerun-if-changed={}", dir.display());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            watch_dir(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
