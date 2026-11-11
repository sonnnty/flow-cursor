// Release builds on Windows use the GUI subsystem: no console window when
// started at login or from the Start menu.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

//! flow-tray: FlowCursor tray icon and status window.
//!
//!   flow-tray                 tray icon (one per session; started at login)
//!   flow-tray --show          tray icon (if not running) and the status window
//!   flow-tray --window        only the status window
//!   flow-tray --quit          ask the running tray to exit
//!   --no-start-agent          (Windows) never start flow-agent from the tray
//!   flow-tray --version       print the build identity

use flow_tray::instance::{self, TRAY_LOCK, WINDOW_LOCK};

const USAGE: &str = "usage: flow-tray [--show | --window | --quit | --version] [--no-start-agent]";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |f: &str| args.iter().any(|a| a == f);
    if let Some(a) =
        args.iter().find(|a| !matches!(a.as_str(), "--show" | "--window" | "--quit" | "--version" | "--no-start-agent"))
    {
        eprintln!("unknown argument {a}\n{USAGE}");
        std::process::exit(2);
    }
    if has("--version") {
        println!("flow-tray {}", flow_version::FULL);
        return;
    }
    if has("--quit") {
        quit_running_tray();
        return;
    }
    if has("--window") {
        return window();
    }
    let show = has("--show");
    let lock = match instance::try_lock(TRAY_LOCK) {
        Ok(Some(l)) => Some(l),
        Ok(None) => {
            // Already running (the Start menu entry / a second login item).
            if show {
                window();
            } else {
                eprintln!("flow-tray is already running");
            }
            return;
        }
        Err(e) => {
            eprintln!("flow-tray: single-instance lock: {e}");
            None
        }
    };
    let r = run_tray(show, !has("--no-start-agent"));
    drop(lock);
    if let Err(e) = r {
        eprintln!("flow-tray: {e:#}");
        std::process::exit(1);
    }
}

/// The status window in this process, or the open one to the front.
fn window() {
    match instance::try_lock(WINDOW_LOCK) {
        Ok(None) => instance::request_focus(),
        Ok(Some(_lock)) => {
            if let Err(e) = flow_tray::window::run() {
                eprintln!("flow-tray: status window: {e:#}");
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("flow-tray: single-instance lock: {e}");
            if let Err(e) = flow_tray::window::run() {
                eprintln!("flow-tray: status window: {e:#}");
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn run_tray(show: bool, _start_agent: bool) -> anyhow::Result<()> {
    flow_tray::tray_linux::run(show)
}

#[cfg(windows)]
fn run_tray(show: bool, start_agent: bool) -> anyhow::Result<()> {
    flow_tray::tray_windows::run(show, start_agent)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn run_tray(_show: bool, _start_agent: bool) -> anyhow::Result<()> {
    anyhow::bail!("no tray icon on this platform; use flow-tray --window")
}

#[cfg(unix)]
fn quit_running_tray() {
    match instance::holder_pid(TRAY_LOCK).filter(|_| instance::is_locked(TRAY_LOCK)) {
        Some(pid) => {
            if let Some(p) = rustix::process::Pid::from_raw(pid as i32) {
                let _ = rustix::process::kill_process(p, rustix::process::Signal::TERM);
            }
        }
        None => eprintln!("flow-tray is not running"),
    }
}

#[cfg(windows)]
fn quit_running_tray() {
    if !flow_tray::tray_windows::quit_running() {
        eprintln!("flow-tray is not running");
    }
}
