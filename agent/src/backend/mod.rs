//! OS backends. Every platform-specific piece sits behind [`Backend`]; the
//! agent core only sees [`LocalEvent`]s coming in and calls the trait to
//! capture, inject and set the clipboard.

use anyhow::{bail, Result};
use flow_layout::{Edge, Side};
use flow_net::link::Action;
use flow_proto::{Button, Capabilities, ClipContent, Monitor};
use std::sync::Arc;
use tokio::sync::mpsc;

pub mod dib;
pub mod sim;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(windows)]
pub mod windows;

/// Events from the local machine.
#[derive(Clone, Debug, PartialEq)]
pub enum LocalEvent {
    /// The local pointer pushes against an edge that leads to another
    /// machine (local desktop coordinates). On Wayland this arrives when a
    /// pointer barrier fires, i.e. capture is already active.
    EdgeHit {
        side: Side,
        x: i32,
        y: i32,
    },
    /// Captured relative pointer motion (only while capturing).
    Motion {
        dx: f64,
        dy: f64,
    },
    /// Captured key (canonical HID usage; only while capturing).
    Key {
        usage: u32,
        down: bool,
    },
    Button {
        button: Button,
        down: bool,
    },
    /// Wheel in 1/120 notch units.
    Wheel {
        dx: i32,
        dy: i32,
    },
    /// The backend lost its capture (portal session closed, hook removed…).
    CaptureEnded,
    /// The local clipboard changed.
    Clipboard(ClipContent),
    MonitorsChanged(Vec<Monitor>),
}

pub type EventTx = mpsc::UnboundedSender<LocalEvent>;
pub type EventRx = mpsc::UnboundedReceiver<LocalEvent>;

/// Platform backend: capture, injection and clipboard.
///
/// Methods must not block for long: backends forward work to their own
/// threads (Windows message loop, portal/EIS tasks, X11 connection).
pub trait Backend: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn caps(&self) -> Capabilities;
    fn monitors(&self) -> Vec<Monitor>;
    /// Edges of this machine that lead to another machine (local coords).
    fn set_edges(&self, edges: Vec<Edge>);
    /// Start capturing: local input is swallowed and reported as events.
    fn begin_capture(&self);
    /// Stop capturing; optionally place the local pointer at (`x`,`y`).
    fn end_capture(&self, warp: Option<(i32, i32)>);
    /// Inject an action received from a controlling machine.
    fn inject(&self, action: &Action);
    fn set_clipboard(&self, content: &ClipContent);
    /// True when changing the edges is expensive (Wayland: new barriers
    /// recreate the capture session and may ask for permission again).
    /// The core then keeps the edges and refuses crossings at hit time
    /// instead of clearing them (e.g. while away).
    fn keep_edges_stable(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Sim,
    #[cfg(target_os = "linux")]
    Wayland,
    #[cfg(target_os = "linux")]
    X11,
    #[cfg(windows)]
    Windows,
}

pub fn parse_kind(s: Option<&str>) -> Result<BackendKind> {
    Ok(match s.unwrap_or("auto").trim().to_ascii_lowercase().as_str() {
        "sim" | "simulated" => BackendKind::Sim,
        #[cfg(target_os = "linux")]
        "wayland" | "portal" => BackendKind::Wayland,
        #[cfg(target_os = "linux")]
        "x11" | "xorg" => BackendKind::X11,
        #[cfg(windows)]
        "windows" | "win32" => BackendKind::Windows,
        "auto" | "" => auto_kind(),
        other => bail!("unknown or unsupported backend {other:?} on this OS"),
    })
}

#[cfg(target_os = "linux")]
fn auto_kind() -> BackendKind {
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    if session == "wayland" || (std::env::var_os("WAYLAND_DISPLAY").is_some() && session != "x11") {
        BackendKind::Wayland
    } else if std::env::var_os("DISPLAY").is_some() {
        BackendKind::X11
    } else {
        BackendKind::Sim
    }
}

#[cfg(windows)]
fn auto_kind() -> BackendKind {
    BackendKind::Windows
}

#[cfg(not(any(target_os = "linux", windows)))]
fn auto_kind() -> BackendKind {
    BackendKind::Sim
}

/// Options that some backends need.
#[derive(Clone, Debug, Default)]
pub struct BackendOptions {
    pub state_dir: std::path::PathBuf,
}

/// Creates the backend and the stream of local events.
pub async fn create(kind: BackendKind, opts: &BackendOptions) -> Result<(Arc<dyn Backend>, EventRx)> {
    let _ = opts;
    match kind {
        BackendKind::Sim => {
            let (b, rx) = sim::SimBackend::create(sim::default_monitors());
            Ok((b, rx))
        }
        #[cfg(target_os = "linux")]
        BackendKind::Wayland => linux::wayland::create(opts).await,
        #[cfg(target_os = "linux")]
        BackendKind::X11 => linux::x11::create(opts).await,
        #[cfg(windows)]
        BackendKind::Windows => windows::create(opts).await,
    }
}

/// Monitor containing a local point (for dead-corner checks).
pub fn monitor_at(mons: &[Monitor], x: i32, y: i32) -> Option<flow_proto::Rect> {
    mons.iter().map(|m| m.rect).find(|r| r.contains(x as f64, y as f64))
}
