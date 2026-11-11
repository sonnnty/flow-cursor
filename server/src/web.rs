//! Web dashboard: static page embedded in the binary plus a small JSON API
//! protected by a bearer token (`FLOW_ADMIN_TOKEN` or the generated token
//! printed at startup).

use crate::state::{unix_now, ClipMeta};
use crate::Hub;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use flow_proto::{Capabilities, FocusEntry, FromHub, HubSettings, Monitor, NodeId, OsKind, PathKind};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tracing::info;

const INDEX_HTML: &str = include_str!("../static/index.html");

pub fn router(hub: Arc<Hub>) -> Router {
    let api = Router::new()
        .route("/state", get(get_state))
        .route("/layout", put(put_layout))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/nodes/{id}", axum::routing::patch(patch_node).delete(delete_node))
        .route("/join-token/regenerate", post(regenerate_token))
        .layer(middleware::from_fn_with_state(hub.clone(), auth));
    Router::new().route("/", get(index)).nest("/api", api).with_state(hub).layer(middleware::from_fn(security_headers))
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(header::X_FRAME_OPTIONS, "DENY".parse().expect("static"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().expect("static"));
    h.insert(header::CACHE_CONTROL, "no-store".parse().expect("static"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        "default-src 'self'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src 'self' data:"
            .parse()
            .expect("static"),
    );
    res
}

async fn auth(State(hub): State<Arc<Hub>>, headers: HeaderMap, req: Request, next: Next) -> Response {
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if bool::from(given.as_bytes().ct_eq(hub.admin_token.as_bytes())) {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "missing or wrong dashboard token").into_response()
    }
}

#[derive(Serialize)]
struct HubInfo {
    fingerprint: String,
    version: &'static str,
    join_token: String,
}

#[derive(Serialize)]
struct NodeView {
    id: NodeId,
    name: String,
    hostname: String,
    fingerprint: String,
    os: OsKind,
    enabled: bool,
    online: bool,
    mouse_speed: f32,
    x: i32,
    y: i32,
    monitors: Vec<Monitor>,
    caps: Capabilities,
    remote: Option<SocketAddr>,
    p2p_addrs: Vec<SocketAddr>,
    agent_version: Option<String>,
    last_seen: u64,
}

#[derive(Serialize)]
struct PairView {
    source: NodeId,
    target: NodeId,
    path: PathKind,
    rtt_us: Option<u32>,
    p2p_rtt_us: Option<u32>,
    relay_rtt_us: Option<u32>,
}

#[derive(Serialize)]
struct StateView {
    hub: HubInfo,
    now: u64,
    nodes: Vec<NodeView>,
    focus: Vec<FocusEntry>,
    pairs: Vec<PairView>,
    last_clip: Option<ClipMeta>,
}

async fn get_state(State(hub): State<Arc<Hub>>) -> Json<StateView> {
    let join_token = hub.join_token();
    let st = hub.lock();
    let nodes =
        st.p.nodes
            .values()
            .map(|n| {
                let o = st.online.get(&n.id);
                NodeView {
                    id: n.id,
                    name: n.name.clone(),
                    hostname: n.hostname.clone(),
                    fingerprint: n.fingerprint.clone(),
                    os: n.os,
                    enabled: n.enabled,
                    online: o.is_some(),
                    mouse_speed: n.mouse_speed,
                    x: n.x,
                    y: n.y,
                    monitors: n.monitors.clone(),
                    caps: n.caps,
                    remote: o.map(|o| o.remote),
                    p2p_addrs: o.map(|o| o.p2p_addrs.clone()).unwrap_or_default(),
                    agent_version: o.map(|o| o.agent_version.clone()),
                    last_seen: if o.is_some() { unix_now() } else { n.last_seen },
                }
            })
            .collect();
    let mut pairs: Vec<PairView> = st
        .pairs
        .iter()
        .map(|((s, t), p)| PairView {
            source: *s,
            target: *t,
            path: p.path,
            rtt_us: p.rtt_us,
            p2p_rtt_us: p.p2p_rtt_us,
            relay_rtt_us: p.relay_rtt_us,
        })
        .collect();
    pairs.sort_by_key(|p| (p.source, p.target));
    Json(StateView {
        hub: HubInfo { fingerprint: hub.fingerprint().to_hex(), version: env!("CARGO_PKG_VERSION"), join_token },
        now: unix_now(),
        nodes,
        focus: st.focus_entries(),
        pairs,
        last_clip: st.last_clip.clone(),
    })
}

#[derive(Deserialize)]
struct Position {
    id: NodeId,
    x: i32,
    y: i32,
}

#[derive(Deserialize)]
struct LayoutUpdate {
    positions: Vec<Position>,
}

const COORD_LIMIT: i32 = 1_000_000;

async fn put_layout(State(hub): State<Arc<Hub>>, Json(u): Json<LayoutUpdate>) -> Result<StatusCode, ApiError> {
    let mut st = hub.lock();
    for p in &u.positions {
        if p.x.abs() > COORD_LIMIT || p.y.abs() > COORD_LIMIT {
            return Err(ApiError::bad("coordinates out of range"));
        }
        if !st.p.nodes.contains_key(&p.id.0) {
            return Err(ApiError::bad(format!("unknown node {}", p.id)));
        }
    }
    for p in u.positions {
        if let Some(n) = st.p.nodes.get_mut(&p.id.0) {
            n.x = p.x;
            n.y = p.y;
        }
    }
    st.save();
    st.broadcast_layout();
    info!("layout updated from dashboard");
    Ok(StatusCode::NO_CONTENT)
}

async fn get_settings(State(hub): State<Arc<Hub>>) -> Json<HubSettings> {
    Json(hub.lock().p.settings.clone())
}

pub fn validate_settings(s: &HubSettings) -> Result<(), String> {
    flow_keymap::parse_combo(&s.emergency_hotkey).map_err(|e| format!("emergency hotkey: {e}"))?;
    if s.emergency_hotkey.split('+').filter(|p| !p.trim().is_empty()).count() < 2 {
        return Err("emergency hotkey must combine at least two keys".into());
    }
    if !(100..=60_000).contains(&s.probe_interval_ms) {
        return Err("probe interval must be 100–60000 ms".into());
    }
    if s.edge.switch_delay_ms > 5000 || s.edge.double_bump_window_ms > 5000 || s.edge.dead_corner_px > 2000 {
        return Err("edge settings out of range".into());
    }
    if s.clipboard.max_text_bytes as usize > flow_proto::MAX_CLIPBOARD_FRAME
        || s.clipboard.max_image_bytes as usize > flow_proto::MAX_CLIPBOARD_FRAME
    {
        return Err(format!("clipboard limits must be ≤ {} bytes", flow_proto::MAX_CLIPBOARD_FRAME));
    }
    if s.log_level.trim().is_empty() || s.log_level.len() > 200 {
        return Err("log level must be a tracing filter like \"info\" or \"debug\"".into());
    }
    Ok(())
}

async fn put_settings(State(hub): State<Arc<Hub>>, Json(s): Json<HubSettings>) -> Result<Json<HubSettings>, ApiError> {
    validate_settings(&s).map_err(ApiError::bad)?;
    let level_changed;
    {
        let mut st = hub.lock();
        level_changed = st.p.settings.log_level != s.log_level;
        st.p.settings = s.clone();
        st.save();
        st.broadcast_settings();
    }
    if level_changed {
        if let Some(f) = hub.on_log_level.lock().expect("poisoned").as_ref() {
            f(&s.log_level);
        }
    }
    info!("settings updated from dashboard and pushed to agents");
    Ok(Json(s))
}

#[derive(Deserialize)]
struct NodePatch {
    name: Option<String>,
    enabled: Option<bool>,
    mouse_speed: Option<f32>,
}

async fn patch_node(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<u32>,
    Json(p): Json<NodePatch>,
) -> Result<StatusCode, ApiError> {
    let mut st = hub.lock();
    if let Some(name) = &p.name {
        let name = name.trim();
        if name.is_empty() || name.len() > 64 {
            return Err(ApiError::bad("name must be 1–64 characters"));
        }
        if st.p.nodes.values().any(|n| n.id.0 != id && n.name == name) {
            return Err(ApiError::bad("name already used"));
        }
    }
    if let Some(s) = p.mouse_speed {
        if !(0.1..=10.0).contains(&s) {
            return Err(ApiError::bad("mouse speed must be 0.1–10"));
        }
    }
    let node = NodeId(id);
    let n = st.p.nodes.get_mut(&id).ok_or_else(|| ApiError::not_found("no such node"))?;
    if let Some(name) = p.name {
        n.name = name.trim().to_string();
    }
    if let Some(s) = p.mouse_speed {
        n.mouse_speed = s;
    }
    let mut disabled = false;
    if let Some(e) = p.enabled {
        disabled = n.enabled && !e;
        n.enabled = e;
    }
    st.save();
    if disabled {
        st.release_focus_on(node);
        if st.focus.remove(&node).is_some() {
            st.send_to(node, FromHub::ReleaseControl);
        }
        st.broadcast_focus();
    }
    st.broadcast_peers();
    st.broadcast_layout();
    Ok(StatusCode::NO_CONTENT)
}

/// Removes a node and revokes its credentials (certificate fingerprint).
async fn delete_node(State(hub): State<Arc<Hub>>, Path(id): Path<u32>) -> Result<StatusCode, ApiError> {
    let node = NodeId(id);
    let conn = {
        let mut st = hub.lock();
        let rec = st.p.nodes.remove(&id).ok_or_else(|| ApiError::not_found("no such node"))?;
        info!(%node, name = %rec.name, "node removed, credentials revoked");
        st.save();
        st.release_focus_on(node);
        st.focus.remove(&node);
        st.pairs.retain(|(a, b), _| *a != node && *b != node);
        let conn = st.online.remove(&node).map(|o| {
            let _ = o.ctl.send(FromHub::Revoked);
            o.conn
        });
        st.broadcast_peers();
        st.broadcast_layout();
        st.broadcast_focus();
        conn
    };
    if let Some(c) = conn {
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            c.close(3u32.into(), b"revoked");
        });
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct TokenView {
    join_token: String,
}

async fn regenerate_token(State(hub): State<Arc<Hub>>) -> Json<TokenView> {
    {
        let mut st = hub.lock();
        st.p.regenerate_join_secret();
        st.save();
    }
    info!("join token regenerated");
    Json(TokenView { join_token: hub.join_token() })
}

pub struct ApiError(StatusCode, String);

impl ApiError {
    fn bad(m: impl Into<String>) -> Self {
        ApiError(StatusCode::BAD_REQUEST, m.into())
    }
    fn not_found(m: impl Into<String>) -> Self {
        ApiError(StatusCode::NOT_FOUND, m.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, self.1).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_validation() {
        let mut s = HubSettings::default();
        assert!(validate_settings(&s).is_ok());
        s.emergency_hotkey = "Escape".into();
        assert!(validate_settings(&s).is_err());
        s.emergency_hotkey = "Ctrl+Nope".into();
        assert!(validate_settings(&s).is_err());
        s = HubSettings { probe_interval_ms: 10, ..Default::default() };
        assert!(validate_settings(&s).is_err());
    }
}
