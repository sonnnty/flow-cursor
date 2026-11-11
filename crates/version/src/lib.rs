//! Build identity of a FlowCursor binary.
//!
//! Every build has the workspace version plus a build id: `0.2.0+142.g1a2b3c4d5e`
//! for builds made by `build/build.sh` (commit count and commit), `0.2.0+dev`
//! for a plain `cargo build`. Agents report it to the hub, which updates
//! them to the build it carries (see `flow_proto::update`); two builds are
//! the same exactly when [`FULL`] is equal.

/// Workspace version (`Cargo.toml`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Build id (`dev` unless `FLOW_BUILD_ID` was set at compile time).
pub const BUILD: &str = env!("FLOW_BUILD_ID");
/// Full git commit the build was made from (empty when unknown).
pub const COMMIT: &str = env!("FLOW_BUILD_COMMIT");
/// `VERSION+BUILD`, what `--version` prints and what update decisions compare.
pub const FULL: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("FLOW_BUILD_ID"));
/// `<os>-<arch>` this binary was built for (`linux-x86_64`, `windows-x86_64`):
/// the key of the per-platform files in an agent-build manifest.
pub const PLATFORM: &str = env!("FLOW_PLATFORM");

/// Platform keys of the agent builds `build/build.sh` produces.
pub const PLATFORM_LINUX_X86_64: &str = "linux-x86_64";
pub const PLATFORM_WINDOWS_X86_64: &str = "windows-x86_64";

/// `VERSION+BUILD` from its two parts.
pub fn full(version: &str, build: &str) -> String {
    format!("{version}+{build}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_consistent() {
        assert_eq!(FULL, full(VERSION, BUILD));
        assert!(!BUILD.is_empty());
        assert!(PLATFORM.contains('-'));
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            assert_eq!(PLATFORM, PLATFORM_LINUX_X86_64);
        }
    }
}
