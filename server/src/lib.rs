//! FlowCursor hub (`flow-server`).
//!
//! Control plane for all agents: node registry and pairing, screen layout,
//! focus tracking, settings, clipboard broker, peer address book, relay for
//! input when the direct agent ↔ agent path fails, mDNS advertisement and the
//! web dashboard.

pub mod conn;
pub mod state;
pub mod web;

use anyhow::{Context, Result};
use flow_net::endpoint::{bind, server_config, TransportOpts};
use flow_net::tls::AnyClient;
use flow_net::token::JoinToken;
use flow_net::Identity;
use flow_proto::{Fingerprint, ALPN_HUB};
use state::{HubState, Persisted};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

#[derive(Clone, Debug)]
pub struct HubConfig {
    pub data_dir: PathBuf,
    pub quic_addr: SocketAddr,
    /// Dashboard listen address (None = no dashboard).
    pub http_addr: Option<SocketAddr>,
    /// Overrides the persisted dashboard token.
    pub admin_token: Option<String>,
    pub mdns: bool,
    pub name: String,
}

/// Callback that applies a new log filter.
pub type LogLevelHook = Box<dyn Fn(&str) + Send + Sync>;

pub struct Hub {
    pub identity: Identity,
    pub state: Mutex<HubState>,
    pub admin_token: String,
    /// Log filter reload hook (set by the binary).
    pub on_log_level: Mutex<Option<LogLevelHook>>,
}

impl Hub {
    pub fn fingerprint(&self) -> Fingerprint {
        self.identity.fingerprint
    }

    pub fn join_token(&self) -> String {
        let st = self.state.lock().expect("poisoned");
        JoinToken::new(st.p.join_secret_bytes(), &self.identity.fingerprint).encode()
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state.lock().expect("hub state poisoned")
    }
}

/// A running hub.
pub struct HubHandle {
    pub hub: Arc<Hub>,
    pub quic_addr: SocketAddr,
    pub http_addr: Option<SocketAddr>,
    endpoint: flow_net::quinn::Endpoint,
    _mdns: Option<flow_net::mdns::Advertisement>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl HubHandle {
    pub fn join_token(&self) -> String {
        self.hub.join_token()
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.hub.fingerprint()
    }

    pub async fn shutdown(self) {
        self.endpoint.close(0u32.into(), b"hub shutting down");
        for t in &self.tasks {
            t.abort();
        }
        self.endpoint.wait_idle().await;
    }
}

pub async fn start(cfg: HubConfig) -> Result<HubHandle> {
    std::fs::create_dir_all(&cfg.data_dir).with_context(|| format!("creating {}", cfg.data_dir.display()))?;
    let identity = Identity::load_or_create(&cfg.data_dir, "hub", &cfg.name)?;
    let persisted = Persisted::load_or_create(&cfg.data_dir)?;
    let admin_token =
        cfg.admin_token.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| persisted.admin_token.clone());
    let hub = Arc::new(Hub {
        identity: identity.clone(),
        state: Mutex::new(HubState {
            data_dir: cfg.data_dir.clone(),
            p: persisted,
            online: Default::default(),
            focus: Default::default(),
            pairs: Default::default(),
            last_clip: None,
            next_conn_id: 1,
        }),
        admin_token,
        on_log_level: Mutex::new(None),
    });

    let sc = server_config(&identity, ALPN_HUB, Arc::new(AnyClient), TransportOpts::HUB)?;
    let endpoint = bind(cfg.quic_addr, Some(sc))?;
    let quic_addr = endpoint.local_addr()?;
    info!(%quic_addr, fingerprint = %identity.fingerprint, "hub listening (QUIC)");

    let mut tasks = Vec::new();
    {
        let hub = hub.clone();
        let ep = endpoint.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                tokio::spawn(conn::handle_incoming(hub.clone(), incoming));
            }
        }));
    }

    let http_addr = match cfg.http_addr {
        Some(addr) => {
            let listener =
                tokio::net::TcpListener::bind(addr).await.with_context(|| format!("binding dashboard {addr}"))?;
            let local = listener.local_addr()?;
            let app = web::router(hub.clone());
            tasks.push(tokio::spawn(async move {
                if let Err(e) = axum::serve(listener, app).await {
                    warn!("dashboard server stopped: {e}");
                }
            }));
            info!(addr = %local, "dashboard listening (HTTP)");
            Some(local)
        }
        None => None,
    };

    let mdns = if cfg.mdns {
        match flow_net::mdns::advertise(&cfg.name, quic_addr.port(), &identity.fingerprint) {
            Ok(a) => Some(a),
            Err(e) => {
                warn!("mDNS advertisement failed: {e:#}");
                None
            }
        }
    } else {
        None
    };

    Ok(HubHandle { hub, quic_addr, http_addr, endpoint, _mdns: mdns, tasks })
}
