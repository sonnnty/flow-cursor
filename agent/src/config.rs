//! Agent configuration (TOML, edited by the user) and agent state (JSON,
//! written by the agent: enrolment, pinned hub fingerprint, portal token).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// Hub address (`host` or `host:port`). Empty = discover with mDNS.
    pub hub: Option<String>,
    /// Pin the hub certificate (hex SHA-256). Normally learned from the
    /// join token and stored in the state file.
    pub hub_fingerprint: Option<String>,
    /// Join token for unattended provisioning (`flow-agent join` is the
    /// usual way). Ignored once the agent is enrolled.
    pub join_token: Option<String>,
    /// UDP port of the P2P listener (default 7879, 0 = random).
    pub p2p_port: Option<u16>,
    /// Addresses other agents should use to reach this one directly
    /// (default: the address the hub sees + p2p_port).
    pub p2p_advertise: Vec<SocketAddr>,
    /// auto | wayland | x11 | windows | sim
    pub backend: Option<String>,
    /// Local override of the emergency hotkey (works even without the hub).
    pub emergency_hotkey: Option<String>,
    /// Local override of the log filter.
    pub log_level: Option<String>,
    /// Where identity and state live (default: platform data dir).
    pub state_dir: Option<PathBuf>,
}

impl AgentConfig {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let s = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&s).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let body = format!("# FlowCursor agent configuration\n{}", toml::to_string_pretty(self)?);
        flow_net::identity::write_private(path, body.as_bytes())
    }

    pub fn state_dir(&self) -> PathBuf {
        self.state_dir.clone().unwrap_or_else(default_state_dir)
    }
}

pub fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "flowcursor")
}

pub fn default_config_path() -> PathBuf {
    project_dirs().map(|d| d.config_dir().join("agent.toml")).unwrap_or_else(|| PathBuf::from("flowcursor-agent.toml"))
}

pub fn default_state_dir() -> PathBuf {
    project_dirs().map(|d| d.data_local_dir().to_path_buf()).unwrap_or_else(|| PathBuf::from("flowcursor-state"))
}

/// Written by the agent itself.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgentState {
    pub node_id: Option<u32>,
    /// Full hub fingerprint, pinned after the first successful connection.
    pub hub_fingerprint: Option<String>,
    /// Last hub address that worked.
    pub hub_addr: Option<String>,
    /// Token from `flow-agent join`, cleared after enrolment.
    pub pending_join_token: Option<String>,
    /// Set when the hub revoked this agent; it will not re-enrol from a
    /// token in the config file until `flow-agent join` is run again.
    pub revoked: bool,
}

/// xdg-desktop-portal restore tokens (Linux), kept in their own file so the
/// backend and the hub client never overwrite each other's state.
pub const PORTAL_TOKENS_FILE: &str = "portal-tokens.json";

pub fn load_portal_token(dir: &Path, portal: &str) -> Option<String> {
    let s = std::fs::read_to_string(dir.join(PORTAL_TOKENS_FILE)).ok()?;
    let m: std::collections::BTreeMap<String, String> = serde_json::from_str(&s).ok()?;
    m.get(portal).cloned()
}

pub fn save_portal_token(dir: &Path, portal: &str, token: &str) -> Result<()> {
    let p = dir.join(PORTAL_TOKENS_FILE);
    let mut m: std::collections::BTreeMap<String, String> =
        std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    m.insert(portal.to_string(), token.to_string());
    std::fs::create_dir_all(dir)?;
    flow_net::identity::write_private(&p, &serde_json::to_vec_pretty(&m)?)
}

pub fn forget_portal_tokens(dir: &Path) -> Result<bool> {
    let p = dir.join(PORTAL_TOKENS_FILE);
    if p.exists() {
        std::fs::remove_file(&p)?;
        return Ok(true);
    }
    Ok(false)
}

pub const STATE_FILE: &str = "agent-state.json";

impl AgentState {
    pub fn load(dir: &Path) -> Result<Self> {
        let p = dir.join(STATE_FILE);
        if !p.exists() {
            return Ok(Self::default());
        }
        let s = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
        serde_json::from_str(&s).with_context(|| format!("parsing {}", p.display()))
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        flow_net::identity::write_private(&dir.join(STATE_FILE), &serde_json::to_vec_pretty(self)?)
    }
}

/// Parses `host`, `host:port`, `ip`, `ip:port`, `[v6]:port`.
pub async fn resolve_hub(s: &str) -> Result<SocketAddr> {
    let s = s.trim();
    if let Ok(a) = s.parse::<SocketAddr>() {
        return Ok(a);
    }
    if let Ok(ip) = s.parse::<std::net::IpAddr>() {
        return Ok(SocketAddr::new(ip, flow_proto::DEFAULT_HUB_PORT));
    }
    let with_port = if s.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        s.to_string()
    } else {
        format!("{s}:{}", flow_proto::DEFAULT_HUB_PORT)
    };
    let mut it = tokio::net::lookup_host(&with_port).await.with_context(|| format!("resolving {with_port}"))?;
    it.find(|a| a.is_ipv4()).context("hub name did not resolve to an IPv4 address")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip_and_unknown_keys() {
        let c: AgentConfig = toml::from_str("hub = \"10.0.0.2\"\np2p_port = 9000\n").unwrap();
        assert_eq!(c.hub.as_deref(), Some("10.0.0.2"));
        assert_eq!(c.p2p_port, Some(9000));
        assert!(toml::from_str::<AgentConfig>("hubb = 1").is_err());
        let dir = std::env::temp_dir().join(format!("flow-agent-cfg-{}", rand::random::<u64>()));
        let p = dir.join("agent.toml");
        c.save(&p).unwrap();
        assert_eq!(AgentConfig::load(&p).unwrap().p2p_port, Some(9000));
        let st = AgentState { node_id: Some(3), revoked: true, ..Default::default() };
        st.save(&dir).unwrap();
        assert_eq!(AgentState::load(&dir).unwrap(), st);
        assert_eq!(load_portal_token(&dir, "rd"), None);
        save_portal_token(&dir, "rd", "tok").unwrap();
        save_portal_token(&dir, "ic", "tok2").unwrap();
        assert_eq!(load_portal_token(&dir, "rd").as_deref(), Some("tok"));
        assert!(forget_portal_tokens(&dir).unwrap());
        assert_eq!(load_portal_token(&dir, "ic"), None);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn resolve() {
        assert_eq!(resolve_hub("127.0.0.1").await.unwrap(), "127.0.0.1:7878".parse().unwrap());
        assert_eq!(resolve_hub("127.0.0.1:9").await.unwrap(), "127.0.0.1:9".parse().unwrap());
        assert_eq!(resolve_hub("localhost:9").await.unwrap().port(), 9);
    }
}
