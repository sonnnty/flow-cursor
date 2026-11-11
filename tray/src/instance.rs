//! One tray and one status window per user session: lock files in the
//! private runtime directory (`$XDG_RUNTIME_DIR/flowcursor`, Windows
//! `%LOCALAPPDATA%\flowcursor\run`), held for the life of the process. The
//! tray's lock file holds its pid (`flow-tray --quit`).

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::PathBuf;

pub const TRAY_LOCK: &str = "tray.lock";
pub const WINDOW_LOCK: &str = "window.lock";
/// Created by a second "open the status window" request; the open window
/// comes to the front and deletes it.
const FOCUS_REQUEST: &str = "window.focus";

pub fn runtime_dir() -> io::Result<PathBuf> {
    let dir = flow_ipc::endpoint::runtime_dir();
    #[cfg(unix)]
    flow_ipc::endpoint::ensure_private_dir(&dir)?;
    #[cfg(not(unix))]
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Held while this process is the instance.
pub struct Lock {
    _file: File,
}

/// Takes the lock `name`; `Ok(None)` when another process holds it.
pub fn try_lock(name: &str) -> io::Result<Option<Lock>> {
    let path = runtime_dir()?.join(name);
    let mut f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
    match f.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    }
    f.set_len(0)?;
    f.rewind()?;
    write!(f, "{}", std::process::id())?;
    f.flush()?;
    Ok(Some(Lock { _file: f }))
}

/// True when some process holds the lock `name` (without keeping it).
pub fn is_locked(name: &str) -> bool {
    matches!(try_lock(name), Ok(None))
}

/// Pid written by the holder of `name`.
pub fn holder_pid(name: &str) -> Option<u32> {
    let mut s = String::new();
    // Windows does not let others read a locked range; the pid is only
    // used on Unix (`--quit`).
    File::open(runtime_dir().ok()?.join(name)).ok()?.read_to_string(&mut s).ok()?;
    s.trim().parse().ok()
}

/// Asks the open status window to come to the front.
pub fn request_focus() {
    if let Ok(d) = runtime_dir() {
        let _ = std::fs::write(d.join(FOCUS_REQUEST), b"");
    }
}

/// True (once) when another process asked the window to come to the front.
pub fn take_focus_request() -> bool {
    runtime_dir().map(|d| std::fs::remove_file(d.join(FOCUS_REQUEST)).is_ok()).unwrap_or(false)
}

/// Opens the status window: brings the open one to the front, or starts
/// `flow-tray --window`.
pub fn open_window() {
    if is_locked(WINDOW_LOCK) {
        request_focus();
        return;
    }
    // A stale request would raise the new window twice.
    let _ = take_focus_request();
    match crate::actions::spawn_self(&["--window"]) {
        // Reap it when it closes (no zombie per opened window).
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => eprintln!("flow-tray: starting the status window: {e}"),
    }
}
