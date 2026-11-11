//! Starting programs that must outlive their parent and must not belong to
//! its console (Windows: a child of a terminal attaches to that console and
//! dies when the terminal is closed; a restarted agent must survive its old
//! process; the tray starts the agent).

use std::io;
use std::process::{Child, Command, Stdio};

/// Spawns `cmd` with no stdio, in its own process group (Unix) or detached
/// from any console and outside the parent's job when allowed (Windows).
pub fn spawn_detached(cmd: &mut Command) -> io::Result<Child> {
    cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    platform(cmd)
}

#[cfg(unix)]
fn platform(cmd: &mut Command) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0).spawn()
}

#[cfg(windows)]
fn platform(cmd: &mut Command) -> io::Result<Child> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const ERROR_ACCESS_DENIED: i32 = 5;
    // Leaving the parent's job keeps the child alive when a job-based
    // parent (some terminals, installers) closes; jobs that forbid
    // breaking away refuse it, then start inside the job.
    match cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB).spawn() {
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP).spawn()
        }
        r => r,
    }
}
