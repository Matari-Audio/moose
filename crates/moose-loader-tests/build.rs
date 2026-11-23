use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let readme_path = manifest_dir.join("../../README.md");
    println!("cargo:rerun-if-changed={}", readme_path.display());

    let readme = fs::read_to_string(&readme_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", readme_path.display()));

    let mut in_rust_block = false;
    let mut blocks: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in readme.lines() {
        if !in_rust_block && line.trim() == "```rust" {
            in_rust_block = true;
            current.clear();
        } else if in_rust_block && line.trim() == "```" {
            in_rust_block = false;
            blocks.push(std::mem::take(&mut current));
        } else if in_rust_block {
            current.push_str(line);
            current.push('\n');
        }
    }

    assert!(
        !blocks.is_empty(),
        "expected at least one ```rust code block in {}",
        readme_path.display()
    );

    let code = &blocks[0];
    assert!(
        code.contains("moose::plugin! { plugin: Gain, params: GainParams }"),
        "README.md minimal example must end with `moose::plugin! {{ plugin: Gain, params: GainParams }}`"
    );
    // `moose::plugin!` reads `CARGO_PKG_NAME` from `moose.toml`; strip only
    // that final export line so `moose-loader-tests` compiles and runs the
    // full `Gain` + `GainParams` + `Plugin` + `editor` body verbatim.
    let body = code.replace("moose::plugin! { plugin: Gain, params: GainParams }", "");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::write(out_dir.join("readme_example.rs"), body)
        .expect("failed to write extracted readme_example.rs");

    // On Linux, compile the real AU v2 C shim (`au_v2_shim.c` +
    // `au_shim_common.c`) against `fake_audiotoolbox.h` so `au_shim_harness`
    // exercises the shipping C shim without a Mac.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        let aax_template = manifest_dir.join("../cargo-moose/templates/aax");
        let aax_test = manifest_dir.join("test-support/aax_latency.cpp");
        let shim_include = manifest_dir.join("../moose-shim-types/include");
        println!("cargo:rerun-if-changed={}", aax_test.display());
        println!(
            "cargo:rerun-if-changed={}",
            aax_template.join("MooseAAX_Bridge.h").display()
        );
        println!(
            "cargo:rerun-if-changed={}",
            shim_include.join("aax_shim_types.h").display()
        );
        cc::Build::new()
            .cpp(true)
            .std("c++17")
            .file(aax_test)
            .include(aax_template)
            .include(&shim_include)
            .compile("aax_latency_harness");

        let au_shim = manifest_dir.join("../moose-au/shim");
        for f in [
            "au_shim_common.c",
            "au_v2_shim.c",
            "fake_audiotoolbox.h",
            "fake_audiotoolbox.c",
        ] {
            println!("cargo:rerun-if-changed={}", au_shim.join(f).display());
        }
        println!(
            "cargo:rerun-if-changed={}",
            shim_include.join("au_shim_types.h").display()
        );
        cc::Build::new()
            .file(au_shim.join("au_shim_common.c"))
            .file(au_shim.join("au_v2_shim.c"))
            .file(au_shim.join("fake_audiotoolbox.c"))
            .include(&au_shim)
            .include(&shim_include)
            .define("MOOSE_AU_FAKE_AUDIOTOOLBOX", None)
            .flag("-std=gnu11")
            .flag("-Wall")
            .flag("-Wextra")
            .flag("-Wno-unused-parameter")
            .compile("au_shim_linux_harness");
    }
}
