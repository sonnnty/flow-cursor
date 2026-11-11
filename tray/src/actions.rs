//! What the menu items and buttons do. Each IPC command uses its own short
//! connection (see [`crate::watch`]).

use flow_ipc::{Command, Reply};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

pub fn endpoint() -> PathBuf {
    flow_ipc::endpoint::default_endpoint()
}

/// Sends one command to the agent; an error reply becomes an `Err`.
pub fn request(cmd: Command) -> io::Result<String> {
    match flow_ipc::client::request(&endpoint(), &cmd)? {
        Reply::Ok { message } => Ok(message),
        Reply::Error { message } => Err(io::Error::other(message)),
        other => Ok(format!("{other:?}")),
    }
}

pub fn set_paused(paused: bool) -> io::Result<String> {
    request(if paused { Command::Pause } else { Command::Resume })
}

pub fn update_now() -> io::Result<String> {
    request(Command::UpdateNow)
}

pub fn join(token: &str, hub: &str) -> io::Result<String> {
    let hub = hub.trim();
    request(Command::Join { token: token.trim().to_string(), hub: (!hub.is_empty()).then(|| hub.to_string()) })
}

/// Restarts the agent; starts it when nothing answers.
pub fn restart_agent() -> io::Result<String> {
    match request(Command::Restart) {
        Ok(m) => Ok(m),
        Err(e) if is_unreachable(&e) => start_agent().map(|_| "agent started".into()),
        Err(e) => Err(e),
    }
}

/// Nothing listens on the endpoint (as opposed to an error reply).
pub fn is_unreachable(e: &io::Error) -> bool {
    matches!(e.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset)
        || e.raw_os_error() == Some(2) // ERROR_FILE_NOT_FOUND (no such pipe)
}

/// `flow-agent` next to this program, else the one on `PATH`.
pub fn agent_exe() -> PathBuf {
    let name = if cfg!(windows) { "flow-agent.exe" } else { "flow-agent" };
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Starts the agent. Linux: the systemd user service (journal, restart on
/// failure); Windows: `flow-agent run` detached from any console.
pub fn start_agent() -> io::Result<()> {
    #[cfg(windows)]
    {
        flow_ipc::detach::spawn_detached(Process::new(agent_exe()).arg("run")).map(|_| ())
    }
    #[cfg(not(windows))]
    {
        let st = Process::new("systemctl").args(["--user", "restart", "flow-agent.service"]).status()?;
        if st.success() {
            Ok(())
        } else {
            Err(io::Error::other(
                "systemctl --user restart flow-agent failed (is the flow-agent user service installed?)",
            ))
        }
    }
}

/// Opens a URL or file with the desktop's default application.
pub fn open(target: &str) -> io::Result<()> {
    #[cfg(windows)]
    {
        crate::win::shell_open(target)
    }
    #[cfg(not(windows))]
    {
        flow_ipc::detach::spawn_detached(Process::new("xdg-open").arg(target)).map(|_| ())
    }
}

/// Opens the agent's log: its log file when it writes one, otherwise
/// (systemd journal) the recent lines it keeps in memory, written to a
/// private file.
pub fn open_log(log_file: Option<&str>) -> io::Result<()> {
    if let Some(f) = log_file.filter(|f| Path::new(f).exists()) {
        return open(f);
    }
    let lines = match flow_ipc::client::request(&endpoint(), &Command::Logs { max: 1000 })? {
        Reply::Logs { lines } => lines,
        Reply::Error { message } => return Err(io::Error::other(message)),
        _ => Vec::new(),
    };
    let dir = flow_ipc::endpoint::runtime_dir();
    #[cfg(unix)]
    flow_ipc::endpoint::ensure_private_dir(&dir)?;
    #[cfg(windows)]
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("agent-log.txt");
    let mut body = String::from(
        "# Recent FlowCursor agent log (from the agent's memory).\n# Full log: journalctl --user -u flow-agent\n\n",
    );
    for l in lines {
        body.push_str(&l);
        body.push('\n');
    }
    std::fs::write(&path, body)?;
    open(&path.to_string_lossy())
}

/// Runs the (updated) tray binary in place of this process, same
/// arguments; `build` is what it should be now (see [`crate::ENV_REEXEC`]).
/// Call after the tray released its single-instance lock.
pub fn restart_tray(build: &str) -> io::Error {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return e,
    };
    // Linux: the replaced file shows as "<path> (deleted)".
    let exe = match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(p) if !exe.exists() => PathBuf::from(p),
        _ => exe,
    };
    let mut cmd = Process::new(&exe);
    cmd.args(std::env::args_os().skip(1)).env(crate::ENV_REEXEC, build);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.exec()
    }
    #[cfg(not(unix))]
    {
        match flow_ipc::detach::spawn_detached(&mut cmd) {
            Ok(_) => std::process::exit(0),
            Err(e) => e,
        }
    }
}

/// Starts this program again with `args` (the status window).
pub fn spawn_self(args: &[&str]) -> io::Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    Process::new(exe).args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).spawn()
}
