//! Bakes the build identity into the crate. `build/build.sh` sets
//! `FLOW_BUILD_ID` (`<commit count>.g<short commit>[.dirty…]`) and
//! `FLOW_BUILD_COMMIT`; plain `cargo build` gets `dev`.

fn main() {
    println!("cargo:rerun-if-env-changed=FLOW_BUILD_ID");
    println!("cargo:rerun-if-env-changed=FLOW_BUILD_COMMIT");
    let id = std::env::var("FLOW_BUILD_ID").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "dev".into());
    // Semver build metadata: the id ends up after the `+` of the version.
    let valid = id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    assert!(valid, "FLOW_BUILD_ID {id:?} must be at most 64 characters of [0-9A-Za-z.-]");
    let commit = std::env::var("FLOW_BUILD_COMMIT").unwrap_or_default();
    assert!(commit.len() <= 64 && commit.chars().all(|c| c.is_ascii_hexdigit()), "FLOW_BUILD_COMMIT must be hex");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    println!("cargo:rustc-env=FLOW_BUILD_ID={id}");
    println!("cargo:rustc-env=FLOW_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=FLOW_PLATFORM={os}-{arch}");
}
