// Release builds on Windows use the GUI subsystem so that autostart does not
// open a console window; CLI output still works from a terminal because we
// attach to the parent console.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use flow_agent::backend::{self, BackendOptions};
use flow_agent::config::{default_config_path, AgentConfig, AgentState};
use flow_net::token::JoinToken;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};
use tracing_subscriber::{prelude::*, reload, EnvFilter};

/// FlowCursor agent: share mouse, keyboard and clipboard with the other
/// machines registered at your FlowCursor hub.
#[derive(Parser, Debug)]
#[command(version, about)]
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
    /// Join a hub with the token shown in its dashboard / log.
    Join {
        token: String,
        /// Hub address (host or host:port). Omit to use mDNS discovery.
        #[arg(long)]
        hub: Option<String>,
    },
    /// Show configuration and enrolment state.
    Status,
    /// Forget stored xdg-desktop-portal permissions (Linux): the next start
    /// asks again.
    ForgetPortal,
    /// Print the monitors the backend detects.
    Monitors {
        #[arg(long)]
        backend: Option<String>,
    },
}

fn init_logging(level: &str, file: Option<&std::path::Path>) -> flow_agent::LogHook {
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
    tracing_subscriber::registry().with(filter).with(stdout_layer).with(file_layer).init();
    let env_set = std::env::var("RUST_LOG").is_ok();
    Arc::new(move |lvl: &str| {
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
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    #[cfg(windows)]
    let has_console = flow_agent::backend::windows::attach_parent_console() || cfg!(debug_assertions);
    #[cfg(not(windows))]
    let has_console = true;
    let cli = Cli::parse();
    let cfg_path = cli.config.clone().unwrap_or_else(default_config_path);
    let mut cfg = AgentConfig::load(&cfg_path)?;
    match cli.cmd.unwrap_or(Cmd::Run { backend: None }) {
        Cmd::Join { token, hub } => {
            let t = JoinToken::parse(&token).context("this does not look like a FlowCursor join token (FC1-…)")?;
            let dir = cfg.state_dir();
            let mut st = AgentState::load(&dir)?;
            st.node_id = None;
            st.hub_fingerprint = None;
            st.revoked = false;
            st.pending_join_token = Some(t.encode());
            st.save(&dir)?;
            if let Some(h) = hub {
                cfg.hub = Some(h);
            }
            cfg.save(&cfg_path)?;
            println!("Token stored. Config: {}", cfg_path.display());
            println!("Start the agent (e.g. `systemctl --user restart flow-agent` or `flow-agent run`);");
            println!("it verifies the hub fingerprint from the token before sending it.");
            Ok(())
        }
        Cmd::Status => {
            let dir = cfg.state_dir();
            let st = AgentState::load(&dir)?;
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
            // Without a console (Windows autostart) log to a file by default.
            let log_file = cfg.log_file.clone().or_else(|| (!has_console).then(|| cfg.state_dir().join("agent.log")));
            let hook = init_logging(cfg.log_level.as_deref().unwrap_or("info"), log_file.as_deref());
            if let Some(l) = &cfg.log_level {
                hook(l);
            }
            let kind = backend::parse_kind(b.as_deref().or(cfg.backend.as_deref()))?;
            info!(?kind, config = %cfg_path.display(), "starting flow-agent {}", env!("CARGO_PKG_VERSION"));
            let (be, rx) = backend::create(kind, &BackendOptions { state_dir: cfg.state_dir() })
                .await
                .context("starting the input backend")?;
            if be.monitors().is_empty() {
                bail!("the backend reported no monitors");
            }
            let agent = flow_agent::start(cfg, be, rx, Some(hook)).await?;
            info!(p2p = %agent.p2p_addr, fingerprint = %agent.fingerprint, "agent running");
            wait_for_shutdown().await;
            info!("stopping");
            agent.shutdown().await;
            Ok(())
        }
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
