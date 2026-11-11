//! Where the agent listens: one endpoint per user (and per session on
//! Unix, since `$XDG_RUNTIME_DIR` is per login).
//!
//! * Unix: `$XDG_RUNTIME_DIR/flowcursor/agent.sock`, or
//!   `/tmp/flowcursor-<uid>/agent.sock` without a runtime directory. The
//!   directory is created 0700 and must be owned by the user; the socket
//!   is 0600.
//! * Windows: `\\.\pipe\flowcursor-agent-<user SID>`.
//!
//! `FLOW_AGENT_IPC` overrides the path (tests, several agents per user).

use std::path::PathBuf;
#[cfg(unix)]
use std::{io, path::Path};

/// Environment variable that overrides [`default_endpoint`].
pub const ENV_OVERRIDE: &str = "FLOW_AGENT_IPC";

/// The agent's IPC endpoint for the current user.
pub fn default_endpoint() -> PathBuf {
    if let Some(p) = std::env::var_os(ENV_OVERRIDE).filter(|p| !p.is_empty()) {
        return p.into();
    }
    platform_endpoint()
}

#[cfg(unix)]
fn platform_endpoint() -> PathBuf {
    runtime_dir().join("agent.sock")
}

#[cfg(windows)]
fn platform_endpoint() -> PathBuf {
    let who = crate::windows::current_user_sid()
        .unwrap_or_else(|_| std::env::var("USERNAME").unwrap_or_else(|_| "user".into()));
    PathBuf::from(format!(r"\\.\pipe\flowcursor-agent-{who}"))
}

/// Per-user directory for the socket and other runtime files (tray lock,
/// log dumps). Not created here; see [`ensure_private_dir`].
#[cfg(unix)]
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d).join("flowcursor"),
        None => std::env::temp_dir().join(format!("flowcursor-{}", rustix::process::getuid().as_raw())),
    }
}

/// Real uid of this process.
#[cfg(unix)]
pub fn current_uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// Per-user directory for runtime files of the tray (Windows: the local
/// app data directory, which is private to the user).
#[cfg(windows)]
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("flowcursor")
        .join("run")
}

/// Creates `dir` (mode 0700) if needed and makes sure only the current
/// user can use it: it must be a real directory (not a symlink) owned by
/// the user; group/other permission bits are removed.
#[cfg(unix)]
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let md = std::fs::symlink_metadata(dir)?;
    if !md.is_dir() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("{} is not a directory", dir.display())));
    }
    let me = rustix::process::getuid().as_raw();
    if md.uid() != me {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} belongs to uid {}, not to this user (uid {me})", dir.display(), md.uid()),
        ));
    }
    if md.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("flow-ipc-{tag}-{}-{:?}", std::process::id(), std::thread::current().id()))
    }

    #[test]
    fn private_dir_is_created_and_tightened() {
        let d = tmp("dir").join("flowcursor");
        ensure_private_dir(&d).unwrap();
        assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700);
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&d).unwrap();
        assert_eq!(std::fs::metadata(&d).unwrap().permissions().mode() & 0o777, 0o700, "loosened dir is fixed");
        std::fs::remove_dir_all(d.parent().unwrap()).ok();
    }

    #[test]
    fn symlinks_and_files_are_refused() {
        let base = tmp("link");
        std::fs::create_dir_all(&base).unwrap();
        let target = base.join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        let link = base.join("flowcursor");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(ensure_private_dir(&link).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        let file = base.join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(ensure_private_dir(&file).is_err());
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn endpoint_lives_in_the_runtime_dir() {
        // Not touching the process environment (tests run in parallel).
        if std::env::var_os(ENV_OVERRIDE).is_none() {
            assert_eq!(default_endpoint(), runtime_dir().join("agent.sock"));
        }
        assert!(runtime_dir().ends_with("flowcursor") || runtime_dir().to_string_lossy().contains("flowcursor-"));
    }
}
