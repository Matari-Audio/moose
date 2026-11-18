fn main() {
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let crt_static = std::env::var("CARGO_CFG_TARGET_FEATURE")
        .unwrap_or_default()
        .split(',')
        .any(|f| f == "crt-static");
    if target_env == "msvc" && !crt_static {
        println!(
            "cargo:warning=moose-vst3: building for Windows MSVC without `+crt-static`; the \
             VST3 DLL will import VCRUNTIME140, which some hosts cannot find. \
             `cargo moose` adds it; for plain cargo builds add \
             `rustflags = [\"-C\", \"target-feature=+crt-static\"]` under \
             `[target.<triple>-windows-msvc]` in .cargo/config.toml."
        );
    }
}
