//! Self-update from the hub.
//!
//! The hub tells the agent its update policy and which build it carries for
//! this platform ([`flow_proto::update::Offer`], over the control stream of
//! the pinned, authenticated QUIC connection). When that build differs
//! from this one:
//!
//! * `auto`: download, then install as soon as this machine is idle;
//! * `notify`: report "available"; the tray offers "Update now";
//! * `off`: report "available"; nothing automatic.
//!
//! "Update now" (tray, `flow-agent update`, dashboard) works with every
//! policy, also for a build that failed before. Idle = neither controlling
//! nor controlled, not holding an edge, no keys or buttons held, and no
//! input for a few seconds ([`is_idle`]).
//!
//! Files are downloaded on the same connection and verified against the
//! offer ([`fetch`]), installed by the platform [`install::Installer`] with
//! a copy of every replaced file, and the agent restarts into the new
//! version. A separate guard process started from the *previous* agent
//! binary ([`guard`]) watches the new version: if it does not reach the hub
//! in a full session in time, the guard puts the previous files back,
//! restarts it and records the failure, which is reported to the hub and
//! never retried automatically.

pub mod fetch;
pub mod guard;
pub mod install;
pub mod state;

use crate::core::AgentStatus;
use flow_net::quinn::Connection;
use flow_proto::update::{FailedUpdate, Offer, Release, UpdateFromHub, UpdatePhase, UpdateReport, UpdateToHub};
use flow_proto::{BuildId, UpdatePolicy};
use install::Installer;
use state::{ServiceKind, Stage};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};

/// No input for this long before installing.
pub const QUIET: Duration = Duration::from_secs(3);
/// How long a new version has to reach the hub once its backend is up.
pub const TRIAL_TIMEOUT: Duration = Duration::from_secs(120);

/// From the hub client.
#[derive(Debug)]
pub enum UpdateEvent {
    /// A session started; `update_only` holds the hub's reason when the
    /// protocols differ. Downloads use `conn`.
    Session {
        conn: Connection,
        update_only: Option<String>,
    },
    Msg(UpdateFromHub),
    /// The session ended.
    Ended,
}

/// What the agent binary has to do after an update was installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestartRequest {
    pub service: ServiceKind,
    /// The systemd unit file changed: `daemon-reload` and a service restart
    /// instead of re-executing in place.
    pub unit_changed: bool,
    /// The installed agent executable to start (on Windows the running one
    /// was renamed aside).
    pub exe: PathBuf,
}

/// Starts the rollback guard: `(guard executable, state dir, service)`.
pub type GuardLauncher = Arc<dyn Fn(&Path, &Path, ServiceKind) -> anyhow::Result<()> + Send + Sync>;

/// What the tray and `flow-agent status` show.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UpdateStatus {
    /// Effective policy (None: no offer from the hub yet).
    pub policy: Option<UpdatePolicy>,
    pub phase: UpdatePhase,
    pub failed: Option<FailedUpdate>,
    /// Connected only to update (the hub's reason).
    pub update_only: Option<String>,
    /// Build of the tray binary the last update or rollback installed.
    pub tray_build: Option<String>,
    /// The build this agent runs.
    pub current: BuildId,
}

/// The parts of [`crate::StartOptions`] that configure updates.
#[derive(Clone)]
pub struct UpdateOptions {
    /// Where files go (None: by platform, for the running executable).
    pub installer: Option<Arc<dyn Installer>>,
    /// The build this agent claims to be (tests simulate older agents).
    pub current: BuildId,
    pub platform: String,
    pub quiet: Duration,
    pub trial_timeout: Duration,
    /// None: [`guard::launch_process`].
    pub guard: Option<GuardLauncher>,
}

impl Default for UpdateOptions {
    fn default() -> Self {
        UpdateOptions {
            installer: None,
            current: BuildId::current(),
            platform: flow_version::PLATFORM.to_string(),
            quiet: QUIET,
            trial_timeout: TRIAL_TIMEOUT,
            guard: None,
        }
    }
}

/// Handle to the running updater.
#[derive(Clone)]
pub struct UpdaterHandle {
    cmds: mpsc::UnboundedSender<()>,
    status: watch::Receiver<UpdateStatus>,
}

impl UpdaterHandle {
    /// Update now (waits for idle like any update).
    pub fn update_now(&self) {
        let _ = self.cmds.send(());
    }

    pub fn status(&self) -> UpdateStatus {
        self.status.borrow().clone()
    }

    pub fn watch(&self) -> watch::Receiver<UpdateStatus> {
        self.status.clone()
    }
}

/// True when installing now cannot disturb anyone.
pub fn is_idle(s: &AgentStatus, quiet: Duration, now: Instant) -> bool {
    s.controlling.is_none()
        && s.controlled_by.is_empty()
        && !s.holding_edge
        && !s.keys_held
        && s.last_input.is_none_or(|t| now.saturating_duration_since(t) >= quiet)
}

enum Dl {
    Progress { build: String, done: u64, total: u64 },
    Done { build: String, result: Result<(), String>, session_lost: bool },
}

struct Updater {
    state_dir: PathBuf,
    installer: Arc<dyn Installer>,
    opts: UpdateOptions,
    guard: GuardLauncher,
    offer: Option<Offer>,
    conn: Option<Connection>,
    update_only: Option<String>,
    manual: bool,
    /// Build whose files are staged and verified.
    staged: Option<String>,
    download: Option<(String, tokio::task::JoinHandle<()>)>,
    progress: (u64, u64),
    failed: Option<FailedUpdate>,
    tray_build: Option<String>,
    /// `installer.check()` for the current offer (the Windows check writes
    /// a probe file: not on every tick).
    supported: Option<Result<(), String>>,
    /// Install done, restart requested: nothing else happens.
    finished: bool,
    phase: UpdatePhase,
    status_tx: watch::Sender<UpdateStatus>,
    to_hub: mpsc::UnboundedSender<UpdateToHub>,
    restart: mpsc::UnboundedSender<RestartRequest>,
    dl_tx: mpsc::UnboundedSender<Dl>,
    agent: watch::Receiver<AgentStatus>,
}

/// Starts the updater task.
pub fn spawn(
    state_dir: PathBuf,
    opts: UpdateOptions,
    events: mpsc::UnboundedReceiver<UpdateEvent>,
    agent: watch::Receiver<AgentStatus>,
    to_hub: mpsc::UnboundedSender<UpdateToHub>,
    restart: mpsc::UnboundedSender<RestartRequest>,
) -> (UpdaterHandle, tokio::task::JoinHandle<()>) {
    let installer = opts.installer.clone().unwrap_or_else(|| {
        let exe = crate::update::current_exe().unwrap_or_default();
        install::for_this_process(exe)
    });
    let guard = opts.guard.clone().unwrap_or_else(|| Arc::new(guard::launch_process));
    let mut persisted = state::read(&state_dir);
    // A tray build recorded by an older update than the version running now
    // (installed by hand since): no tray should restart for it.
    if persisted.trial.is_none() && persisted.tray_build.as_ref().is_some_and(|b| *b != opts.current.full()) {
        if let Ok(mut l) = state::lock(&state_dir) {
            l.state.tray_build = None;
            let _ = l.save();
        }
        persisted.tray_build = None;
    }
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (status_tx, status_rx) = watch::channel(UpdateStatus {
        failed: persisted.failed.clone(),
        tray_build: persisted.tray_build.clone(),
        current: opts.current.clone(),
        ..Default::default()
    });
    let (dl_tx, dl_rx) = mpsc::unbounded_channel();
    let u = Updater {
        state_dir,
        installer,
        guard,
        offer: None,
        conn: None,
        update_only: None,
        manual: false,
        staged: None,
        download: None,
        progress: (0, 0),
        failed: persisted.failed,
        tray_build: persisted.tray_build,
        supported: None,
        finished: false,
        phase: UpdatePhase::Current,
        status_tx,
        to_hub,
        restart,
        dl_tx,
        agent,
        opts,
    };
    let task = tokio::spawn(u.run(events, cmd_rx, dl_rx));
    (UpdaterHandle { cmds: cmd_tx, status: status_rx }, task)
}

impl Updater {
    async fn run(
        mut self,
        mut events: mpsc::UnboundedReceiver<UpdateEvent>,
        mut cmds: mpsc::UnboundedReceiver<()>,
        mut dl: mpsc::UnboundedReceiver<Dl>,
    ) {
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            tokio::select! {
                ev = events.recv() => match ev {
                    Some(ev) => self.on_event(ev),
                    None => break,
                },
                Some(()) = cmds.recv() => {
                    info!("update requested (Update now)");
                    self.manual = true;
                }
                Some(d) = dl.recv() => self.on_download(d),
                _ = tick.tick() => {}
            }
            self.step();
        }
    }

    fn on_event(&mut self, ev: UpdateEvent) {
        match ev {
            UpdateEvent::Session { conn, update_only } => {
                if update_only.is_none() {
                    self.confirm_trial();
                }
                self.conn = Some(conn);
                self.update_only = update_only;
                // The hub learns the state at every connect.
                self.report();
            }
            UpdateEvent::Msg(UpdateFromHub::Offer(o)) => {
                if let Some(r) = &o.release {
                    if let Err(e) = r.validate() {
                        warn!("ignoring an invalid update offer: {e}");
                        return;
                    }
                }
                self.offer = Some(o);
                self.supported = None;
            }
            UpdateEvent::Msg(UpdateFromHub::UpdateNow) => {
                info!("update requested from the dashboard");
                self.manual = true;
            }
            UpdateEvent::Ended => {
                self.conn = None;
                self.offer = None;
                self.update_only = None;
                if let Some((_, t)) = self.download.take() {
                    t.abort();
                }
            }
        }
    }

    /// The new version reached the hub in a full session: it stays.
    fn confirm_trial(&mut self) {
        let current = self.opts.current.clone();
        let Ok(mut l) = state::lock(&self.state_dir) else { return };
        let Some(t) = l.state.trial.clone() else { return };
        if t.to != current || t.rolling_back {
            return;
        }
        info!(from = %t.from, to = %t.to, "update confirmed: this version reached the hub");
        l.state.trial = None;
        if l.state.failed.as_ref().is_some_and(|f| f.target == current) {
            l.state.failed = None;
        }
        if let Err(e) = l.save() {
            warn!("saving the update state: {e:#}");
        }
        self.failed = l.state.failed.clone();
        drop(l);
        // Downloads of this or older builds are no longer needed.
        let _ = std::fs::remove_dir_all(state::staging_root(&self.state_dir));
    }

    fn on_download(&mut self, d: Dl) {
        match d {
            Dl::Progress { build, done, total } => {
                if self.download.as_ref().is_some_and(|(b, _)| *b == build) {
                    self.progress = (done, total);
                }
            }
            Dl::Done { build, result, session_lost } => {
                if self.download.as_ref().is_some_and(|(b, _)| *b == build) {
                    self.download = None;
                }
                match result {
                    Ok(()) => {
                        info!(%build, "update downloaded and verified");
                        self.staged = Some(build);
                    }
                    // Lost connection: the next session tries again.
                    Err(e) if session_lost => warn!(%build, "update download interrupted: {e}"),
                    Err(e) => {
                        warn!(%build, "update download refused: {e}");
                        let target = self.target().map(|r| r.id).unwrap_or_else(|| BuildId::new("?", build));
                        self.fail(target, e, false);
                    }
                }
            }
        }
    }

    fn fail(&mut self, target: BuildId, reason: String, rolled_back: bool) {
        let f = FailedUpdate { target, reason, rolled_back, at: state::unix_now() };
        if let Ok(mut l) = state::lock(&self.state_dir) {
            l.state.failed = Some(f.clone());
            let _ = l.save();
        }
        self.failed = Some(f);
        self.manual = false;
        self.report();
    }

    /// The release to move to, if the hub carries another build for us.
    fn target(&self) -> Option<Release> {
        let r = self.offer.as_ref()?.release.as_ref()?;
        (r.id != self.opts.current && r.platform == self.opts.platform).then(|| r.clone())
    }

    fn step(&mut self) {
        if self.finished {
            return;
        }
        let phase = self.decide();
        if phase != self.phase {
            self.phase = phase;
            self.report();
        } else {
            self.publish();
        }
    }

    fn decide(&mut self) -> UpdatePhase {
        let Some(rel) = self.target() else {
            self.manual = false;
            return UpdatePhase::Current;
        };
        let target = rel.id.clone();
        let installer = self.installer.clone();
        if let Err(reason) = self.supported.get_or_insert_with(|| installer.check()).clone() {
            self.manual = false;
            return UpdatePhase::Unsupported { reason };
        }
        let policy = self.offer.as_ref().map(|o| o.policy).unwrap_or_default();
        let failed_before = self.failed.as_ref().is_some_and(|f| f.target == target);
        let wanted = self.manual || (policy == UpdatePolicy::Auto && !failed_before);
        if !wanted {
            return UpdatePhase::Available { target };
        }
        if self.staged.as_deref() == Some(target.build.as_str()) {
            let idle = is_idle(&self.agent.borrow(), self.opts.quiet, Instant::now());
            if !idle {
                return UpdatePhase::Ready { target };
            }
            self.phase = UpdatePhase::Installing { target: target.clone() };
            self.report();
            return match self.install(&rel) {
                Ok(req) => {
                    info!(to = %target, ?req, "update installed; restarting into the new version");
                    self.finished = true;
                    let _ = self.restart.send(req);
                    UpdatePhase::Installing { target }
                }
                Err(e) => {
                    warn!(to = %target, "installing the update failed (nothing changed): {e:#}");
                    self.staged = None;
                    self.fail(target.clone(), format!("install failed: {e:#}"), false);
                    UpdatePhase::Available { target }
                }
            };
        }
        if let Some((b, _)) = &self.download {
            if *b == target.build {
                let (done, total) = self.progress;
                return UpdatePhase::Downloading { target, done, total };
            }
        }
        match self.conn.clone() {
            Some(conn) => {
                self.start_download(conn, rel);
                UpdatePhase::Downloading { target, done: 0, total: self.progress.1 }
            }
            None => UpdatePhase::Available { target },
        }
    }

    fn start_download(&mut self, conn: Connection, rel: Release) {
        if let Some((_, t)) = self.download.take() {
            t.abort();
        }
        let files = self.installer.wanted(&rel);
        let build = rel.id.build.clone();
        let dir = match fetch::staging_dir(&state::staging_root(&self.state_dir), &build) {
            Ok(d) => d,
            Err(e) => {
                self.fail(rel.id.clone(), format!("{e:#}"), false);
                return;
            }
        };
        self.progress = (0, files.iter().map(|f| f.size).sum());
        info!(to = %rel.id, files = files.len(), bytes = self.progress.1, "downloading update from the hub");
        let tx = self.dl_tx.clone();
        let b = build.clone();
        let task = tokio::spawn(async move {
            let last = std::sync::Mutex::new(Instant::now());
            let progress = |done: u64, total: u64| {
                let mut l = last.lock().expect("poisoned");
                if done == total || l.elapsed() >= Duration::from_millis(250) {
                    *l = Instant::now();
                    let _ = tx.send(Dl::Progress { build: b.clone(), done, total });
                }
            };
            let result = fetch::download(&conn, &rel, &files, &dir, &progress).await.map_err(|e| format!("{e:#}"));
            let session_lost = result.is_err() && conn.close_reason().is_some();
            let _ = tx.send(Dl::Done { build: b.clone(), result, session_lost });
        });
        self.download = Some((build, task));
    }

    /// Replaces the files, records the trial and starts the guard.
    fn install(&mut self, rel: &Release) -> anyhow::Result<RestartRequest> {
        let staged = fetch::staging_dir(&state::staging_root(&self.state_dir), &rel.id.build)?;
        for f in self.installer.wanted(rel) {
            if !fetch::staged_ok(&staged, &f) {
                anyhow::bail!("staged {} changed since it was verified", f.name);
            }
        }
        let plan = self.installer.plan(rel)?;
        let ops = install::changes(&plan, &staged)?;
        let mut l = state::lock(&self.state_dir)?;
        if let Some(t) = &l.state.trial {
            anyhow::bail!("the update to {} has not finished yet", t.to);
        }
        let backups = install::apply(&ops, &staged, &state::previous_dir(&self.state_dir))?;
        let unit_changed = ops.iter().any(|o| o.name == "flow-agent.service");
        let tray_changed = ops.iter().any(|o| o.name.starts_with("flow-tray"));
        // The guard runs the previous agent binary (it outlives this one).
        let guard_exe = backups
            .iter()
            .find(|b| b.dest == plan.agent_exe)
            .and_then(|b| b.saved.clone())
            .unwrap_or_else(|| plan.agent_exe.clone());
        l.state.trial = Some(state::Trial {
            from: self.opts.current.clone(),
            to: rel.id.clone(),
            installed_at: state::unix_now(),
            backups,
            service: plan.service,
            unit_changed,
            agent_exe: plan.agent_exe.clone(),
            timeout_s: self.opts.trial_timeout.as_secs().max(1),
            stage: Stage::Installed,
            starts: 0,
            rolling_back: false,
        });
        if tray_changed {
            l.state.tray_build = Some(rel.id.full());
            self.tray_build = l.state.tray_build.clone();
        }
        l.save()?;
        drop(l);
        self.installer.after_install(rel);
        if let Err(e) = (self.guard)(&guard_exe, &self.state_dir, plan.service) {
            // The new version starts its own guard if none runs.
            warn!("starting the update guard: {e:#}");
        }
        Ok(RestartRequest { service: plan.service, unit_changed, exe: plan.agent_exe })
    }

    fn status(&self) -> UpdateStatus {
        UpdateStatus {
            policy: self.offer.as_ref().map(|o| o.policy),
            phase: self.phase.clone(),
            failed: self.failed.clone(),
            update_only: self.update_only.clone(),
            tray_build: self.tray_build.clone(),
            current: self.opts.current.clone(),
        }
    }

    fn publish(&self) {
        let s = self.status();
        self.status_tx.send_if_modified(|old| {
            if *old != s {
                *old = s;
                true
            } else {
                false
            }
        });
    }

    fn report(&self) {
        self.publish();
        let r = UpdateReport { phase: self.phase.clone(), failed: self.failed.clone(), ext: vec![] };
        let _ = self.to_hub.send(UpdateToHub::Report(r));
    }
}

/// The running executable; after an update replaced it on Linux the kernel
/// reports `<path> (deleted)` and the new file is at `<path>`.
pub fn current_exe() -> std::io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    if !exe.exists() {
        if let Some(p) = exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
            return Ok(p.into());
        }
    }
    Ok(exe)
}

/// At process start (before the input backend): a version on trial counts
/// its start and makes sure a guard watches it (after a reboot the first
/// guard is gone). Also removes leftovers of earlier Windows updates.
pub fn on_start(state_dir: &Path, current: &BuildId, backend: &str, launch: &GuardLauncher) {
    if let Ok(exe) = current_exe() {
        if let Some(d) = exe.parent() {
            install::remove_leftovers(d);
        }
    }
    let Ok(mut l) = state::lock(state_dir) else { return };
    let Some(t) = l.state.trial.as_mut() else { return };
    if t.to != *current || t.rolling_back {
        return;
    }
    t.starts += 1;
    t.stage = Stage::Starting { pid: std::process::id(), at: state::unix_now(), backend: backend.to_string() };
    let guard_exe = t.backups.iter().find(|b| b.dest == t.agent_exe).and_then(|b| b.saved.clone());
    let service = t.service;
    info!(from = %t.from, to = %t.to, start = t.starts, "starting a newly installed version (on trial)");
    let _ = l.save();
    drop(l);
    if !state::is_held(&state::update_dir(state_dir).join(state::GUARD_LOCK)) {
        if let Some(g) = guard_exe {
            if let Err(e) = launch(&g, state_dir, service) {
                warn!("starting the update guard: {e:#}");
            }
        }
    }
}

/// The backend is up: the trial clock for reaching the hub starts.
pub fn on_backend_ready(state_dir: &Path, current: &BuildId) {
    let Ok(mut l) = state::lock(state_dir) else { return };
    let Some(t) = l.state.trial.as_mut() else { return };
    if t.to == *current && !t.rolling_back {
        t.stage = Stage::Connecting { pid: std::process::id(), at: state::unix_now() };
        let _ = l.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flow_proto::NodeId;

    #[test]
    fn idle_needs_quiet_no_focus_and_no_keys() {
        let now = Instant::now();
        let q = Duration::from_secs(3);
        let s = AgentStatus::default();
        assert!(is_idle(&s, q, now));
        assert!(!is_idle(&AgentStatus { controlling: Some(NodeId(2)), ..s.clone() }, q, now));
        assert!(!is_idle(&AgentStatus { controlled_by: vec![NodeId(2)], ..s.clone() }, q, now));
        assert!(!is_idle(&AgentStatus { holding_edge: true, ..s.clone() }, q, now));
        assert!(!is_idle(&AgentStatus { keys_held: true, ..s.clone() }, q, now));
        let recent = AgentStatus { last_input: Some(now - Duration::from_secs(1)), ..s.clone() };
        assert!(!is_idle(&recent, q, now));
        assert!(is_idle(&recent, q, now + Duration::from_secs(5)));
    }
}
