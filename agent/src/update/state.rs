//! What the updater remembers across restarts (`<state_dir>/update/`):
//!
//! * `update-state.json`: the last failed update (never retried on its
//!   own), the running trial of a just-installed version, and the build of
//!   the tray binary the last update or rollback put in place;
//! * `previous/`: copies of the files the last update replaced (the
//!   rollback source; kept until the next update);
//! * `staging/<build>/`: verified downloads waiting to be installed;
//! * `state.lock`: held for every read-modify-write of the state file
//!   (the agent and the rollback guard are separate processes).
//!
//! The JSON is read by the guard of the *previous* version and written by
//! the new one, so fields are only ever added (all default when missing)
//! and unknown fields are ignored.

use anyhow::{Context, Result};
use flow_proto::update::FailedUpdate;
use flow_proto::BuildId;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

pub const DIR: &str = "update";
const STATE_FILE: &str = "update-state.json";
const LOCK_FILE: &str = "state.lock";
pub const GUARD_LOCK: &str = "guard.lock";

pub fn update_dir(state_dir: &Path) -> PathBuf {
    state_dir.join(DIR)
}

pub fn previous_dir(state_dir: &Path) -> PathBuf {
    update_dir(state_dir).join("previous")
}

pub fn staging_root(state_dir: &Path) -> PathBuf {
    update_dir(state_dir).join("staging")
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// One file an update replaced or added.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Backup {
    /// Installed location.
    pub dest: PathBuf,
    /// Copy of the file before the update (None: it did not exist).
    pub saved: Option<PathBuf>,
    #[serde(default)]
    pub executable: bool,
}

/// How the agent process is restarted (and restarted again by a rollback).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKind {
    /// Not managed: re-exec in place (Unix) or a detached new process.
    #[default]
    Plain,
    /// The systemd user service `flow-agent.service`.
    Systemd,
}

/// How far the new version got (written by the new version).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case", tag = "stage")]
pub enum Stage {
    /// Files replaced, restart requested.
    #[default]
    Installed,
    /// The new process runs and is creating its input backend (on
    /// GNOME/Wayland this waits for the permission dialogs).
    Starting { pid: u32, at: u64, backend: String },
    /// Backend ready, connecting to the hub.
    Connecting { pid: u32, at: u64 },
}

/// A just-installed version that has not yet proved itself (reached the
/// hub in a full session). The guard rolls it back otherwise.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trial {
    pub from: BuildId,
    pub to: BuildId,
    pub installed_at: u64,
    pub backups: Vec<Backup>,
    #[serde(default)]
    pub service: ServiceKind,
    /// The systemd unit was replaced (a restart needs `daemon-reload`).
    #[serde(default)]
    pub unit_changed: bool,
    /// The agent executable, to start it again after a rollback.
    pub agent_exe: PathBuf,
    /// Seconds the new version has to reach the hub once its backend is up.
    pub timeout_s: u64,
    #[serde(default)]
    pub stage: Stage,
    /// Process starts of the new version (a crash loop rolls back).
    #[serde(default)]
    pub starts: u32,
    /// The guard is restoring the previous version.
    #[serde(default)]
    pub rolling_back: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Persisted {
    pub failed: Option<FailedUpdate>,
    pub trial: Option<Trial>,
    /// Build of the `flow-tray` binary the last update or rollback
    /// installed: a running tray of another build restarts itself.
    pub tray_build: Option<String>,
}

/// Exclusive access to the state file while held.
pub struct Locked {
    dir: PathBuf,
    _lock: File,
    pub state: Persisted,
}

impl Locked {
    /// Writes the state back (the lock stays held).
    pub fn save(&self) -> Result<()> {
        let body = serde_json::to_vec_pretty(&self.state)?;
        flow_net::identity::write_private(&self.dir.join(STATE_FILE), &body)
    }
}

/// Locks and reads the state of `state_dir` (blocks while another process
/// holds the lock; they only hold it for a file read or write).
pub fn lock(state_dir: &Path) -> Result<Locked> {
    let dir = update_dir(state_dir);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join(LOCK_FILE))?;
    f.lock().context("locking the update state")?;
    let state = read_unlocked(&dir);
    Ok(Locked { dir, _lock: f, state })
}

fn read_unlocked(dir: &Path) -> Persisted {
    std::fs::read(dir.join(STATE_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// The state without locking (display only).
pub fn read(state_dir: &Path) -> Persisted {
    read_unlocked(&update_dir(state_dir))
}

/// A lock file held for the life of a process (`Ok(None)`: another holds it).
pub fn try_hold(path: &Path) -> Result<Option<File>> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path)?;
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Some process (the guard) holds the lock file at `path`.
pub fn is_held(path: &Path) -> bool {
    matches!(try_hold(path), Ok(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_tolerant_reading() {
        let d = std::env::temp_dir().join(format!("flow-upd-state-{}", rand::random::<u64>()));
        {
            let mut l = lock(&d).unwrap();
            assert_eq!(l.state, Persisted::default());
            l.state.tray_build = Some("0.2.0+1".into());
            l.state.trial = Some(Trial {
                from: BuildId::new("0.2.0", "1"),
                to: BuildId::new("0.2.0", "2"),
                installed_at: 5,
                backups: vec![Backup { dest: "/x".into(), saved: None, executable: true }],
                service: ServiceKind::Systemd,
                unit_changed: false,
                agent_exe: "/x".into(),
                timeout_s: 120,
                stage: Stage::Starting { pid: 4, at: 6, backend: "x11".into() },
                starts: 1,
                rolling_back: false,
            });
            l.save().unwrap();
        }
        let s = read(&d);
        assert_eq!(s.trial.as_ref().unwrap().stage, Stage::Starting { pid: 4, at: 6, backend: "x11".into() });
        // A newer version's extra fields and an older version's missing ones.
        std::fs::write(update_dir(&d).join(STATE_FILE), br#"{"tray_build":"b","future":1}"#).unwrap();
        assert_eq!(read(&d).tray_build.as_deref(), Some("b"));
        // Garbage: start over rather than fail.
        std::fs::write(update_dir(&d).join(STATE_FILE), b"{").unwrap();
        assert_eq!(read(&d), Persisted::default());
        let g = update_dir(&d).join(GUARD_LOCK);
        let held = try_hold(&g).unwrap().unwrap();
        assert!(try_hold(&g).unwrap().is_none());
        drop(held);
        assert!(!is_held(&g));
        std::fs::remove_dir_all(d).ok();
    }
}
