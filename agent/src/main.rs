// Release builds on Windows use the GUI subsystem so that autostart does not
// open a console window; CLI output still works from a terminal because we
// attach to the parent console.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use flow_agent::backend::{self, BackendOptions};
use flow_agent::config::{default_config_path, store_join, AgentConfig, AgentState};
use flow_agent::ipc::{AgentInfo, BindError, Control, IpcContext, IpcServer};
use flow_agent::logbuf::LogBuffer;
use flow_agent::StartOptions;
use flow_ipc::endpoint::default_endpoint;
use flow_ipc::{Command, Reply};
use flow_net::token::JoinToken;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::{prelude::*, reload, EnvFilter};

/// FlowCursor agent: share mouse, keyboard and clipboard with the other
/// machines registered at your FlowCursor hub.
#[derive(Parser, Debug)]
#[command(version = flow_version::FULL, about)]
struct Cli {
    /// Configuration file (TOML).
    #[arg(long, global = true, env = "FLOW_AGENT_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the agent (default).
    Run {
        /// auto | wayland | x11 | windows | sim
        #[arg(long)]
        backend: Option<String>,
    },
    /// Join a hub with the token shown in its dashboard / log. A running
    /// agent stores it and restarts to join.
    Join {
        token: String,
        /// Hub address (host or host:port). Omit to use mDNS discovery.
        #[arg(long)]
        hub: Option<String>,
    },
    /// Show configuration and enrolment state (and the live state of a
    /// running agent).
    Status,
    /// Pause sharing in the running agent (like being away) until resumed
    /// or restarted.
    Pause,
    /// Resume sharing in the running agent.
    Resume,
    /// Restart the running agent (re-reads its configuration).
    Restart,
    /// Stop the running agent (installers, upgrades). Exits 0 also when
    /// none is running.
    Stop,
    /// Install the build the hub carries now (as soon as this machine is
    /// idle), whatever the update policy; also retries a failed update.
    Update,
    /// Forget stored xdg-desktop-portal permissions (Linux): the next start
    /// asks again.
    ForgetPortal,
    /// Internal: watch a just-installed version and roll it back if it does
    /// not reach the hub (started by the agent after an update).
    #[command(hide = true)]
    UpdateGuard {
        #[arg(long)]
        state_dir: PathBuf,
    },
    /// Print the monitors the backend detects.
    Monitors {
        #[arg(long)]
        backend: Option<String>,
    },
}

/// Sets up logging to stdout or `file`, plus an in-memory copy of the last
/// lines for the status window (`flow-tray`).
fn init_logging(level: &str, file: Option<&std::path::Path>) -> (flow_agent::LogHook, LogBuffer) {
    let initial = std::env::var("RUST_LOG").unwrap_or_else(|_| level.to_string());
    let (filter, handle) = reload::Layer::new(EnvFilter::try_new(&initial).unwrap_or_else(|_| EnvFilter::new("info")));
    let file_layer = file.and_then(|p| {
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let f = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(p).ok()?;
        Some(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(std::sync::Mutex::new(f)))
    });
    let stdout_layer = file.is_none().then(tracing_subscriber::fmt::layer);
    let logs = LogBuffer::default();
    let mem_layer = tracing_subscriber::fmt::layer().with_ansi(false).with_writer(logs.clone());
    tracing_subscriber::registry().with(filter).with(stdout_layer).with(file_layer).with(mem_layer).init();
    let env_set = std::env::var("RUST_LOG").is_ok();
    let hook: flow_agent::LogHook = Arc::new(move |lvl: &str| {
        if env_set {
            return;
        }
        match EnvFilter::try_new(lvl) {
            Ok(f) => {
                let _ = handle.modify(|old| *old = f);
                info!("log level set to {lvl}");
            }
            Err(e) => warn!("ignoring invalid log level {lvl:?}: {e}"),
        }
    });
    (hook, logs)
}

#[tokio::main]
async fn main() -> Result<()> {
    #[cfg(windows)]
    let has_console = flow_agent::backend::windows::attach_parent_console() || cfg!(debug_assertions);
    #[cfg(not(windows))]
    let has_console = true;
    let mut cli = Cli::parse();
    let cfg_path = cli.config.clone().unwrap_or_else(default_config_path);
    let mut cfg = AgentConfig::load(&cfg_path)?;
    match cli.cmd.take().unwrap_or(Cmd::Run { backend: None }) {
        Cmd::Join { token, hub } => {
            let t = JoinToken::parse(token.trim())
                .context("this does not look like a FlowCursor join token (FC1-…)")?
                .encode();
            // A running agent stores the token itself after stopping its hub
            // client (no race with it saving its state) and restarts. Only
            // with the default config: it writes to its own config file.
            if cli.config.is_none() {
                if let Ok(reply) = ipc_request(&Command::Join { token: t.clone(), hub: hub.clone() }) {
                    match reply {
                        Reply::Ok { .. } => {
                            println!("Token handed to the running agent; it restarts and joins the hub.");
                            return Ok(());
                        }
                        Reply::Error { message } => bail!("the running agent refused the token: {message}"),
                        other => bail!("unexpected reply from the running agent: {other:?}"),
                    }
                }
            }
            store_join(&cfg_path, &mut cfg, &t, hub)?;
            println!("Token stored. Config: {}", cfg_path.display());
            println!("Start the agent (e.g. `systemctl --user restart flow-agent` or `flow-agent run`);");
            println!("it verifies the hub fingerprint from the token before sending it.");
            Ok(())
        }
        Cmd::Stop => {
            match ipc_request(&Command::Shutdown) {
                Ok(_) => {
                    // Gone once the endpoint stops answering.
                    for _ in 0..50 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        if flow_ipc::Client::connect(&default_endpoint()).is_err() {
                            break;
                        }
                    }
                    println!("agent stopped");
                }
                Err(_) => println!("no agent running"),
            }
            Ok(())
        }
        Cmd::UpdateGuard { state_dir } => {
            let log = flow_agent::update::state::update_dir(&state_dir).join("guard.log");
            let _log = init_logging("info", Some(&log));
            info!("update guard {} started", flow_version::FULL);
            let end = flow_agent::update::guard::run(
                &state_dir,
                &flow_agent::update::guard::GuardOptions::default(),
                &flow_agent::update::guard::ProcessRestarter,
            )?;
            info!(?end, "update guard done");
            Ok(())
        }
        cmd @ (Cmd::Pause | Cmd::Resume | Cmd::Restart | Cmd::Update) => {
            let cmd = match cmd {
                Cmd::Pause => Command::Pause,
                Cmd::Resume => Command::Resume,
                Cmd::Update => Command::UpdateNow,
                _ => Command::Restart,
            };
            match ipc_request(&cmd).context("no running agent answers on the local IPC endpoint")? {
                Reply::Ok { message } => {
                    println!("{message}");
                    Ok(())
                }
                Reply::Error { message } => bail!("{message}"),
                other => bail!("unexpected reply: {other:?}"),
            }
        }
        Cmd::Status => {
            let dir = cfg.state_dir();
            let st = AgentState::load(&dir)?;
            println!("version:          {} ({})", flow_version::FULL, flow_version::PLATFORM);
            println!("config file:      {}", cfg_path.display());
            println!("state directory:  {}", dir.display());
            println!("hub:              {}", cfg.hub.as_deref().unwrap_or("(mDNS discovery)"));
            println!(
                "node id:          {}",
                st.node_id.map(|n| n.to_string()).unwrap_or_else(|| "(not joined)".into())
            );
            println!("hub fingerprint:  {}", st.hub_fingerprint.as_deref().unwrap_or("(not pinned)"));
            println!("pending join:     {}", st.pending_join_token.is_some());
            println!("revoked:          {}", st.revoked);
            println!("portal tokens:    {}", dir.join(flow_agent::config::PORTAL_TOKENS_FILE).exists());
            // What the allowed-networks check compares against the CIDRs.
            let addrs: Vec<String> = {
                use flow_agent::netcheck::AddrSource;
                flow_agent::netcheck::SystemAddrs.addrs().iter().map(|a| a.to_string()).collect()
            };
            println!("interface addrs:  {}", if addrs.is_empty() { "(none)".into() } else { addrs.join(", ") });
            match ipc_request(&Command::Status) {
                Ok(Reply::Status { status: s }) => print_live_status(&s),
                _ => println!("running agent:    no (nothing answers on {})", default_endpoint().display()),
            }
            Ok(())
        }
        Cmd::ForgetPortal => {
            if flow_agent::config::forget_portal_tokens(&cfg.state_dir())? {
                println!("Portal permissions forgotten; the next start shows the permission dialogs again.");
            } else {
                println!("No stored portal permissions.");
            }
            Ok(())
        }
        Cmd::Monitors { backend: b } => {
            let _log = init_logging("warn", None);
            let kind = backend::parse_kind(b.as_deref().or(cfg.backend.as_deref()))?;
            let (be, _rx) = backend::create(kind, &BackendOptions { state_dir: cfg.state_dir() }).await?;
            for m in be.monitors() {
                println!(
                    "{:<20} {}x{} at {},{}{}",
                    m.name,
                    m.rect.w,
                    m.rect.h,
                    m.rect.x,
                    m.rect.y,
                    if m.primary { " (primary)" } else { "" }
                );
            }
            Ok(())
        }
        Cmd::Run { backend: b } => {
            // Set by a restart requested over IPC (see `restart_process`);
            // not passed on to anything this process starts.
            let restarted = std::env::var_os(ENV_RESTARTED).is_some();
            let start_paused = std::env::var_os(ENV_PAUSED).is_some();
            std::env::remove_var(ENV_RESTARTED);
            std::env::remove_var(ENV_PAUSED);
            // Without a console (Windows autostart) log to a file by default.
            let log_file = cfg.log_file.clone().or_else(|| (!has_console).then(|| cfg.state_dir().join("agent.log")));
            let (hook, logs) = init_logging(cfg.log_level.as_deref().unwrap_or("info"), log_file.as_deref());
            if let Some(l) = &cfg.log_level {
                hook(l);
            }
            let endpoint = default_endpoint();
            // Before the backend: on Wayland creating it opens permission
            // dialogs, which a second agent must not do.
            if !restarted && flow_ipc::Client::connect(&endpoint).is_ok() {
                bail!("{}", BindError::AlreadyRunning(endpoint));
            }
            let kind = backend::parse_kind(b.as_deref().or(cfg.backend.as_deref()))?;
            info!(?kind, config = %cfg_path.display(), "starting flow-agent {}", flow_version::FULL);
            // A version on trial after an update: count the start, make sure
            // the rollback guard watches.
            let current = flow_proto::BuildId::current();
            let launcher: flow_agent::update::GuardLauncher = Arc::new(flow_agent::update::guard::launch_process);
            let backend_name = format!("{kind:?}").to_lowercase();
            flow_agent::update::on_start(&cfg.state_dir(), &current, &backend_name, &launcher);
            let (be, rx) = backend::create(kind, &BackendOptions { state_dir: cfg.state_dir() })
                .await
                .context("starting the input backend")?;
            flow_agent::update::on_backend_ready(&cfg.state_dir(), &current);
            if be.monitors().is_empty() {
                bail!("the backend reported no monitors");
            }
            let opts = StartOptions {
                log_hook: Some(hook),
                paused: start_paused,
                bind_retry: if restarted { RESTART_GRACE } else { Duration::ZERO },
                ..Default::default()
            };
            let mut agent = flow_agent::start_with(cfg.clone(), be, rx, opts).await?;
            info!(p2p = %agent.p2p_addr, fingerprint = %agent.fingerprint, "agent running");
            let mut restarts = agent.restarts.take().expect("restart channel");

            let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel();
            let ctx = IpcContext {
                agent: agent.observer(),
                info: Arc::new(AgentInfo {
                    config_file: cfg_path.clone(),
                    log_file: log_file.clone(),
                    cfg: cfg.clone(),
                }),
                logs,
                control: ctl_tx,
            };
            let ipc = serve_ipc(&endpoint, ctx, restarted).await?;

            let control = tokio::select! {
                _ = wait_for_shutdown() => None,
                c = ctl_rx.recv() => c,
                Some(r) = restarts.recv() => Some(Control::Update(r)),
            };
            info!(?control, "stopping");
            let paused = agent.status().paused;
            agent.shutdown().await;
            if let Some(s) = ipc {
                s.close().await;
            }
            match control {
                None | Some(Control::Shutdown) => Ok(()),
                Some(Control::Restart) => restart_process(paused),
                Some(Control::Update(r)) => restart_after_update(r, paused),
                Some(Control::Join { token, hub }) => {
                    if let Err(e) = store_join(&cfg_path, &mut cfg, &token, hub) {
                        warn!("storing the join token: {e:#}");
                    }
                    restart_process(paused)
                }
            }
        }
    }
}

/// Environment of a process started by [`restart_process`].
const ENV_RESTARTED: &str = "FLOW_AGENT_RESTARTED";
const ENV_PAUSED: &str = "FLOW_AGENT_START_PAUSED";
/// How long a restarted agent waits for the old process to release the
/// P2P port and the IPC endpoint (Windows: both run for a moment).
const RESTART_GRACE: Duration = Duration::from_secs(5);

/// Opens the IPC endpoint. Another running agent is fatal (two agents
/// would fight over the input devices and the P2P port); any other
/// problem only costs the tray, so the agent keeps running without it.
async fn serve_ipc(endpoint: &std::path::Path, ctx: IpcContext, restarted: bool) -> Result<Option<IpcServer>> {
    let deadline = std::time::Instant::now() + if restarted { RESTART_GRACE } else { Duration::ZERO };
    loop {
        match flow_agent::ipc::serve(endpoint, ctx.clone()) {
            Ok(s) => return Ok(Some(s)),
            Err(e @ BindError::AlreadyRunning(_)) if std::time::Instant::now() >= deadline => bail!("{e}"),
            Err(BindError::AlreadyRunning(_)) => tokio::time::sleep(Duration::from_millis(200)).await,
            Err(e) => {
                warn!("no local IPC endpoint (the tray icon cannot reach this agent): {e}");
                return Ok(None);
            }
        }
    }
}

/// Replaces this process with a fresh agent started the same way. Unix:
/// `exec` in place (same pid, so systemd keeps tracking it). Windows: a new
/// detached process, then this one exits.
fn restart_process(paused: bool) -> Result<()> {
    restart_exe(&current_exe()?, paused)
}

/// [`restart_process`] with the executable to start.
fn restart_exe(exe: &std::path::Path, paused: bool) -> Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1)).env(ENV_RESTARTED, "1");
    if paused {
        cmd.env(ENV_PAUSED, "1");
    }
    info!(exe = %exe.display(), paused, "restarting");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = cmd.exec();
        Err(e).with_context(|| format!("restarting {}", exe.display()))
    }
    #[cfg(not(unix))]
    {
        flow_ipc::detach::spawn_detached(&mut cmd).with_context(|| format!("restarting {}", exe.display()))?;
        std::process::exit(0)
    }
}

/// Starts the version an update just installed. The usual restart path
/// (re-exec in place, same pid under systemd; Windows: a detached new
/// process), so stored portal permissions are reused as on any restart.
/// Only a changed systemd unit needs systemd itself to restart the service.
fn restart_after_update(r: flow_agent::update::RestartRequest, paused: bool) -> Result<()> {
    let exe = r.exe.clone();
    use flow_agent::update::state::ServiceKind;
    if r.service == ServiceKind::Systemd && r.unit_changed {
        info!("the systemd unit changed: restarting the service through systemd");
        let run = |args: &[&str]| std::process::Command::new("systemctl").arg("--user").args(args).status();
        let reload = run(&["daemon-reload"]);
        let restart = run(&["restart", "--no-block", "flow-agent.service"]);
        if reload.is_ok_and(|s| s.success()) && restart.is_ok_and(|s| s.success()) {
            return Ok(());
        }
        warn!("systemctl failed; restarting in place with the old unit");
    }
    restart_exe(&exe, paused)
}

/// The running executable. On Linux an upgrade may have replaced it: the
/// kernel then reports `<path> (deleted)` and the new file is at `<path>`.
fn current_exe() -> Result<std::path::PathBuf> {
    flow_agent::update::current_exe().context("locating flow-agent")
}

/// One request to the running agent of this user.
fn ipc_request(cmd: &Command) -> std::io::Result<Reply> {
    flow_ipc::client::request(&default_endpoint(), cmd)
}

fn print_live_status(s: &flow_ipc::Snapshot) {
    println!("running agent:    pid {} (version {}, backend {})", s.pid, s.agent_version, s.backend);
    let hub = if s.hub_connected {
        "connected".to_string()
    } else {
        format!("not connected{}", s.hub_error.as_deref().map(|e| format!(" ({e})")).unwrap_or_default())
    };
    println!("hub connection:   {hub}");
    println!("node name:        {}", s.node_name.as_deref().unwrap_or("-"));
    let sharing = match (s.paused, s.away) {
        (true, _) => "paused by the user",
        (false, true) => "away (not on an allowed network)",
        _ => "on",
    };
    println!("sharing:          {sharing}");
    if let Some(c) = &s.controlling {
        println!("controlling:      {}", c.name);
    }
    for c in &s.controlled_by {
        println!("controlled by:    {}", c.name);
    }
    for p in &s.peers {
        let state = match (p.online, p.away, p.enabled) {
            (_, _, false) => "disabled",
            (false, _, _) => "offline",
            (true, true, _) => "away",
            (true, false, _) => "online",
        };
        let path = p.path.as_deref().unwrap_or("-");
        let rtt = p.rtt_ms.map(|r| format!(" {r:.1} ms")).unwrap_or_default();
        println!("peer:             {} ({state}, {path}{rtt})", p.name);
    }
    for p in &s.problems {
        println!("problem:          {p}");
    }
    let u = &s.update;
    let mut line = format!("{} (policy {})", u.state, if u.policy.is_empty() { "-" } else { &u.policy });
    if let Some(a) = &u.available {
        line.push_str(&format!(", hub has {a}"));
    }
    if let Some(p) = u.progress {
        line.push_str(&format!(", {p}%"));
    }
    println!("update:           {line}");
    if let Some(d) = &u.detail {
        println!("update detail:    {d}");
    }
    if let Some(r) = &u.update_only {
        println!("update only:      {r}");
    }
    if let Some(f) = &u.failed {
        let rb = if u.failed_rolled_back { " (previous version restored)" } else { "" };
        println!("update failed:    {f}{rb}");
    }
}

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("signal handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
