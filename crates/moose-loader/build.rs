fn main() {
    let output = std::process::Command::new("rustc")
        .arg("--version")
        .arg("--verbose")
        .output()
        .expect("rustc not found");
    let version = String::from_utf8_lossy(&output.stdout);
    let hash = fnv1a_64(version.as_bytes());
    println!("cargo:rustc-env=MOOSE_RUSTC_HASH={hash}");
    // Cargo already rebuilds every crate when the rustc version changes, so
    // only this script is an input. Naming a file that may not exist (the
    // monorepo has no frameworks/moose/rust-toolchain.toml) reran this script
    // on every build and recompiled moose and all its dependents.
    println!("cargo:rerun-if-changed=build.rs");
}

fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}
