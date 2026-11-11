//! The rollback guard: `flow-agent update-guard --state-dir <dir>`, run from
//! the copy of the *previous* agent binary that an update saved, in its own
//! process (Linux: a transient systemd user unit when the agent is a
//! systemd service, so restarting `flow-agent.service` does not kill it).
//!
//! It watches the trial in `update-state.json` that the new version updates
//! as it starts ([`super::on_start`], [`super::on_backend_ready`]) and
//! clears once it reached the hub. If the new version
//!
//! * does not start within [`GuardOptions::start_timeout`] (30 s; e.g. it
//!   cannot even be loaded),
//! * keeps crashing ([`GuardOptions::max_starts`] starts),
//! * stops and stays down, or
//! * does not reach the hub within the trial timeout after its input
//!   backend came up,
//!
//! the guard puts the saved files back, records the failure (reported to the
//! hub by the restored version, never retried automatically) and restarts
//! the agent. On GNOME/Wayland the backend waits for the permission dialogs
//! (GNOME 46's Input Capture dialog has no restore token, so it appears
//! after every restart); the clock only starts once they are answered.

use super::state::{self, ServiceKind, Stage, Trial};
use anyhow::{Context, Result};
use flow_proto::update::FailedUpdate;
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

#[derive(Clone, Copy, Debug)]
pub struct GuardOptions {
    pub poll: Duration,
    /// From install (or a crash) to the new process running.
    pub start_timeout: Duration,
    pub max_starts: u32,
}

impl Default for GuardOptions {
    fn default() -> Self {
        GuardOptions { poll: Duration::from_millis(500), start_timeout: Duration::from_secs(30), max_starts: 4 }
    }
}

/// Whether the trial failed, and why. `alive(pid)` tells whether a process
/// runs; `now` is unix seconds.
pub fn verdict(t: &Trial, now: u64, opts: &GuardOptions, alive: &dyn Fn(u32) -> bool) -> Option<String> {
    let start = opts.start_timeout.as_secs();
    let since = |at: u64| now.saturating_sub(at);
    if t.starts > opts.max_starts {
        return Some(format!("the new version restarted {} times without reaching the hub", t.starts - 1));
    }
    match &t.stage {
        Stage::Installed if since(t.installed_at) > start => {
            Some(format!("the new version did not start within {start} s"))
        }
        Stage::Installed => None,
        Stage::Starting { pid, at, .. } | Stage::Connecting { pid, at } if !alive(*pid) && since(*at) > start => {
            Some("the new version stopped and did not start again".into())
        }
        // Waiting for the desktop's permission dialogs takes as long as the
        // user needs; other backends start in seconds.
        Stage::Starting { at, backend, .. } if backend != "wayland" && since(*at) > t.timeout_s => {
            Some(format!("the new version's input backend did not start within {} s", t.timeout_s))
        }
        Stage::Starting { .. } => None,
        Stage::Connecting { at, .. } if since(*at) > t.timeout_s => {
            Some(format!("the new version did not reach the hub within {} s", t.timeout_s))
        }
        Stage::Connecting { .. } => None,
    }
}

/// Starts the agent again after the files were restored.
pub trait Restarter {
    fn restart(&self, trial: &Trial) -> Result<()>;
}

/// Puts the previous version back: restore files, record the failure,
/// restart. Only one process gets to do it (`rolling_back`).
pub fn rollback(state_dir: &Path, reason: &str, restarter: &dyn Restarter) -> Result<()> {
    let trial = {
        let mut l = state::lock(state_dir)?;
        let Some(t) = l.state.trial.as_mut() else { return Ok(()) };
        if t.rolling_back {
            return Ok(());
        }
        t.rolling_back = true;
        let t = t.clone();
        l.save()?;
        t
    };
    warn!(from = %trial.from, to = %trial.to, "rolling back the update: {reason}");
    let restored = super::install::restore(&trial.backups);
    if let Err(e) = &restored {
        warn!("restoring the previous version: {e:#}");
    }
    {
        let mut l = state::lock(state_dir)?;
        l.state.failed = Some(FailedUpdate {
            target: trial.to.clone(),
            reason: reason.to_string(),
            rolled_back: restored.is_ok(),
            at: state::unix_now(),
        });
        l.state.trial = None;
        if trial
            .backups
            .iter()
            .any(|b| b.dest.file_name().is_some_and(|n| n.to_string_lossy().starts_with("flow-tray")))
        {
            l.state.tray_build = Some(trial.from.full());
        }
        l.save()?;
    }
    restarter.restart(&trial)
}

/// How a guard run ended.
#[derive(Debug, PartialEq, Eq)]
pub enum GuardEnd {
    /// The new version reached the hub (or there was nothing to watch).
    Done,
    RolledBack(String),
    /// Another guard watches already.
    AlreadyRunning,
}

/// Watches the trial until it ends.
pub fn run(state_dir: &Path, opts: &GuardOptions, restarter: &dyn Restarter) -> Result<GuardEnd> {
    let Some(_lock) = state::try_hold(&state::update_dir(state_dir).join(state::GUARD_LOCK))? else {
        return Ok(GuardEnd::AlreadyRunning);
    };
    let Some(first) = state::read(state_dir).trial else { return Ok(GuardEnd::Done) };
    info!(from = %first.from, to = %first.to, "watching the new version");
    loop {
        let t = match state::read(state_dir).trial {
            Some(t) if t.to == first.to => t,
            _ => {
                info!(to = %first.to, "the new version reached the hub; guard done");
                return Ok(GuardEnd::Done);
            }
        };
        let reason = if t.rolling_back {
            // A previous guard died half-way.
            Some("rollback interrupted earlier".to_string())
        } else {
            verdict(&t, state::unix_now(), opts, &process_alive)
        };
        if let Some(reason) = reason {
            if t.rolling_back {
                let mut l = state::lock(state_dir)?;
                if let Some(t) = l.state.trial.as_mut() {
                    t.rolling_back = false;
                }
                l.save()?;
            }
            rollback(state_dir, &reason, restarter)?;
            return Ok(GuardEnd::RolledBack(reason));
        }
        std::thread::sleep(opts.poll);
    }
}

/// True while process `pid` exists.
pub fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        rustix::process::Pid::from_raw(pid as i32).is_some_and(|p| rustix::process::test_kill_process(p).is_ok())
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: plain Win32 calls on a handle we own and close.
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            ok && code == STILL_ACTIVE as u32
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

/// Restarts the real agent: the systemd service, or the running agent
/// over IPC (it re-executes the restored file), or a fresh detached one.
pub struct ProcessRestarter;

impl Restarter for ProcessRestarter {
    fn restart(&self, t: &Trial) -> Result<()> {
        if t.service == ServiceKind::Systemd {
            if t.unit_changed {
                let _ = systemctl(&["daemon-reload"]);
            }
            return systemctl(&["restart", "flow-agent.service"]);
        }
        let ep = flow_ipc::endpoint::default_endpoint();
        if flow_ipc::client::request(&ep, &flow_ipc::Command::Restart).is_ok() {
            info!("asked the running agent to restart into the previous version");
            return Ok(());
        }
        let pid = match t.stage {
            Stage::Starting { pid, .. } | Stage::Connecting { pid, .. } => Some(pid),
            Stage::Installed => None,
        };
        if let Some(pid) = pid.filter(|p| process_alive(*p)) {
            kill(pid);
        }
        let mut cmd = std::process::Command::new(&t.agent_exe);
        cmd.arg("run");
        flow_ipc::detach::spawn_detached(&mut cmd).with_context(|| format!("starting {}", t.agent_exe.display()))?;
        info!(exe = %t.agent_exe.display(), "started the previous version");
        Ok(())
    }
}

fn systemctl(args: &[&str]) -> Result<()> {
    let st = std::process::Command::new("systemctl").arg("--user").args(args).status()?;
    anyhow::ensure!(st.success(), "systemctl --user {} failed", args.join(" "));
    Ok(())
}

fn kill(pid: u32) {
    #[cfg(unix)]
    if let Some(p) = rustix::process::Pid::from_raw(pid as i32) {
        let _ = rustix::process::kill_process(p, rustix::process::Signal::TERM);
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(100));
            if !process_alive(pid) {
                return;
            }
        }
        let _ = rustix::process::kill_process(p, rustix::process::Signal::KILL);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).status();
    }
}

/// Starts `guard_exe update-guard --state-dir <dir>` so that it outlives
/// the agent process and its restart.
pub fn launch_process(guard_exe: &Path, state_dir: &Path, service: ServiceKind) -> Result<()> {
    let args = |c: &mut std::process::Command| {
        c.arg("update-guard").arg("--state-dir").arg(state_dir);
    };
    if service == ServiceKind::Systemd {
        let mut c = std::process::Command::new("systemd-run");
        c.args(["--user", "--quiet", "--collect"])
            .arg(format!("--unit=flowcursor-update-guard-{:08x}", rand::random::<u32>()))
            .arg(guard_exe);
        args(&mut c);
        match c.status() {
            Ok(s) if s.success() => return Ok(()),
            Ok(s) => warn!("systemd-run failed ({s}); starting the guard directly"),
            Err(e) => warn!("systemd-run: {e}; starting the guard directly"),
        }
    }
    let mut c = std::process::Command::new(guard_exe);
    args(&mut c);
    flow_ipc::detach::spawn_detached(&mut c).with_context(|| format!("starting {}", guard_exe.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flow_proto::BuildId;

    fn trial(stage: Stage) -> Trial {
        Trial {
            from: BuildId::new("0.2.0", "1"),
            to: BuildId::new("0.2.0", "2"),
            installed_at: 1000,
            backups: vec![],
            service: ServiceKind::Plain,
            unit_changed: false,
            agent_exe: "/x".into(),
            timeout_s: 120,
            stage,
            starts: 1,
            rolling_back: false,
        }
    }

    #[test]
    fn verdicts() {
        let o = GuardOptions::default();
        let up = |_| true;
        let down = |_| false;
        // Not started yet.
        assert_eq!(verdict(&trial(Stage::Installed), 1030, &o, &up), None);
        assert!(verdict(&trial(Stage::Installed), 1031, &o, &up).unwrap().contains("did not start within 30 s"));
        // Starting: X11/Windows backends come up in seconds.
        let s = |b: &str| Stage::Starting { pid: 7, at: 1000, backend: b.into() };
        assert_eq!(verdict(&trial(s("x11")), 1100, &o, &up), None);
        assert!(verdict(&trial(s("x11")), 1121, &o, &up).unwrap().contains("backend"));
        // Wayland: waits for the permission dialogs as long as it runs.
        assert_eq!(verdict(&trial(s("wayland")), 9999, &o, &up), None);
        assert_eq!(verdict(&trial(s("wayland")), 1030, &o, &down), None, "a moment to restart");
        assert!(verdict(&trial(s("wayland")), 1031, &o, &down).unwrap().contains("stopped"));
        // Connecting: the trial timeout.
        let c = Stage::Connecting { pid: 7, at: 1000 };
        assert_eq!(verdict(&trial(c.clone()), 1100, &o, &up), None);
        assert!(verdict(&trial(c.clone()), 1121, &o, &up).unwrap().contains("reach the hub"));
        // A crash loop.
        let mut t = trial(c);
        t.starts = 5;
        assert!(verdict(&t, 1001, &o, &up).unwrap().contains("restarted 4 times"));
        assert!(process_alive(std::process::id()));
    }
}
