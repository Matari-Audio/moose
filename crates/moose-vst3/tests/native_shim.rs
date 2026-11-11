//! Driver-free regressions against the actual C++ COM shim.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;

fn run_fixture(name: &str, arguments: &[&str]) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/native")
        .join(format!("{name}.cpp"));
    let binary = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::create_dir_all(binary.parent().expect("fixture output parent")).unwrap();
    let compiled = Command::new("c++")
        .args(["-std=c++17", "-O0"])
        .arg(source)
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("C++ compiler required by moose-vst3");
    assert!(
        compiled.status.success(),
        "{name}: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let run = Command::new(binary)
        .args(arguments)
        .output()
        .expect("run shim fixture");
    assert!(
        run.status.success(),
        "{name} {arguments:?}: {} {}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
}

#[test]
fn lifecycle_resets() {
    run_fixture("lifecycle", &[]);
    run_fixture("lifecycle", &["processing"]);
}

#[test]
fn editor_attachment() {
    run_fixture("editor_attach", &[]);
}

#[test]
fn editor_ownership() {
    run_fixture("editor_lifetime", &[]);
}

#[test]
fn editor_calls_off_the_ui_thread() {
    run_fixture("editor_threads", &[]);
}

#[test]
fn parameter_units() {
    run_fixture("parameter_units", &[]);
}
