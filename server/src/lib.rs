//! FlowCursor hub (`flow-server`).
//!
//! Control plane for all agents: node registry and pairing, screen layout,
//! focus tracking, settings, clipboard broker, peer address book, relay for
//! input when the direct agent ↔ agent path fails, mDNS advertisement and the
//! web dashboard.

pub mod conn;
pub mod state;
pub mod updates;
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
    /// Overrides the persisted dashboard token. Empty or blank counts as
    /// unset (compose passes `FLOW_ADMIN_TOKEN: ${FLOW_ADMIN_TOKEN:-}`).
    pub admin_token: Option<String>,
    pub mdns: bool,
    pub name: String,
    /// Directory with the agent builds this hub offers for self-update
    /// (see [`updates`]); None or missing = no updates offered.
    pub agent_builds: Option<PathBuf>,
}

/// Callback that applies a new log filter.
pub type LogLevelHook = Box<dyn Fn(&str) + Send + Sync>;

/// Where the dashboard token in use comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdminTokenSource {
    /// `HubConfig::admin_token` (FLOW_ADMIN_TOKEN / --admin-token).
    Configured,
    /// The random token stored in the data directory; the operator only
    /// learns it from the startup log.
    Persisted,
}

impl HubConfig {
    /// The dashboard token to use and where it comes from: the configured
    /// one unless it is unset, empty or blank, else the persisted one.
    pub fn effective_admin_token(&self, persisted: &str) -> (String, AdminTokenSource) {
        match self.admin_token.as_deref().filter(|t| !t.trim().is_empty()) {
            Some(t) => (t.to_string(), AdminTokenSource::Configured),
            None => (persisted.to_string(), AdminTokenSource::Persisted),
        }
    }
}

pub struct Hub {
    pub identity: Identity,
    pub state: Mutex<HubState>,
    pub admin_token: String,
    pub admin_token_source: AdminTokenSource,
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

/// Join token of the hub whose state lives in `data_dir`, without starting
/// it (works while the hub is running).
pub fn join_token_from_dir(data_dir: &std::path::Path, name: &str) -> Result<String> {
    let identity = Identity::load_or_create(data_dir, "hub", name)?;
    let p = Persisted::load_or_create(data_dir)?;
    Ok(JoinToken::new(p.join_secret_bytes(), &identity.fingerprint).encode())
}

pub async fn start(cfg: HubConfig) -> Result<HubHandle> {
    std::fs::create_dir_all(&cfg.data_dir).with_context(|| format!("creating {}", cfg.data_dir.display()))?;
    let identity = Identity::load_or_create(&cfg.data_dir, "hub", &cfg.name)?;
    let persisted = Persisted::load_or_create(&cfg.data_dir)?;
    let (admin_token, admin_token_source) = cfg.effective_admin_token(&persisted.admin_token);
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
            updating: Default::default(),
            legacy: Default::default(),
            builds: cfg.agent_builds.as_deref().and_then(updates::load_logged),
            last_relayout: None,
        }),
        admin_token,
        admin_token_source,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_admin_token_counts_as_unset() {
        let cfg = |t: Option<&str>| HubConfig {
            data_dir: PathBuf::new(),
            quic_addr: "127.0.0.1:0".parse().unwrap(),
            http_addr: None,
            admin_token: t.map(Into::into),
            mdns: false,
            name: "t".into(),
            agent_builds: None,
        };
        let stored = ("stored".to_string(), AdminTokenSource::Persisted);
        assert_eq!(cfg(None).effective_admin_token("stored"), stored);
        assert_eq!(cfg(Some("")).effective_admin_token("stored"), stored);
        assert_eq!(cfg(Some(" \t")).effective_admin_token("stored"), stored);
        assert_eq!(
            cfg(Some("secret")).effective_admin_token("stored"),
            ("secret".to_string(), AdminTokenSource::Configured)
        );
    }
}
