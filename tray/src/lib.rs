//! FlowCursor tray icon and status window (`flow-tray`): shows the state of
//! the local agent and controls it over the local IPC endpoint. The agent
//! itself has no UI and keeps working without this program.

pub mod actions;
pub mod icon;
pub mod instance;
pub mod model;
pub mod watch;
pub mod window;

#[cfg(target_os = "linux")]
pub mod tray_linux;
#[cfg(windows)]
pub mod tray_windows;
#[cfg(windows)]
pub mod win;

use model::AgentState;
use std::time::{Duration, Instant};

/// Menu clicks and state changes, handled on the tray's main thread.
#[derive(Clone, Debug)]
pub enum Action {
    State(Box<AgentState>),
    OpenWindow,
    OpenDashboard,
    SetPaused(bool),
    RestartAgent,
    OpenLog,
    Quit,
}

/// Behaviour shared by the Linux and Windows trays.
#[derive(Default)]
pub struct TrayCore {
    state: AgentState,
    /// The first-run window (no join token yet) was opened.
    first_run_shown: bool,
    /// Start the agent when nothing answers (Windows: the tray is what
    /// starts at login; Linux: systemd runs the agent).
    pub start_agent: bool,
    last_start: Option<Instant>,
    /// "Quit" also stops the agent (Windows, where the tray started it).
    pub quit_stops_agent: bool,
}

/// Do not start the agent more often than this (it may be crashing).
const START_INTERVAL: Duration = Duration::from_secs(30);

/// Runs `f` off the tray thread (IPC calls can take a moment) and logs
/// its failure.
fn background(what: &'static str, f: impl FnOnce() -> std::io::Result<String> + Send + 'static) {
    std::thread::spawn(move || {
        if let Err(e) = f() {
            eprintln!("flow-tray: {what}: {e}");
        }
    });
}

impl TrayCore {
    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// Carries out `a`; false when the tray should exit.
    pub fn handle(&mut self, a: Action) -> bool {
        match a {
            Action::State(s) => {
                self.state = *s;
                match &self.state {
                    AgentState::Running(snap) if model::needs_join(snap) && !self.first_run_shown => {
                        self.first_run_shown = true;
                        instance::open_window();
                    }
                    AgentState::Unreachable(_)
                        if self.start_agent && self.last_start.is_none_or(|t| t.elapsed() >= START_INTERVAL) =>
                    {
                        self.last_start = Some(Instant::now());
                        if let Err(e) = actions::start_agent() {
                            eprintln!("flow-tray: starting the agent: {e}");
                        }
                    }
                    _ => {}
                }
            }
            Action::OpenWindow => instance::open_window(),
            Action::OpenDashboard => {
                if let Some(url) = self.state.snapshot().and_then(|s| s.dashboard_url.clone()) {
                    background("opening the dashboard", move || actions::open(&url).map(|_| String::new()));
                }
            }
            Action::SetPaused(p) => background("pause/resume", move || actions::set_paused(p)),
            Action::RestartAgent => {
                self.last_start = Some(Instant::now());
                background("restarting the agent", actions::restart_agent)
            }
            Action::OpenLog => {
                let file = self.state.snapshot().and_then(|s| s.log_file.clone());
                background("opening the log", move || actions::open_log(file.as_deref()).map(|_| String::new()))
            }
            Action::Quit => {
                if self.quit_stops_agent {
                    let _ = actions::request(flow_ipc::Command::Shutdown);
                }
                return false;
            }
        }
        true
    }
}
