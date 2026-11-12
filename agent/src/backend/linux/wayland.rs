//! Wayland backend through xdg-desktop-portal + libei (pure Rust: `ashpd`
//! over `zbus`, `reis` for the EI protocol). Designed for GNOME ≥ 45
//! (Zorin OS 17/18, Ubuntu 24.04+); also works on other compositors that
//! implement these portals.
//!
//! **Capture** – `org.freedesktop.portal.InputCapture`: we place pointer
//! barriers on the screen edges that lead to other machines. When the
//! pointer hits one, the compositor *activates* the capture: the local
//! pointer stops and all pointer/keyboard events go to our EI receiver
//! context until we call `Release` (optionally with a cursor position).
//! GNOME 46 cannot re-enable a disabled capture session, so changing the
//! barriers recreates the session; barriers therefore cover every layout
//! edge and edges to offline machines are refused at activation time.
//!
//! **Local cursor while capturing** (cursor-park setting): GNOME 46 cannot
//! do "centre" or "hide", so this backend reports no cursor support and
//! the cursor stays at the edge. From mutter's gnome-46 source: barriers
//! are created `META_BARRIER_FLAG_STICKY`, and a hit sets the barrier
//! manager's `pointer_trap`, which pins the pointer to the hit position
//! for all motion (absolute EI/RemoteDesktop motion goes through the same
//! barrier constraint) until the barrier is released, which only
//! `Release` or closing the session does; activating the capture does not
//! touch cursor visibility (the event router only flips a flag), and no
//! portal call moves or hides the pointer while capture stays active. The
//! cursor is therefore visible, pinned at the edge. What works:
//! `Release` with `cursor_position` (honoured by mutter after it drops the
//! barriers, `clutter_seat_warp_pointer`), used to enter at the position
//! mapped from the other machine on the way back, and otherwise just
//! inside the edge where the capture started.
//!
//! **Injection** – `org.freedesktop.portal.RemoteDesktop` with
//! `ConnectToEIS` (EI sender context: absolute/relative pointer, buttons,
//! scroll, keyboard with evdev keycodes). The portal restore token is
//! stored (`portal-tokens.json`, persist mode "until revoked") so the
//! permission dialog appears only once. Without EIS (older portals) the
//! D-Bus `Notify*` methods are used.
//!
//! While a RemoteDesktop session exists GNOME shows its "someone is
//! controlling this computer" indicator, so the session is opened only on
//! demand: at start the backend only checks that the portal is there, and
//! the core opens and closes the session with
//! [`Backend::set_controllable`] (open while connected to the hub with
//! sharing on, see [`crate::controllable`]). Reopening uses the stored
//! restore token, so no dialog appears again; `caps().inject` is true only
//! while a session is open. The InputCapture session is not affected: on
//! GNOME 46 every new one asks for permission again, so it stays open.
//!
//! **Clipboard** – the portal Clipboard interface on the RemoteDesktop
//! session (xdg-desktop-portal ≥ 1.17, GNOME ≥ 45), so only while that
//! session is open (see [`super::portalclip`]). Content from another
//! machine that arrives while no session is open is put on the clipboard
//! when the next one opens. If a session opens without it (or the
//! RemoteDesktop portal is missing), the X11 CLIPBOARD selection through
//! XWayland is used from then on (mutter syncs it with the Wayland
//! clipboard), which needs XWayland to be running.

use super::portalclip::{self, ClipWork, PortalClip};
use super::x11clip::{self, X11Clipboard};
use super::{button_to_evdev, evdev_to_button};
use crate::backend::{Backend, BackendOptions, CaptureReport, EventRx, EventTx, LocalEvent};
use crate::config::{load_portal_token, save_portal_token};
use anyhow::{anyhow, Context, Result};
use ashpd::desktop::clipboard::Clipboard;
use ashpd::desktop::input_capture::{
    Activated, Barrier, BarrierID, BarrierPosition, Capabilities as IcCaps, CreateSessionOptions as IcCreate,
    Deactivated, Disabled, InputCapture, ReleaseOptions, Zones, ZonesChanged,
};
use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::{PersistMode, Session};
use flow_layout::{Edge, Side};
use flow_net::link::Action;
use flow_proto::{Capabilities, ClipContent, Monitor, Rect};
use futures_util::stream::LocalBoxStream;
use futures_util::StreamExt;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use std::collections::HashMap;
use std::future::Future;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, info, warn};

const RD_TOKEN: &str = "remote_desktop";
/// How long content from another machine is kept for a session that is
/// not open yet (opening takes a moment, a dialog maybe longer).
const REMOTE_CLIP_KEEP: Duration = Duration::from_secs(60);

enum InjCmd {
    Inject(Action),
    SetClipboard(ClipContent),
    /// [`RdShared::want`] changed: open or close the session. Queued
    /// behind the injections before it, so releases happen first.
    Sync,
}

/// The on-demand RemoteDesktop session, shared by the backend and the
/// injector thread.
struct RdShared {
    /// The core wants the session open.
    want: AtomicBool,
    /// A session is open (injection works).
    ready: AtomicBool,
    /// The open session has the portal clipboard.
    clip: AtomicBool,
    /// X11 clipboard through XWayland, when the portal clipboard is missing.
    x11clip: OnceLock<X11Clipboard>,
    x11_tried: AtomicBool,
    /// Content from another machine that arrived while no session with the
    /// portal clipboard was open, and when; set when the next one opens
    /// within [`REMOTE_CLIP_KEEP`].
    remote_clip: Mutex<Option<(Instant, ClipContent)>>,
    events: EventTx,
}

impl RdShared {
    fn new(events: EventTx) -> Self {
        RdShared {
            want: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            clip: AtomicBool::new(false),
            x11clip: OnceLock::new(),
            x11_tried: AtomicBool::new(false),
            remote_clip: Mutex::new(None),
            events,
        }
    }

    fn wanted(&self) -> bool {
        self.want.load(Ordering::SeqCst)
    }

    /// Ready to inject: open, and not on its way to closing.
    fn injecting(&self) -> bool {
        self.ready.load(Ordering::SeqCst) && self.wanted()
    }

    fn portal_clip(&self) -> bool {
        self.injecting() && self.clip.load(Ordering::SeqCst)
    }

    /// The open session lost its portal clipboard.
    fn lose_clip(&self) {
        if self.clip.swap(false, Ordering::SeqCst) {
            let _ = self.events.send(LocalEvent::CapsChanged);
        }
    }

    fn keep_remote_clip(&self, c: Option<ClipContent>) {
        *self.remote_clip.lock().expect("poisoned") = c.map(|c| (Instant::now(), c));
    }

    /// Remote content kept for the session that just opened, unless it is
    /// too old to still be what the user expects to paste.
    fn take_remote_clip(&self) -> Option<ClipContent> {
        let (at, c) = self.remote_clip.lock().expect("poisoned").take()?;
        if at.elapsed() > REMOTE_CLIP_KEEP {
            debug!("clipboard from another machine dropped: too old by the time the session opened");
            return None;
        }
        Some(c)
    }

    /// Records whether a session is open; the core re-reads the caps.
    fn set_ready(&self, ready: bool, clip: bool) {
        self.clip.store(clip, Ordering::SeqCst);
        if self.ready.swap(ready, Ordering::SeqCst) != ready {
            let _ = self.events.send(LocalEvent::CapsChanged);
        }
    }

    /// Starts the XWayland clipboard (once) for a session without the
    /// portal clipboard, as before sessions were opened on demand.
    fn x11_fallback(&self) {
        if self.x11_tried.swap(true, Ordering::SeqCst) {
            return;
        }
        if std::env::var_os("DISPLAY").is_none() {
            warn!("no clipboard access: portal clipboard missing and DISPLAY not set");
            return;
        }
        match x11clip::spawn(self.events.clone()) {
            Ok(c) => {
                info!("clipboard: portal clipboard unavailable, using the X11 selection via XWayland");
                let _ = self.x11clip.set(c);
                let _ = self.events.send(LocalEvent::CapsChanged);
            }
            Err(e) => warn!("no clipboard access (portal clipboard missing and XWayland not reachable): {e:#}"),
        }
    }
}

enum CapCmd {
    SetEdges(Vec<Edge>),
    Release(Option<(i32, i32)>),
}

pub struct WaylandBackend {
    monitors: Arc<Mutex<Vec<Monitor>>>,
    inj: Option<mpsc::UnboundedSender<InjCmd>>,
    rd: Arc<RdShared>,
    cap: Option<mpsc::UnboundedSender<CapCmd>>,
    capture_report: Arc<Mutex<CaptureReport>>,
}

pub async fn create(opts: &BackendOptions) -> Result<(Arc<dyn Backend>, EventRx)> {
    let (tx, rx) = mpsc::unbounded_channel();
    let monitors = Arc::new(Mutex::new(Vec::new()));

    // The two portal sessions are independent: refusing or losing one
    // leaves the other working.
    let rd = Arc::new(RdShared::new(tx.clone()));
    let inj = match start_injector(opts.state_dir.clone(), rd.clone()).await {
        Ok(s) => Some(s),
        Err(e) => {
            warn!("RemoteDesktop portal unavailable, this machine cannot be controlled: {e:#}");
            None
        }
    };
    let capture_report = Arc::new(Mutex::new(CaptureReport::default()));
    let cap = match start_capture(tx.clone(), monitors.clone(), capture_report.clone()).await {
        Ok(s) => Some(s),
        Err(e) if is_refusal(&e) => {
            warn!("{IC_REFUSED}");
            crate::problems::set(PROBLEM_IC, IC_REFUSED);
            None
        }
        Err(e) => {
            warn!(
                "InputCapture portal unavailable ({e:#}); this machine can be controlled but cannot control others. \
                 InputCapture needs GNOME ≥ 45 / xdg-desktop-portal ≥ 1.18; on older systems use the Xorg session \
                 (backend = \"x11\")."
            );
            None
        }
    };
    if inj.is_none() {
        rd.x11_fallback();
    }
    if monitors.lock().expect("poisoned").is_empty() {
        // No InputCapture zones: ask XWayland, else assume one 1920x1080 screen.
        let m = x11_monitors().unwrap_or_else(|_| {
            vec![Monitor { name: "default".into(), rect: Rect::new(0, 0, 1920, 1080), primary: true }]
        });
        *monitors.lock().expect("poisoned") = m;
    }
    if inj.is_none() && cap.is_none() {
        return Err(anyhow!("neither the RemoteDesktop nor the InputCapture portal works in this session"));
    }
    let b = WaylandBackend { monitors, inj, rd, cap, capture_report };
    info!(
        caps = ?b.caps(),
        remote_desktop = b.inj.is_some(),
        "Wayland portal backend ready (the remote control session opens while connected to the hub)"
    );
    Ok((Arc::new(b), rx))
}

fn x11_monitors() -> Result<Vec<Monitor>> {
    use x11rb::connection::Connection;
    use x11rb::protocol::randr::ConnectionExt;
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let r = conn.randr_get_monitors(root, true)?.reply()?;
    Ok(r.monitors
        .iter()
        .enumerate()
        .map(|(i, m)| Monitor {
            name: format!("xwayland-{i}"),
            rect: Rect::new(m.x as i32, m.y as i32, m.width as i32, m.height as i32),
            primary: m.primary,
        })
        .collect())
}

impl Backend for WaylandBackend {
    fn name(&self) -> &'static str {
        "wayland"
    }
    fn caps(&self) -> Capabilities {
        let inject = self.inj.is_some() && self.rd.injecting();
        Capabilities {
            capture: self.cap.is_some(),
            inject,
            clipboard: self.rd.portal_clip() || self.rd.x11clip.get().is_some(),
            // Neither moving nor hiding the cursor during capture (see above).
            cursor: Default::default(),
        }
    }
    fn monitors(&self) -> Vec<Monitor> {
        self.monitors.lock().expect("poisoned").clone()
    }
    fn set_edges(&self, edges: Vec<Edge>) {
        if let Some(c) = &self.cap {
            let _ = c.send(CapCmd::SetEdges(edges));
        }
    }
    fn begin_capture(&self) {
        // Capture is started by the compositor when a barrier is hit.
    }
    fn end_capture(&self, warp: Option<(i32, i32)>) {
        if let Some(c) = &self.cap {
            let _ = c.send(CapCmd::Release(warp));
        }
    }
    fn inject(&self, action: &Action) {
        if let Some(i) = &self.inj {
            let _ = i.send(InjCmd::Inject(*action));
        }
    }
    fn set_controllable(&self, on: bool) {
        if let Some(i) = &self.inj {
            self.rd.want.store(on, Ordering::SeqCst);
            let _ = i.send(InjCmd::Sync);
        }
    }
    fn session_support(&self) -> crate::controllable::SessionSupport {
        use crate::controllable::SessionSupport::*;
        if self.inj.is_some() {
            OnDemand
        } else {
            Unavailable
        }
    }
    fn keep_edges_stable(&self) -> bool {
        true
    }
    /// Barriers activate capture on contact; GNOME never says which keys
    /// were already held, so the core holds the pointer at the edge and
    /// watches the captured keys (see `crate::cross`).
    fn edge_hit_captures(&self) -> bool {
        self.cap.is_some()
    }
    fn capture_report(&self) -> Option<CaptureReport> {
        if self.cap.is_none() {
            let refused = crate::problems::list().iter().any(|p| p == IC_REFUSED);
            return Some(CaptureReport {
                state: if refused { "refused" } else { "unavailable" }.into(),
                ..Default::default()
            });
        }
        Some(self.capture_report.lock().expect("poisoned").clone())
    }
    fn set_clipboard(&self, content: &ClipContent) {
        if self.rd.portal_clip() {
            self.rd.keep_remote_clip(None);
            if let Some(i) = &self.inj {
                let _ = i.send(InjCmd::SetClipboard(content.clone()));
            }
        } else if let Some(c) = self.rd.x11clip.get() {
            c.set(content);
        } else if self.inj.is_some() {
            // Not open (yet), e.g. still opening after the hub connection
            // came back: set it once the session is there.
            info!("clipboard from another machine kept until the remote control session is open");
            self.rd.keep_remote_clip(Some(content.clone()));
        }
    }
}

fn now_us() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    t.tv_sec as u64 * 1_000_000 + t.tv_nsec as u64 / 1000
}

// ---------------------------------------------------------------------------
// injection: RemoteDesktop (+ clipboard on the same session)
// ---------------------------------------------------------------------------

/// Runs a future that is not `Send` (reis event streams hold callbacks) on
/// its own thread with a current-thread runtime. `setup` reports its
/// result through the returned receiver before the long-running part.
fn spawn_portal_thread<T, F, Fut>(name: &str, f: F) -> tokio::sync::oneshot::Receiver<Result<T>>
where
    T: Send + 'static,
    F: FnOnce(tokio::sync::oneshot::Sender<Result<T>>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new().name(name.into()).spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(Err(e.into()));
                return;
            }
        };
        rt.block_on(f(tx));
    });
    if let Err(e) = spawned {
        warn!("cannot spawn {name} thread: {e}");
    }
    rx
}

/// A private session bus connection for one portal session.
///
/// ashpd's `Portal::new()` constructors share one cached connection, and
/// zbus runs a connection's socket tasks on the tokio runtime that is
/// current when it is built. With one runtime per portal thread, the
/// connection would die with whichever thread happened to create it
/// first (e.g. the injector thread after the user cancels the
/// RemoteDesktop dialog) and leave the other session with a dead
/// connection. Each session therefore builds its own connection on its
/// own thread, which lives as long as the session can be re-created.
async fn session_bus() -> Result<ashpd::zbus::Connection> {
    ashpd::zbus::Connection::session().await.context("connecting to the D-Bus session bus")
}

/// Why an injection session ended.
#[derive(Debug, PartialEq)]
enum InjEnd {
    /// The backend was dropped.
    Quit,
    /// The EI connection or the portal session went away.
    Lost,
    /// Closed because the core no longer wants it ([`RdShared::want`]).
    Closed,
}

/// Starts the injector thread. It only checks that the RemoteDesktop
/// portal is there; sessions are opened when the core asks for them.
async fn start_injector(state_dir: PathBuf, rd: Arc<RdShared>) -> Result<mpsc::UnboundedSender<InjCmd>> {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
    let ready = spawn_portal_thread("portal-inject", move |ready| async move {
        let probe = probe_remote_desktop().await;
        let ok = probe.is_ok();
        let _ = ready.send(probe);
        if !ok {
            return;
        }
        let shared = rd.clone();
        keep_injecting(
            &rd,
            &mut cmd_rx,
            async || {
                let parts = setup_injector(&state_dir).await?;
                let clip = parts.2.is_some();
                if !clip {
                    shared.x11_fallback();
                }
                Ok((parts, clip))
            },
            async |(portal, session, clipboard, eis): InjParts, cmds: &mut mpsc::UnboundedReceiver<InjCmd>| match eis {
                Some((ctx, conn, events)) => {
                    run_ei_sender(ctx, conn, events, cmds, &shared, portal, session, clipboard).await
                }
                None => run_dbus_injector(&shared, portal, session, cmds).await,
            },
        )
        .await
    });
    ready.await.map_err(|_| anyhow!("portal thread died"))??;
    Ok(cmd_tx)
}

/// The RemoteDesktop portal answers (without creating a session, which
/// would show GNOME's indicator).
async fn probe_remote_desktop() -> Result<()> {
    let bus = session_bus().await?;
    let rd = bounded("RemoteDesktop portal", RemoteDesktop::with_connection(bus)).await?;
    let devices = bounded("RemoteDesktop.AvailableDeviceTypes", rd.available_device_types()).await?;
    debug!(?devices, version = rd.version(), "RemoteDesktop portal available");
    Ok(())
}

/// Runs injection sessions until the backend is dropped: a session is
/// opened (`setup`) while [`RdShared::want`] is set and closed when it is
/// cleared (`run` returns [`InjEnd::Closed`]); input for a closed session
/// is dropped. A session that is lost (EI disconnect, portal gone) or
/// fails to open is re-created with backoff while still wanted. A refused
/// dialog stops injection until the agent restarts (asking again and
/// again would pop the dialog up every time).
async fn keep_injecting<S>(
    rd: &RdShared,
    cmds: &mut mpsc::UnboundedReceiver<InjCmd>,
    mut setup: impl AsyncFnMut() -> Result<(S, bool)>,
    mut run: impl AsyncFnMut(S, &mut mpsc::UnboundedReceiver<InjCmd>) -> InjEnd,
) {
    let mut backoff = Backoff::default();
    loop {
        while !rd.wanted() {
            if cmds.recv().await.is_none() {
                return;
            }
        }
        let started = Instant::now();
        let end = match setup().await {
            Ok((session, clip)) => {
                // Queued while opening (perhaps behind a dialog): stale.
                if drop_stale(cmds) {
                    return;
                }
                crate::problems::clear(PROBLEM_RD);
                if rd.wanted() {
                    info!(portal_clipboard = clip, "remote control session open");
                    rd.set_ready(true, clip);
                }
                // Closes at once if it is no longer wanted.
                let end = run(session, cmds).await;
                rd.set_ready(false, false);
                end
            }
            Err(e) if is_refusal(&e) => {
                warn!("{RD_REFUSED}");
                crate::problems::set(PROBLEM_RD, RD_REFUSED);
                return;
            }
            Err(e) => {
                warn!("opening the remote control session failed: {e:#}");
                InjEnd::Lost
            }
        };
        match end {
            InjEnd::Quit => return,
            InjEnd::Closed => {
                info!("remote control session closed");
                backoff.reset();
                continue;
            }
            InjEnd::Lost => {}
        }
        if started.elapsed() > RETRY_MAX {
            backoff.reset();
        }
        let delay = backoff.next();
        warn!(?delay, "remote control session lost; re-creating it");
        // Input for this machine that arrives meanwhile would be stale by
        // the time a new session is up: drop it.
        let until = tokio::time::sleep(delay);
        tokio::pin!(until);
        loop {
            tokio::select! {
                _ = &mut until => break,
                cmd = cmds.recv() => match cmd {
                    None => return,
                    // No longer wanted: wait for the next request instead.
                    Some(_) if !rd.wanted() => break,
                    Some(_) => {}
                },
            }
        }
    }
}

/// Drops the queued commands; true when the backend is gone.
fn drop_stale(cmds: &mut mpsc::UnboundedReceiver<InjCmd>) -> bool {
    loop {
        match cmds.try_recv() {
            Ok(_) => {}
            Err(mpsc::error::TryRecvError::Empty) => return false,
            Err(mpsc::error::TryRecvError::Disconnected) => return true,
        }
    }
}

/// Keys of the refusals in [`crate::problems`].
const PROBLEM_RD: &str = "wayland-remote-desktop";
const PROBLEM_IC: &str = "wayland-input-capture";
const RD_REFUSED: &str = "remote control permission (RemoteDesktop portal) was refused in the GNOME dialog: other \
                          machines cannot control this one until flow-agent is restarted; controlling them from \
                          here still works";
const IC_REFUSED: &str = "input capture permission (InputCapture portal) was refused in the GNOME dialog: this \
                          machine cannot control other machines until flow-agent is restarted; it can still be \
                          controlled";

type EiParts = (ei::Context, reis::event::Connection, reis::tokio::EiConvertEventStream);
type InjParts = (RemoteDesktop, Session<RemoteDesktop>, Option<PortalClip>, Option<EiParts>);

async fn setup_injector(state_dir: &std::path::Path) -> Result<InjParts> {
    let bus = session_bus().await?;
    // The clipboard must use the same connection: the portal ties the
    // session to the caller's bus name.
    let rd = RemoteDesktop::with_connection(bus.clone()).await.context("RemoteDesktop portal")?;
    let session = rd.create_session(Default::default()).await.context("creating RemoteDesktop session")?;
    let token = load_portal_token(state_dir, RD_TOKEN);
    rd.select_devices(
        &session,
        SelectDevicesOptions::default()
            .set_devices(DeviceType::KEYBOARD | DeviceType::POINTER)
            .set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(token.as_deref()),
    )
    .await
    .context("selecting devices")?;
    // The clipboard must be requested before Start.
    let clipboard = match Clipboard::with_connection(bus).await {
        Ok(c) => match c.request(&session, Default::default()).await {
            Ok(()) => Some(c),
            Err(e) => {
                debug!("portal clipboard not available: {e}");
                None
            }
        },
        Err(e) => {
            debug!("portal clipboard not available: {e}");
            None
        }
    };
    info!("asking for remote control permission (a GNOME dialog appears the first time)");
    let resp =
        rd.start(&session, None, Default::default()).await?.response().context("RemoteDesktop permission denied")?;
    if let Some(t) = resp.restore_token() {
        if let Err(e) = save_portal_token(state_dir, RD_TOKEN, t) {
            warn!("could not store the portal restore token: {e:#}");
        }
    }
    let clipboard = match clipboard {
        Some(c) if resp.is_clipboard_enabled() => match portalclip::session_path(&session) {
            Ok(path) => match PortalClip::subscribe(c, path).await {
                Ok(c) => {
                    info!("clipboard via the portal Clipboard interface");
                    Some(c)
                }
                Err(e) => {
                    warn!("portal clipboard unavailable: {e:#}");
                    None
                }
            },
            Err(e) => {
                warn!("portal clipboard unavailable: {e:#}");
                None
            }
        },
        Some(_) => {
            info!("the portal did not enable the clipboard for this session");
            None
        }
        None => None,
    };
    let eis = match rd.connect_to_eis(&session, Default::default()).await {
        Ok(fd) => {
            let stream = UnixStream::from(fd);
            stream.set_nonblocking(true)?;
            let ctx = ei::Context::new(stream)?;
            let (conn, events) = ctx
                .handshake_tokio("flowcursor-inject", ei::handshake::ContextType::Sender)
                .await
                .map_err(|e| anyhow!("EI handshake: {e:?}"))?;
            info!("injection via libei (RemoteDesktop.ConnectToEIS)");
            Some((ctx, conn, events))
        }
        Err(e) => {
            warn!("ConnectToEIS not supported ({e}); falling back to RemoteDesktop D-Bus Notify* calls");
            None
        }
    };
    Ok((rd, session, clipboard, eis))
}

#[derive(Default)]
struct EiDevices {
    list: Vec<reis::event::Device>,
    resumed: HashMap<u64, bool>,
}

fn dev_key(d: &reis::event::Device) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    d.device().hash(&mut h);
    h.finish()
}

impl EiDevices {
    fn with<T: ei::Interface>(&self) -> Option<(&reis::event::Device, T)> {
        self.list
            .iter()
            .filter(|d| self.resumed.get(&dev_key(d)).copied().unwrap_or(false))
            .find_map(|d| d.interface::<T>().map(|i| (d, i)))
    }
}

/// After the last injections before a close, how long the compositor gets
/// to read them from the EI socket before the session is closed over D-Bus
/// (a different channel), so the releases are applied first.
const CLOSE_SETTLE: Duration = Duration::from_millis(100);

#[allow(clippy::too_many_arguments)]
async fn run_ei_sender(
    ctx: ei::Context,
    conn: reis::event::Connection,
    mut events: reis::tokio::EiConvertEventStream,
    cmds: &mut mpsc::UnboundedReceiver<InjCmd>,
    rd: &RdShared,
    _portal: RemoteDesktop,
    session: Session<RemoteDesktop>,
    mut clip: Option<PortalClip>,
) -> InjEnd {
    let tx = rd.events.clone();
    if !rd.wanted() {
        let _ = bounded("Session.Close", session.close()).await;
        return InjEnd::Closed;
    }
    let mut devs = EiDevices::default();
    let mut sequence = 1u32;
    let mut last_abs: Option<(f32, f32)> = None;
    if let Some(c) = clip.as_mut() {
        if let Some(content) = rd.take_remote_clip() {
            info!("setting the clipboard from another machine that arrived before the session was open");
            if let Err(e) = c.set(content).await {
                warn!("setting the clipboard failed: {e:#}");
            }
        }
    }
    let end = loop {
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(ev)) => match ev {
                    EiEvent::SeatAdded(s) => {
                        s.seat.bind_capabilities(
                            DeviceCapability::Pointer | DeviceCapability::PointerAbsolute | DeviceCapability::Keyboard
                                | DeviceCapability::Scroll | DeviceCapability::Button,
                        );
                        let _ = ctx.flush();
                    }
                    EiEvent::DeviceAdded(d) => {
                        debug!(name = ?d.device.name(), "EI device added");
                        devs.list.push(d.device);
                    }
                    EiEvent::DeviceRemoved(d) => {
                        let k = dev_key(&d.device);
                        devs.list.retain(|x| dev_key(x) != k);
                        devs.resumed.remove(&k);
                    }
                    EiEvent::DeviceResumed(r) => {
                        devs.resumed.insert(dev_key(&r.device), true);
                        r.device.device().start_emulating(r.serial, sequence);
                        sequence += 1;
                        let _ = ctx.flush();
                    }
                    EiEvent::DevicePaused(p) => {
                        devs.resumed.insert(dev_key(&p.device), false);
                    }
                    EiEvent::Disconnected(d) => {
                        warn!("EI injection connection closed: {:?}", d.reason);
                        break InjEnd::Lost;
                    }
                    _ => {}
                },
                Some(Err(e)) => {
                    warn!("EI injection error: {e:?}");
                    break InjEnd::Lost;
                }
                None => {
                    warn!("EI injection connection closed");
                    break InjEnd::Lost;
                }
            },
            cmd = cmds.recv() => match cmd {
                None => break InjEnd::Quit,
                Some(InjCmd::Sync) => if !rd.wanted() {
                    let _ = ctx.flush();
                    tokio::time::sleep(CLOSE_SETTLE).await;
                    break InjEnd::Closed;
                },
                Some(InjCmd::Inject(a)) => {
                    ei_inject(&devs, &conn, a, &mut last_abs);
                    let _ = ctx.flush();
                }
                Some(InjCmd::SetClipboard(c)) => {
                    if let Some(cs) = clip.as_mut() {
                        if let Err(e) = cs.set(c).await {
                            warn!("setting the clipboard failed: {e:#}");
                        }
                    }
                }
            },
            work = next_clip(&mut clip) => match work {
                ClipWork::Read(mime) => {
                    if let Some(cs) = &clip {
                        if let Err(e) = cs.read(&mime, &tx).await {
                            warn!("clipboard read failed: {e:#}");
                        }
                    }
                }
                ClipWork::Transfer { mime, serial } => {
                    if let Some(cs) = &clip {
                        cs.transfer(&mime, serial).await;
                    }
                }
                ClipWork::Ended(what) => {
                    warn!("the portal clipboard signal {what} ended; no clipboard until the session is reopened");
                    clip = None;
                    rd.lose_clip();
                }
            },
        }
    };
    let _ = bounded("Session.Close", session.close()).await;
    end
}

/// Next work of the session's clipboard; never resolves without one.
async fn next_clip(c: &mut Option<PortalClip>) -> ClipWork {
    match c {
        Some(c) => c.next().await,
        None => std::future::pending().await,
    }
}

fn ei_inject(devs: &EiDevices, conn: &reis::event::Connection, a: Action, last_abs: &mut Option<(f32, f32)>) {
    let serial = conn.serial();
    let t = now_us();
    match a {
        Action::Entered { x, y } | Action::MoveAbs { x, y, .. } => {
            let (x, y) = (x as f32, y as f32);
            if let Some((d, p)) = devs.with::<ei::PointerAbsolute>() {
                p.motion_absolute(x, y);
                d.device().frame(serial, t);
            } else if let Some((d, p)) = devs.with::<ei::Pointer>() {
                // Relative only: move by the difference to the last position
                // (or by the sender's delta when we do not know it yet).
                let (dx, dy) = match (*last_abs, a) {
                    (Some((lx, ly)), _) => (x - lx, y - ly),
                    (None, Action::MoveAbs { dx, dy, .. }) => (dx as f32, dy as f32),
                    _ => (0.0, 0.0),
                };
                p.motion_relative(dx, dy);
                d.device().frame(serial, t);
            }
            *last_abs = Some((x, y));
        }
        Action::Key { usage, down } => {
            if let (Some(code), Some((d, k))) = (flow_keymap::hid_to_evdev(usage), devs.with::<ei::Keyboard>()) {
                k.key(code as u32, if down { ei::keyboard::KeyState::Press } else { ei::keyboard::KeyState::Released });
                d.device().frame(serial, t);
            }
        }
        Action::Button { button, down } => {
            if let Some((d, b)) = devs.with::<ei::Button>() {
                b.button(
                    button_to_evdev(button),
                    if down { ei::button::ButtonState::Press } else { ei::button::ButtonState::Released },
                );
                d.device().frame(serial, t);
            }
        }
        Action::Wheel { dx, dy } => {
            if let Some((d, s)) = devs.with::<ei::Scroll>() {
                // EI: 120 = one detent, positive y = down; ours: positive = up.
                s.scroll_discrete(dx, -dy);
                d.device().frame(serial, t);
            }
        }
        Action::Left => *last_abs = None,
    }
}

/// Injection through the RemoteDesktop `Notify*` D-Bus calls, for portals
/// without ConnectToEIS. Runs until the backend is dropped or the session
/// is no longer wanted (the calls are awaited in order, so everything
/// before the close is applied).
async fn run_dbus_injector(
    shared: &RdShared,
    rd: RemoteDesktop,
    session: Session<RemoteDesktop>,
    cmds: &mut mpsc::UnboundedReceiver<InjCmd>,
) -> InjEnd {
    // Without a screencast stream only relative motion is available.
    let mut last: Option<(i32, i32)> = None;
    if !shared.wanted() {
        let _ = bounded("Session.Close", session.close()).await;
        return InjEnd::Closed;
    }
    while let Some(c) = cmds.recv().await {
        let r = match c {
            InjCmd::Sync if !shared.wanted() => {
                let _ = bounded("Session.Close", session.close()).await;
                return InjEnd::Closed;
            }
            InjCmd::Sync => Ok(()),
            InjCmd::Inject(Action::Entered { x, y }) => {
                last = Some((x, y));
                Ok(())
            }
            InjCmd::Inject(Action::MoveAbs { x, y, dx, dy }) => {
                let (mx, my) = match last {
                    Some((lx, ly)) => (x - lx, y - ly),
                    None => (dx, dy),
                };
                last = Some((x, y));
                rd.notify_pointer_motion(&session, mx as f64, my as f64, Default::default()).await
            }
            InjCmd::Inject(Action::Key { usage, down }) => match flow_keymap::hid_to_evdev(usage) {
                Some(code) => {
                    let st = if down { KeyState::Pressed } else { KeyState::Released };
                    rd.notify_keyboard_keycode(&session, code as i32, st, Default::default()).await
                }
                None => Ok(()),
            },
            InjCmd::Inject(Action::Button { button, down }) => {
                let st = if down { KeyState::Pressed } else { KeyState::Released };
                rd.notify_pointer_button(&session, button_to_evdev(button) as i32, st, Default::default()).await
            }
            InjCmd::Inject(Action::Wheel { dx, dy }) => {
                if dy != 0 {
                    let _ =
                        rd.notify_pointer_axis_discrete(&session, Axis::Vertical, -dy / 120, Default::default()).await;
                }
                if dx != 0 {
                    let _ =
                        rd.notify_pointer_axis_discrete(&session, Axis::Horizontal, dx / 120, Default::default()).await;
                }
                Ok(())
            }
            InjCmd::Inject(Action::Left) => {
                last = None;
                Ok(())
            }
            InjCmd::SetClipboard(_) => Ok(()),
        };
        if let Err(e) = r {
            debug!("RemoteDesktop notify failed: {e}");
        }
    }
    InjEnd::Quit
}

// ---------------------------------------------------------------------------
// capture: InputCapture
// ---------------------------------------------------------------------------

/// First delay before re-creating a portal session that ended; doubled on
/// every failed attempt up to [`RETRY_MAX`].
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(60);
/// Upper bound for portal calls that never show a dialog (barriers, zones,
/// enable/disable, release, close): a portal that does not answer must not
/// park the capture loop.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

async fn bounded<T, E: Into<anyhow::Error>>(what: &str, f: impl Future<Output = Result<T, E>>) -> Result<T> {
    match tokio::time::timeout(CALL_TIMEOUT, f).await {
        Ok(r) => r.map_err(Into::into),
        Err(_) => Err(anyhow!("{what}: no answer from the portal within {CALL_TIMEOUT:?}")),
    }
}

/// Exponential backoff for re-creating portal sessions.
struct Backoff(Duration);

impl Default for Backoff {
    fn default() -> Self {
        Backoff(RETRY_MIN)
    }
}

impl Backoff {
    fn next(&mut self) -> Duration {
        let d = self.0;
        self.0 = (d * 2).min(RETRY_MAX);
        d
    }

    fn reset(&mut self) {
        self.0 = RETRY_MIN;
    }
}

/// True when a portal request failed because the user refused (cancelled)
/// the permission dialog. Such sessions are not re-requested automatically:
/// the dialog would pop up again and again.
fn is_refusal(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(
            c.downcast_ref::<ashpd::Error>(),
            Some(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled))
                | Some(ashpd::Error::Portal(ashpd::PortalError::Cancelled(_)))
        )
    })
}

/// What happened on an open capture session.
#[derive(Debug)]
enum CapEvent {
    /// A pointer barrier fired; capture is active until released.
    Activated {
        activation_id: Option<u32>,
        barrier: Option<u32>,
        pos: (f64, f64),
    },
    Deactivated,
    /// The compositor disabled the session (GNOME 46 cannot re-enable it).
    Disabled,
    /// The monitor configuration changed. mutter (gnome-46
    /// `on_monitors_changed`) then disables the session without a
    /// `Disabled` signal: its barriers and EI devices are gone.
    ZonesChanged,
    /// Captured input, already converted.
    Input(LocalEvent),
}

/// Barriers the compositor accepted, by our barrier id.
#[derive(Debug, Default)]
struct BarrierSet {
    ids: HashMap<u32, Side>,
    refused: usize,
    /// The zone set they were set for.
    zone_set: u32,
}

/// The InputCapture operations the capture loop needs. [`PortalCapture`]
/// talks to the real portal; tests drive [`run_capture`] with a fake.
trait CapturePortal {
    /// Creates a new session (asks for permission when the portal wants to)
    /// and connects its EI stream; returns the zones as monitors. Closes the
    /// previous session first, if any.
    async fn open(&mut self) -> Result<Vec<Monitor>>;
    /// Disables and closes the current session, if any.
    async fn close(&mut self);
    /// Sets pointer barriers for `edges` on the current zone set.
    async fn set_barriers(&mut self, edges: &[Edge]) -> Result<BarrierSet>;
    async fn enable(&mut self) -> Result<()>;
    async fn release(&mut self, activation_id: Option<u32>, pos: Option<(f64, f64)>) -> Result<()>;
    /// Reads the zones again: (zone set, monitors).
    async fn zones(&mut self) -> Result<(u32, Vec<Monitor>)>;
    /// Next event of the open session, `None` once the session is gone (EI
    /// stream closed or failed, portal signals ended). Must be cancel-safe.
    async fn next_event(&mut self) -> Option<CapEvent>;
}

async fn start_capture(
    tx: EventTx,
    monitors: Arc<Mutex<Vec<Monitor>>>,
    report: Arc<Mutex<CaptureReport>>,
) -> Result<mpsc::UnboundedSender<CapCmd>> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let ready = spawn_portal_thread("portal-capture", move |ready| async move {
        let mut portal = PortalCapture::default();
        match portal.open().await {
            Ok(m) => {
                *monitors.lock().expect("poisoned") = m;
                let _ = ready.send(Ok(()));
                run_capture(portal, cmd_rx, tx, monitors, report).await;
            }
            Err(e) => {
                let _ = ready.send(Err(e));
            }
        }
    });
    ready.await.map_err(|_| anyhow!("portal thread died"))??;
    Ok(cmd_tx)
}

/// Portal objects of one live InputCapture session.
struct LiveCapture {
    ic: InputCapture,
    session: Session<InputCapture>,
    ctx: ei::Context,
    events: reis::tokio::EiConvertEventStream,
    zones: Zones,
    activated: LocalBoxStream<'static, Activated>,
    deactivated: LocalBoxStream<'static, Deactivated>,
    disabled: LocalBoxStream<'static, Disabled>,
    zones_changed: LocalBoxStream<'static, ZonesChanged>,
}

/// [`CapturePortal`] on xdg-desktop-portal.
#[derive(Default)]
struct PortalCapture {
    live: Option<LiveCapture>,
    scroll_acc: (f32, f32),
}

/// Subscribes to a portal signal. ashpd's `receive_*` streams borrow the
/// portal proxy, which would make [`LiveCapture`] self-referential; the
/// zbus signal stream of the (owned, `'static`) proxy does not.
async fn portal_signal<T>(proxy: &ashpd::zbus::Proxy<'static>, name: &'static str) -> Result<LocalBoxStream<'static, T>>
where
    T: for<'de> serde::Deserialize<'de> + ashpd::wire::Type + 'static,
{
    let s = proxy.receive_signal(name).await.with_context(|| format!("subscribing to InputCapture.{name}"))?;
    Ok(s.filter_map(move |m| {
        let r = m.body().deserialize::<T>();
        if let Err(e) = &r {
            warn!("unexpected InputCapture.{name} signal: {e}");
        }
        std::future::ready(r.ok())
    })
    .boxed_local())
}

impl PortalCapture {
    fn live(&mut self) -> Result<&mut LiveCapture> {
        self.live.as_mut().ok_or_else(|| anyhow!("no InputCapture session"))
    }
}

impl CapturePortal for PortalCapture {
    async fn open(&mut self) -> Result<Vec<Monitor>> {
        self.close().await;
        // A new connection per session, see `session_bus`.
        let ic = InputCapture::with_connection(session_bus().await?).await.context("InputCapture portal")?;
        // Subscribe before creating the session so no signal is missed.
        let activated = portal_signal(&ic, "Activated").await?;
        let deactivated = portal_signal(&ic, "Deactivated").await?;
        let disabled = portal_signal(&ic, "Disabled").await?;
        let zones_changed = portal_signal(&ic, "ZonesChanged").await?;
        info!("asking for input capture permission");
        let (session, caps) = ic
            .create_session(None, IcCreate::default().set_capabilities(IcCaps::KEYBOARD | IcCaps::POINTER))
            .await
            .context("creating InputCapture session")?;
        debug!(?caps, "InputCapture session");
        let fd = bounded("InputCapture.ConnectToEIS", ic.connect_to_eis(&session, Default::default())).await?;
        let stream = UnixStream::from(fd);
        stream.set_nonblocking(true)?;
        let ctx = ei::Context::new(stream)?;
        let (_conn, events) = ctx
            .handshake_tokio("flowcursor-capture", ei::handshake::ContextType::Receiver)
            .await
            .map_err(|e| anyhow!("EI handshake: {e:?}"))?;
        let zones = bounded("InputCapture.GetZones", ic.zones(&session, Default::default())).await?.response()?;
        let mons = zones_to_monitors(&zones);
        info!(zone_set = zones.zone_set(), zones = mons.len(), "input capture session created");
        self.scroll_acc = (0.0, 0.0);
        self.live =
            Some(LiveCapture { ic, session, ctx, events, zones, activated, deactivated, disabled, zones_changed });
        Ok(mons)
    }

    async fn close(&mut self) {
        if let Some(l) = self.live.take() {
            let _ = bounded("InputCapture.Disable", l.ic.disable(&l.session, Default::default())).await;
            let _ = bounded("Session.Close", l.session.close()).await;
            info!("input capture session disabled and closed");
        }
    }

    async fn set_barriers(&mut self, edges: &[Edge]) -> Result<BarrierSet> {
        let l = self.live()?;
        let mut ids = HashMap::new();
        let barriers: Vec<Barrier> = edges
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let id = BarrierID::new(i as u32 + 1)?;
                ids.insert(i as u32 + 1, e.side);
                Some(Barrier::new(id, barrier_for(e)))
            })
            .collect();
        let zone_set = l.zones.zone_set();
        let resp = bounded(
            "InputCapture.SetPointerBarriers",
            l.ic.set_pointer_barriers(&l.session, &barriers, zone_set, Default::default()),
        )
        .await?
        .response()?;
        let failed = resp.failed_barriers();
        if !failed.is_empty() {
            warn!(?failed, "the compositor refused some pointer barriers (they must lie on outer screen edges)");
        }
        for f in failed {
            ids.remove(&f.get());
        }
        info!(barriers = ids.len(), zone_set, "pointer barriers set");
        Ok(BarrierSet { refused: barriers.len() - ids.len(), ids, zone_set })
    }

    async fn enable(&mut self) -> Result<()> {
        let l = self.live()?;
        bounded("InputCapture.Enable", l.ic.enable(&l.session, Default::default())).await
    }

    async fn release(&mut self, activation_id: Option<u32>, pos: Option<(f64, f64)>) -> Result<()> {
        let l = self.live()?;
        let opts = ReleaseOptions::default().set_activation_id(activation_id).set_cursor_position(pos);
        bounded("InputCapture.Release", l.ic.release(&l.session, opts)).await
    }

    async fn zones(&mut self) -> Result<(u32, Vec<Monitor>)> {
        let l = self.live()?;
        l.zones = bounded("InputCapture.GetZones", l.ic.zones(&l.session, Default::default())).await?.response()?;
        Ok((l.zones.zone_set(), zones_to_monitors(&l.zones)))
    }

    async fn next_event(&mut self) -> Option<CapEvent> {
        let l = self.live.as_mut()?;
        loop {
            tokio::select! {
                a = l.activated.next() => {
                    let a = a?;
                    let barrier = a.barrier_id().and_then(|b| match b {
                        ashpd::desktop::input_capture::ActivatedBarrier::Barrier(id) => Some(id.get()),
                        _ => None,
                    });
                    let pos = a.cursor_position().map(|(x, y)| (x as f64, y as f64)).unwrap_or((0.0, 0.0));
                    self.scroll_acc = (0.0, 0.0);
                    return Some(CapEvent::Activated { activation_id: a.activation_id(), barrier, pos });
                }
                d = l.deactivated.next() => { d?; return Some(CapEvent::Deactivated); }
                d = l.disabled.next() => { d?; return Some(CapEvent::Disabled); }
                z = l.zones_changed.next() => { z?; return Some(CapEvent::ZonesChanged); }
                ev = l.events.next() => match ev? {
                    Ok(EiEvent::SeatAdded(s)) => {
                        s.seat.bind_capabilities(
                            DeviceCapability::Pointer | DeviceCapability::PointerAbsolute | DeviceCapability::Keyboard
                                | DeviceCapability::Scroll | DeviceCapability::Button,
                        );
                        let _ = l.ctx.flush();
                    }
                    Ok(ev) => {
                        if let Some(le) = ei_to_local(&ev, &mut self.scroll_acc) {
                            return Some(CapEvent::Input(le));
                        }
                    }
                    Err(e) => {
                        warn!("EI capture stream error: {e:?}");
                        return None;
                    }
                },
            }
        }
    }
}

fn zones_to_monitors(z: &Zones) -> Vec<Monitor> {
    z.regions()
        .iter()
        .enumerate()
        .map(|(i, r)| Monitor {
            name: format!("zone-{i}"),
            rect: Rect::new(r.x_offset(), r.y_offset(), r.width() as i32, r.height() as i32),
            primary: i == 0,
        })
        .collect()
}

fn barrier_for(e: &Edge) -> BarrierPosition {
    let (a, b) = (e.start, e.end - 1);
    match e.side {
        Side::Left | Side::Right => BarrierPosition::new(e.pos, a, e.pos, b),
        Side::Top | Side::Bottom => BarrierPosition::new(a, e.pos, b, e.pos),
    }
}

/// Position just inside the screen next to a barrier hit at (x, y).
fn inside(side: Side, x: f64, y: f64) -> (f64, f64) {
    match side {
        Side::Right => (x - 2.0, y),
        Side::Left => (x + 2.0, y),
        Side::Bottom => (x, y - 2.0),
        Side::Top => (x, y + 2.0),
    }
}

/// After a monitor change, how long the core gets to send the edges of the
/// new layout before the session is re-created with the edges it has: one
/// new session (and permission dialog) instead of two.
const ZONES_SETTLE: Duration = Duration::from_secs(2);
/// How often an enabled session is checked against the compositor's zone
/// set (a changed set means its barriers are gone).
const HEALTH_CHECK: Duration = Duration::from_secs(30);

/// State of the capture loop around one [`CapturePortal`].
struct Capture<P> {
    portal: P,
    cmds: mpsc::UnboundedReceiver<CapCmd>,
    tx: EventTx,
    monitors: Arc<Mutex<Vec<Monitor>>>,
    edges: Vec<Edge>,
    /// Barriers of the current session (empty without one).
    barriers: BarrierSet,
    /// (session, zone set) the enabled barriers belong to.
    applied: Option<(u32, u32)>,
    /// The portal has a live session.
    open: bool,
    enabled: bool,
    /// The compositor disabled the session (a `Disabled` signal, or a
    /// monitor change, after which GNOME 46 drops barriers and EI devices
    /// and cannot re-enable): a new session is needed.
    stale: bool,
    /// The dialog was refused: nothing is re-created until a restart.
    refused: bool,
    /// (activation id, side, cursor position) while captured.
    active: Option<(Option<u32>, Side, (f64, f64))>,
    released_by_us: bool,
    backoff: Backoff,
    /// When to re-create a lost session.
    retry_at: Option<Instant>,
    /// When to re-create a stale session (see [`ZONES_SETTLE`]).
    heal_at: Option<Instant>,
    next_check: Instant,
    /// Sessions created so far; the current one is number `sessions`.
    sessions: u32,
    /// When the current session was created; a session that lived longer
    /// than [`RETRY_MAX`] resets the backoff when it is lost.
    opened_at: Instant,
    last_activation: Option<Instant>,
    last_change: Option<(String, Instant)>,
    report: Arc<Mutex<CaptureReport>>,
}

/// Runs the capture loop until the backend is dropped. The portal must have
/// an open session. When the session dies (EI stream closed, portal gone)
/// an active capture ends with [`LocalEvent::CaptureEnded`] and the session
/// is re-created with backoff; commands keep being processed meanwhile.
async fn run_capture<P: CapturePortal>(
    portal: P,
    cmds: mpsc::UnboundedReceiver<CapCmd>,
    tx: EventTx,
    monitors: Arc<Mutex<Vec<Monitor>>>,
    report: Arc<Mutex<CaptureReport>>,
) {
    let mut c = Capture {
        portal,
        cmds,
        tx,
        monitors,
        edges: Vec::new(),
        barriers: BarrierSet::default(),
        applied: None,
        open: true,
        enabled: false,
        stale: false,
        refused: false,
        active: None,
        released_by_us: false,
        backoff: Backoff::default(),
        retry_at: None,
        heal_at: None,
        next_check: Instant::now() + HEALTH_CHECK,
        sessions: 1,
        opened_at: Instant::now(),
        last_activation: None,
        last_change: None,
        report,
    };
    c.publish();
    loop {
        let retry_at = c.retry_at;
        let heal_at = c.heal_at;
        tokio::select! {
            cmd = c.cmds.recv() => match cmd {
                None => {
                    c.portal.close().await;
                    return;
                }
                Some(CapCmd::SetEdges(new)) => c.set_edges(new).await,
                Some(CapCmd::Release(warp)) => c.release(warp).await,
            },
            ev = c.portal.next_event(), if c.open => match ev {
                Some(ev) => c.on_event(ev).await,
                None => c.lost("the EI capture stream closed").await,
            },
            _ = tokio::time::sleep_until(retry_at.unwrap_or_else(Instant::now)), if retry_at.is_some() => {
                c.retry_at = None;
                c.reopen().await;
            }
            _ = tokio::time::sleep_until(heal_at.unwrap_or_else(Instant::now)), if heal_at.is_some() => {
                c.heal_at = None;
                c.heal().await;
            }
            _ = tokio::time::sleep_until(c.next_check) => {
                c.check().await;
            }
        }
        c.publish();
    }
}

impl<P: CapturePortal> Capture<P> {
    fn changed(&mut self, what: impl Into<String>) {
        self.last_change = Some((what.into(), Instant::now()));
    }

    /// Updates what [`Backend::capture_report`] returns.
    fn publish(&self) {
        let state = if self.refused {
            "refused"
        } else if !self.open {
            "lost"
        } else if self.stale {
            "re-creating"
        } else if self.enabled {
            "enabled"
        } else if self.edges.is_empty() {
            "no edges"
        } else {
            "not enabled"
        };
        self.publish_state(state);
    }

    fn publish_state(&self, state: &str) {
        *self.report.lock().expect("poisoned") = CaptureReport {
            state: state.into(),
            barriers: if self.enabled { self.barriers.ids.len() } else { 0 },
            barriers_refused: self.barriers.refused,
            zone_set: self.applied.map(|a| a.1),
            sessions: self.sessions,
            session_age: self.open.then(|| self.opened_at.elapsed()),
            since_activation: self.last_activation.map(|t| t.elapsed()),
            last_change: self.last_change.as_ref().map(|c| c.0.clone()),
            since_change: self.last_change.as_ref().map(|c| c.1.elapsed()),
        };
    }

    async fn set_edges(&mut self, new: Vec<Edge>) {
        if new == self.edges && self.enabled {
            // The same edges again (e.g. the hub connection came back): a
            // cheap check that the barriers are still in place.
            self.check().await;
            return;
        }
        self.edges = new;
        if !self.open {
            // Applied when the session is back.
            return;
        }
        if self.enabled || self.stale {
            // GNOME 46 cannot re-enable a disabled session: start a new one.
            // Closing the old one ends a capture in progress.
            if !self.recreate("the edges changed").await {
                return;
            }
        }
        self.apply_edges().await;
    }

    /// Replaces the session with a new one (GNOME asks for permission
    /// again); false when that failed (a retry is scheduled unless the
    /// dialog was refused).
    async fn recreate(&mut self, why: &str) -> bool {
        self.end_active();
        self.heal_at = None;
        info!("re-creating the input capture session ({why}); GNOME asks for permission again");
        self.publish_state("opening");
        match self.portal.open().await {
            Ok(m) => {
                self.session_opened(m);
                true
            }
            Err(e) => {
                self.open = false;
                self.enabled = false;
                self.applied = None;
                self.barriers = BarrierSet::default();
                self.open_failed(e, "re-creating the capture session failed");
                false
            }
        }
    }

    fn session_opened(&mut self, m: Vec<Monitor>) {
        self.open = true;
        self.enabled = false;
        self.stale = false;
        self.applied = None;
        self.barriers = BarrierSet::default();
        self.sessions += 1;
        self.opened_at = Instant::now();
        self.next_check = Instant::now() + HEALTH_CHECK;
        crate::problems::clear(PROBLEM_IC);
        self.changed(format!("session {} created", self.sessions));
        self.take_queued_edges();
        let changed = *self.monitors.lock().expect("poisoned") != m;
        *self.monitors.lock().expect("poisoned") = m.clone();
        if changed {
            let _ = self.tx.send(LocalEvent::MonitorsChanged(m));
        }
    }

    /// Opening a session may wait for a permission dialog; edge updates
    /// queued meanwhile are applied to the new session right away instead of
    /// re-creating it (and asking again) once per update.
    fn take_queued_edges(&mut self) {
        while let Ok(cmd) = self.cmds.try_recv() {
            match cmd {
                CapCmd::SetEdges(e) => self.edges = e,
                // Nothing is captured while a session is being created.
                CapCmd::Release(_) => {}
            }
        }
    }

    /// Sets the barriers for the current edges and enables capture.
    async fn apply_edges(&mut self) {
        self.enabled = false;
        self.applied = None;
        if self.edges.is_empty() {
            info!("no edges lead to another machine: input capture not enabled");
            self.barriers = BarrierSet::default();
            self.changed("no edges");
            return;
        }
        // Barriers must name the compositor's current zone set.
        if let Err(e) = self.refresh_zones().await {
            warn!("reading the capture zones failed: {e:#}");
        }
        match self.portal.set_barriers(&self.edges).await {
            Ok(b) => self.barriers = b,
            Err(e) => {
                warn!("setting pointer barriers failed: {e:#}");
                self.changed(format!("setting barriers failed: {e:#}"));
                return;
            }
        }
        match self.portal.enable().await {
            Ok(()) => {
                self.enabled = true;
                self.applied = Some((self.sessions, self.barriers.zone_set));
                info!(
                    barriers = self.barriers.ids.len(),
                    zone_set = self.barriers.zone_set,
                    session = self.sessions,
                    "input capture enabled"
                );
                let n = self.barriers.ids.len();
                self.changed(format!("enabled with {n} barrier{}", if n == 1 { "" } else { "s" }));
            }
            Err(e) => {
                warn!("enabling input capture failed: {e:#}");
                self.changed(format!("enabling failed: {e:#}"));
            }
        }
    }

    async fn release(&mut self, warp: Option<(i32, i32)>) {
        if let Some((aid, side, (x, y))) = self.active.take() {
            let pos = warp.map(|(x, y)| (x as f64, y as f64)).unwrap_or_else(|| inside(side, x, y));
            self.released_by_us = true;
            if let Err(e) = self.portal.release(aid, Some(pos)).await {
                warn!("releasing input capture failed: {e:#}");
            }
        }
    }

    async fn on_event(&mut self, ev: CapEvent) {
        match ev {
            CapEvent::Activated { activation_id, barrier, pos: (x, y) } => {
                self.last_activation = Some(Instant::now());
                let Some(side) = barrier.and_then(|b| self.barriers.ids.get(&b).copied()) else {
                    warn!(?barrier, ?activation_id, "input capture activated without one of our barriers; releasing");
                    let _ = self.portal.release(activation_id, None).await;
                    return;
                };
                debug!(?barrier, ?side, x, y, "pointer barrier hit");
                self.active = Some((activation_id, side, (x, y)));
                self.released_by_us = false;
                let _ = self.tx.send(LocalEvent::EdgeHit { side, x: x.round() as i32, y: y.round() as i32 });
            }
            CapEvent::Deactivated => {
                if self.active.take().is_some() && !self.released_by_us {
                    let _ = self.tx.send(LocalEvent::CaptureEnded);
                }
            }
            CapEvent::Disabled => {
                warn!("input capture was disabled by the compositor; re-creating the session");
                self.changed("disabled by the compositor");
                self.went_stale();
            }
            CapEvent::ZonesChanged => {
                match self.refresh_zones().await {
                    Ok(zone_set) => info!(zone_set, "input capture zones changed (monitor configuration)"),
                    Err(e) => warn!("reading the capture zones failed: {e:#}"),
                }
                self.changed("zones changed");
                if self.enabled {
                    // mutter disabled the session with the change (no
                    // `Disabled` signal): its barriers are gone.
                    info!(
                        "the compositor dropped the pointer barriers with the monitor change; re-creating the session"
                    );
                    self.went_stale();
                }
            }
            CapEvent::Input(le) => {
                if self.active.is_some() {
                    let _ = self.tx.send(le);
                }
            }
        }
    }

    /// The session is useless now: end a capture, re-create it shortly
    /// (or earlier when new edges arrive).
    fn went_stale(&mut self) {
        self.enabled = false;
        self.stale = true;
        self.applied = None;
        self.end_active();
        if self.heal_at.is_none() {
            self.heal_at = Some(Instant::now() + ZONES_SETTLE);
        }
    }

    /// Re-creates a stale session for the current edges.
    async fn heal(&mut self) {
        if !self.open || !self.stale {
            return;
        }
        if self.edges.is_empty() {
            // Nothing to capture; the next edges create a new session.
            return;
        }
        if self.recreate("the compositor disabled it").await {
            self.apply_edges().await;
        }
    }

    /// Self-healing: barriers that are set and enabled must belong to the
    /// current session and the compositor's current zone set; otherwise the
    /// compositor dropped them without telling (no signal) and the session
    /// is re-created. Also retries an enable that failed.
    async fn check(&mut self) {
        self.next_check = Instant::now() + HEALTH_CHECK;
        if !self.open || self.stale || self.refused {
            return;
        }
        if !self.enabled {
            if !self.edges.is_empty() {
                info!("input capture is not enabled although edges lead to other machines; enabling it again");
                self.apply_edges().await;
            }
            return;
        }
        let zone_set = match self.refresh_zones().await {
            Ok(z) => z,
            Err(e) => {
                warn!("checking the capture zones failed: {e:#}");
                return;
            }
        };
        match self.applied {
            Some((session, applied)) if session == self.sessions && applied == zone_set => {}
            applied => {
                info!(
                    ?applied,
                    zone_set,
                    session = self.sessions,
                    "the pointer barriers belong to an old session or zone set; re-creating the session"
                );
                self.changed("barriers out of date");
                self.went_stale();
                self.heal_at = Some(Instant::now());
            }
        }
    }

    /// Reads the zones; reports changed monitors to the core. Returns the
    /// zone set.
    async fn refresh_zones(&mut self) -> Result<u32> {
        let (zone_set, m) = self.portal.zones().await?;
        let changed = *self.monitors.lock().expect("poisoned") != m;
        if changed {
            info!(monitors = m.len(), "capture zones differ from the known monitors");
            *self.monitors.lock().expect("poisoned") = m.clone();
            let _ = self.tx.send(LocalEvent::MonitorsChanged(m));
        }
        Ok(zone_set)
    }

    fn end_active(&mut self) {
        if self.active.take().is_some() {
            let _ = self.tx.send(LocalEvent::CaptureEnded);
        }
    }

    /// The session is gone: give the local devices back and schedule a new
    /// session.
    async fn lost(&mut self, why: &str) {
        warn!("input capture session lost ({why}); re-creating it");
        if self.opened_at.elapsed() > RETRY_MAX {
            self.backoff.reset();
        }
        self.open = false;
        self.enabled = false;
        self.stale = false;
        self.heal_at = None;
        self.applied = None;
        self.barriers = BarrierSet::default();
        self.changed(format!("lost: {why}"));
        self.end_active();
        self.portal.close().await;
        self.schedule_retry();
    }

    fn schedule_retry(&mut self) {
        let d = self.backoff.next();
        debug!(delay = ?d, "next capture session attempt");
        self.retry_at = Some(Instant::now() + d);
    }

    fn open_failed(&mut self, e: anyhow::Error, what: &str) {
        if is_refusal(&e) {
            warn!("{IC_REFUSED}");
            crate::problems::set(PROBLEM_IC, IC_REFUSED);
            self.refused = true;
            self.changed("permission refused");
        } else {
            warn!("{what}: {e:#}");
            self.changed(format!("{what}: {e:#}"));
            self.schedule_retry();
        }
    }

    async fn reopen(&mut self) {
        self.publish_state("opening");
        match self.portal.open().await {
            Ok(m) => {
                info!("input capture session re-created");
                self.session_opened(m);
                self.apply_edges().await;
            }
            Err(e) => self.open_failed(e, "re-creating the capture session failed"),
        }
    }
}

fn ei_to_local(ev: &EiEvent, scroll_acc: &mut (f32, f32)) -> Option<LocalEvent> {
    Some(match ev {
        EiEvent::PointerMotion(m) => LocalEvent::Motion { dx: m.dx as f64, dy: m.dy as f64 },
        EiEvent::Button(b) => {
            LocalEvent::Button { button: evdev_to_button(b.button)?, down: b.state == ei::button::ButtonState::Press }
        }
        EiEvent::ScrollDiscrete(s) => LocalEvent::Wheel { dx: s.discrete_dx, dy: -s.discrete_dy },
        EiEvent::ScrollDelta(s) => {
            // Smooth scrolling in logical pixels; ~15 px per wheel detent.
            scroll_acc.0 += s.dx * 8.0;
            scroll_acc.1 += s.dy * 8.0;
            let (dx, dy) = (scroll_acc.0.trunc() as i32, scroll_acc.1.trunc() as i32);
            if dx == 0 && dy == 0 {
                return None;
            }
            scroll_acc.0 -= dx as f32;
            scroll_acc.1 -= dy as f32;
            LocalEvent::Wheel { dx, dy: -dy }
        }
        EiEvent::KeyboardKey(k) => LocalEvent::Key {
            usage: flow_keymap::evdev_to_hid(k.key as u16)?,
            down: k.state == ei::keyboard::KeyState::Press,
        },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use flow_proto::NodeId;

    #[test]
    fn barrier_geometry_matches_portal_convention() {
        let e = Edge { side: Side::Right, pos: 1920, start: 0, end: 1080, to: NodeId(2) };
        assert_eq!(barrier_for(&e).as_tuple(), (1920, 0, 1920, 1079));
        let e = Edge { side: Side::Top, pos: 0, start: 100, end: 300, to: NodeId(2) };
        assert_eq!(barrier_for(&e).as_tuple(), (100, 0, 299, 0));
        assert_eq!(inside(Side::Right, 1920.0, 5.0), (1918.0, 5.0));
    }

    /// A [`CapturePortal`] whose sessions are event channels handed out by
    /// the test: dropping the sender ends the session's EI stream.
    struct FakePortal {
        sessions: mpsc::UnboundedReceiver<Result<mpsc::UnboundedReceiver<CapEvent>>>,
        current: Option<mpsc::UnboundedReceiver<CapEvent>>,
        log: Arc<Mutex<Vec<String>>>,
        /// The compositor's zone set (bumped by a monitor change).
        zone_set: Arc<Mutex<u32>>,
    }

    fn monitor() -> Vec<Monitor> {
        vec![Monitor { name: "zone-0".into(), rect: Rect::new(0, 0, 1920, 1080), primary: true }]
    }

    impl FakePortal {
        fn note(&self, s: impl Into<String>) {
            self.log.lock().unwrap().push(s.into());
        }
    }

    impl CapturePortal for FakePortal {
        async fn open(&mut self) -> Result<Vec<Monitor>> {
            self.current = None;
            self.note("open");
            // Like a portal waiting for its dialog: blocks until the test
            // decides the outcome.
            let s = self.sessions.recv().await.unwrap_or_else(|| Err(anyhow!("portal gone")))?;
            self.current = Some(s);
            Ok(monitor())
        }
        async fn close(&mut self) {
            self.note("close");
            self.current = None;
        }
        async fn set_barriers(&mut self, edges: &[Edge]) -> Result<BarrierSet> {
            self.note(format!("barriers {}", edges.len()));
            let ids = edges.iter().enumerate().map(|(i, e)| (i as u32 + 1, e.side)).collect();
            Ok(BarrierSet { ids, refused: 0, zone_set: *self.zone_set.lock().unwrap() })
        }
        async fn enable(&mut self) -> Result<()> {
            self.note("enable");
            Ok(())
        }
        async fn release(&mut self, _: Option<u32>, _: Option<(f64, f64)>) -> Result<()> {
            self.note("release");
            Ok(())
        }
        async fn zones(&mut self) -> Result<(u32, Vec<Monitor>)> {
            Ok((*self.zone_set.lock().unwrap(), monitor()))
        }
        async fn next_event(&mut self) -> Option<CapEvent> {
            self.current.as_mut()?.recv().await
        }
    }

    struct Rig {
        cmds: mpsc::UnboundedSender<CapCmd>,
        sessions: mpsc::UnboundedSender<Result<mpsc::UnboundedReceiver<CapEvent>>>,
        events: EventRx,
        log: Arc<Mutex<Vec<String>>>,
        zone_set: Arc<Mutex<u32>>,
        report: Arc<Mutex<CaptureReport>>,
    }

    impl Rig {
        /// Starts [`run_capture`] on an open session; returns the session's
        /// event sender.
        fn start() -> (Rig, mpsc::UnboundedSender<CapEvent>) {
            let (sessions, sessions_rx) = mpsc::unbounded_channel();
            let (first, first_rx) = mpsc::unbounded_channel();
            let log = Arc::new(Mutex::new(Vec::new()));
            let zone_set = Arc::new(Mutex::new(1));
            let portal = FakePortal {
                sessions: sessions_rx,
                current: Some(first_rx),
                log: log.clone(),
                zone_set: zone_set.clone(),
            };
            let (cmds, cmds_rx) = mpsc::unbounded_channel();
            let (tx, events) = mpsc::unbounded_channel();
            let report = Arc::new(Mutex::new(CaptureReport::default()));
            tokio::task::spawn_local(run_capture(portal, cmds_rx, tx, Arc::new(Mutex::new(monitor())), report.clone()));
            (Rig { cmds, sessions, events, log, zone_set, report }, first)
        }

        fn new_session(&self) -> mpsc::UnboundedSender<CapEvent> {
            let (s, r) = mpsc::unbounded_channel();
            self.sessions.send(Ok(r)).unwrap();
            s
        }

        async fn next(&mut self) -> LocalEvent {
            tokio::time::timeout(Duration::from_secs(300), self.events.recv()).await.expect("no event").unwrap()
        }

        /// Lets the loop run until it is idle (time is paused in the tests,
        /// so this does not wait for backoff timers).
        async fn settle(&self) {
            for _ in 0..50 {
                tokio::task::yield_now().await;
            }
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        fn take_log(&self) -> Vec<String> {
            std::mem::take(&mut *self.log.lock().unwrap())
        }
    }

    fn right_edge() -> Vec<Edge> {
        vec![Edge { side: Side::Right, pos: 1920, start: 0, end: 1080, to: NodeId(2) }]
    }

    fn activated() -> CapEvent {
        CapEvent::Activated { activation_id: Some(7), barrier: Some(1), pos: (1920.0, 500.0) }
    }

    async fn local<F: Future<Output = ()>>(f: F) {
        tokio::task::LocalSet::new().run_until(f).await
    }

    /// Drives [`keep_injecting`] with fake sessions: `setup` takes the
    /// next outcome from `outcomes`, a session logs what it injects, is
    /// "lost" when it receives `Action::Left` and closes like the real
    /// runners when it is no longer wanted.
    struct InjRig {
        rd: Arc<RdShared>,
        events: mpsc::UnboundedReceiver<LocalEvent>,
        cmds: mpsc::UnboundedSender<InjCmd>,
        outcomes: mpsc::UnboundedSender<Result<u32>>,
        injected: Arc<Mutex<Vec<(u32, Action)>>>,
        /// Sessions closed on request, in order.
        closed: Arc<Mutex<Vec<u32>>>,
        setups: Arc<Mutex<u32>>,
        done: tokio::task::JoinHandle<()>,
    }

    fn start_injecting() -> InjRig {
        let (ev_tx, events) = mpsc::unbounded_channel();
        let rd = Arc::new(RdShared::new(ev_tx));
        let (cmds, mut cmds_rx) = mpsc::unbounded_channel();
        let (outcomes, outcomes_rx) = mpsc::unbounded_channel::<Result<u32>>();
        let injected = Arc::new(Mutex::new(Vec::new()));
        let closed = Arc::new(Mutex::new(Vec::new()));
        let setups = Arc::new(Mutex::new(0));
        let (inj, cl, n, shared) = (injected.clone(), closed.clone(), setups.clone(), rd.clone());
        let done = tokio::task::spawn_local(async move {
            let mut outcomes_rx = outcomes_rx;
            let rd = shared.clone();
            keep_injecting(
                &rd,
                &mut cmds_rx,
                async || {
                    *n.lock().unwrap() += 1;
                    let r = outcomes_rx.recv().await.unwrap_or_else(|| Err(anyhow!("portal gone")));
                    r.map(|id| (id, true))
                },
                async |id: u32, cmds: &mut mpsc::UnboundedReceiver<InjCmd>| {
                    if !shared.wanted() {
                        cl.lock().unwrap().push(id);
                        return InjEnd::Closed;
                    }
                    loop {
                        match cmds.recv().await {
                            None => return InjEnd::Quit,
                            Some(InjCmd::Sync) if !shared.wanted() => {
                                cl.lock().unwrap().push(id);
                                return InjEnd::Closed;
                            }
                            Some(InjCmd::Inject(Action::Left)) => return InjEnd::Lost,
                            Some(InjCmd::Inject(a)) => inj.lock().unwrap().push((id, a)),
                            Some(InjCmd::Sync | InjCmd::SetClipboard(_)) => {}
                        }
                    }
                },
            )
            .await
        });
        InjRig { rd, events, cmds, outcomes, injected, closed, setups, done }
    }

    impl InjRig {
        /// What `Backend::set_controllable` does.
        fn want(&self, on: bool) {
            self.rd.want.store(on, Ordering::SeqCst);
            self.cmds.send(InjCmd::Sync).unwrap();
        }
        fn caps_changes(&mut self) -> usize {
            let mut n = 0;
            while let Ok(ev) = self.events.try_recv() {
                assert_eq!(ev, LocalEvent::CapsChanged);
                n += 1;
            }
            n
        }
    }

    fn key(usage: u32) -> InjCmd {
        InjCmd::Inject(Action::Key { usage, down: true })
    }

    fn key_up(usage: u32) -> InjCmd {
        InjCmd::Inject(Action::Key { usage, down: false })
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn remote_clipboard_waits_for_the_session_but_not_forever() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let rd = RdShared::new(tx);
        let text = |s: &str| ClipContent::Text(s.into());
        rd.keep_remote_clip(Some(text("one")));
        rd.keep_remote_clip(Some(text("two")));
        assert_eq!(rd.take_remote_clip(), Some(text("two")), "the newest content is set once the session opens");
        assert_eq!(rd.take_remote_clip(), None, "and only once");
        rd.keep_remote_clip(Some(text("old")));
        tokio::time::sleep(REMOTE_CLIP_KEEP + Duration::from_secs(1)).await;
        assert_eq!(rd.take_remote_clip(), None, "too old to overwrite the clipboard");
    }

    #[tokio::test(start_paused = true)]
    async fn no_session_until_wanted_and_input_meanwhile_is_dropped() {
        local(async {
            let mut rig = start_injecting();
            rig.cmds.send(key(4)).unwrap();
            settle().await;
            tokio::time::sleep(RETRY_MAX).await;
            settle().await;
            assert_eq!(*rig.setups.lock().unwrap(), 0, "no session (and no GNOME indicator) before it is wanted");
            assert!(!rig.rd.injecting());
            rig.want(true);
            settle().await;
            assert_eq!(*rig.setups.lock().unwrap(), 1);
            assert!(!rig.rd.injecting(), "not ready while opening");
            rig.outcomes.send(Ok(1)).unwrap();
            settle().await;
            assert!(rig.rd.injecting() && rig.rd.portal_clip());
            assert_eq!(rig.caps_changes(), 1, "the core hears that it can inject now");
            rig.cmds.send(key(5)).unwrap();
            settle().await;
            assert_eq!(*rig.injected.lock().unwrap(), [(1, Action::Key { usage: 5, down: true })]);
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn closing_applies_queued_releases_first_and_reopening_makes_a_new_session() {
        local(async {
            let mut rig = start_injecting();
            rig.want(true);
            rig.outcomes.send(Ok(1)).unwrap();
            settle().await;
            assert!(rig.rd.injecting());
            rig.cmds.send(key(4)).unwrap();
            // The core releases, then asks for the close.
            rig.cmds.send(key_up(4)).unwrap();
            rig.want(false);
            assert!(!rig.rd.injecting(), "not ready from the moment the close is asked for");
            settle().await;
            assert_eq!(
                *rig.injected.lock().unwrap(),
                [(1, Action::Key { usage: 4, down: true }), (1, Action::Key { usage: 4, down: false })]
            );
            assert_eq!(*rig.closed.lock().unwrap(), [1]);
            assert_eq!(rig.caps_changes(), 2, "open, closed");
            // Input while closed is dropped; nothing is re-created on its own.
            rig.cmds.send(key(5)).unwrap();
            tokio::time::sleep(RETRY_MAX).await;
            settle().await;
            assert_eq!(*rig.setups.lock().unwrap(), 1);
            // Reopened on request (the real setup reuses the restore token).
            rig.want(true);
            rig.outcomes.send(Ok(2)).unwrap();
            settle().await;
            rig.cmds.send(key(6)).unwrap();
            settle().await;
            assert_eq!(*rig.setups.lock().unwrap(), 2);
            assert_eq!(rig.injected.lock().unwrap().last(), Some(&(2, Action::Key { usage: 6, down: true })));
            drop(rig.cmds);
            rig.done.await.unwrap();
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn a_close_asked_for_while_opening_closes_right_after() {
        local(async {
            let mut rig = start_injecting();
            rig.want(true);
            settle().await;
            // The core changes its mind while the portal is still answering.
            rig.want(false);
            rig.cmds.send(key(4)).unwrap();
            rig.outcomes.send(Ok(1)).unwrap();
            settle().await;
            assert_eq!(*rig.closed.lock().unwrap(), [1]);
            assert!(rig.injected.lock().unwrap().is_empty());
            assert_eq!(rig.caps_changes(), 0, "never reported ready");
            // A flap back on while open does not close it.
            rig.want(true);
            rig.outcomes.send(Ok(2)).unwrap();
            settle().await;
            rig.want(false);
            rig.want(true);
            settle().await;
            assert!(rig.rd.injecting());
            assert_eq!(*rig.closed.lock().unwrap(), [1]);
            assert_eq!(*rig.setups.lock().unwrap(), 2);
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn injection_session_is_recreated_after_an_ei_disconnect() {
        local(async {
            let rig = start_injecting();
            rig.want(true);
            rig.outcomes.send(Ok(1)).unwrap();
            settle().await;
            rig.cmds.send(key(4)).unwrap();
            settle().await;
            // EI disconnect.
            rig.cmds.send(InjCmd::Inject(Action::Left)).unwrap();
            settle().await;
            assert!(!rig.rd.injecting());
            // Input during the outage is dropped, not replayed later.
            rig.cmds.send(key(5)).unwrap();
            settle().await;
            assert_eq!(*rig.setups.lock().unwrap(), 1);
            // The first re-creation fails (portal restarting), the next
            // one works.
            rig.outcomes.send(Err(anyhow!("no portal"))).unwrap();
            tokio::time::sleep(RETRY_MIN).await;
            settle().await;
            assert_eq!(*rig.setups.lock().unwrap(), 2);
            rig.outcomes.send(Ok(2)).unwrap();
            tokio::time::sleep(RETRY_MIN * 2).await;
            settle().await;
            assert_eq!(*rig.setups.lock().unwrap(), 3);
            rig.cmds.send(key(6)).unwrap();
            settle().await;
            assert_eq!(
                *rig.injected.lock().unwrap(),
                [(1, Action::Key { usage: 4, down: true }), (2, Action::Key { usage: 6, down: true })]
            );
            // Dropping the backend ends the thread.
            drop(rig.cmds);
            rig.done.await.unwrap();
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_session_is_not_recreated_once_no_longer_wanted() {
        local(async {
            let rig = start_injecting();
            rig.want(true);
            rig.outcomes.send(Ok(1)).unwrap();
            settle().await;
            rig.cmds.send(InjCmd::Inject(Action::Left)).unwrap();
            settle().await;
            rig.want(false);
            tokio::time::sleep(RETRY_MAX).await;
            settle().await;
            assert_eq!(*rig.setups.lock().unwrap(), 1);
            assert!(!rig.done.is_finished());
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn injection_stops_when_the_dialog_is_refused_on_recreation() {
        local(async {
            let rig = start_injecting();
            rig.want(true);
            rig.outcomes.send(Ok(1)).unwrap();
            settle().await;
            rig.cmds.send(InjCmd::Inject(Action::Left)).unwrap();
            rig.outcomes.send(Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled).into())).unwrap();
            tokio::time::sleep(RETRY_MIN * 2).await;
            settle().await;
            assert!(rig.done.is_finished());
            assert_eq!(*rig.setups.lock().unwrap(), 2);
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_first_dialog_is_not_asked_again() {
        local(async {
            let rig = start_injecting();
            rig.want(true);
            rig.outcomes.send(Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled).into())).unwrap();
            settle().await;
            assert!(rig.done.is_finished());
            assert!(rig.cmds.send(InjCmd::Sync).is_err(), "the injector is gone: set_controllable does nothing");
            assert!(!rig.rd.injecting());
            assert_eq!(*rig.setups.lock().unwrap(), 1);
        })
        .await
    }

    /// The premise of [`session_bus`], on a private bus: a zbus connection
    /// dies with the portal thread whose runtime created it, while a
    /// connection that another portal thread created for itself keeps
    /// working after that thread is gone. Skipped without `dbus-daemon`.
    #[test]
    fn bus_connection_belongs_to_the_portal_thread_that_built_it() {
        let Ok(mut daemon) = std::process::Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address"])
            .stdout(std::process::Stdio::piped())
            .spawn()
        else {
            eprintln!("dbus-daemon not available, skipping");
            return;
        };
        let mut addr = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(daemon.stdout.take().unwrap()), &mut addr).unwrap();
        let addr = addr.trim().to_string();
        let connect = {
            let addr = addr.clone();
            move || {
                let addr = addr.clone();
                async move {
                    ashpd::zbus::connection::Builder::address(addr.as_str()).build().await.map_err(anyhow::Error::from)
                }
            }
        };
        let get_id = |c: ashpd::zbus::Connection| async move {
            let p = ashpd::zbus::fdo::DBusProxy::new(&c).await?;
            tokio::time::timeout(Duration::from_secs(2), p.get_id()).await?.map_err(anyhow::Error::from)
        };
        let block = |rx: tokio::sync::oneshot::Receiver<Result<ashpd::zbus::Connection>>| {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(rx).unwrap()
        };

        // Thread A builds a connection and exits, like the injector thread
        // after a refused dialog.
        let c = connect.clone();
        let from_a = block(spawn_portal_thread("bus-a", move |ready| async move {
            let _ = ready.send(c().await);
        }))
        .unwrap();
        // Thread B keeps running and uses its own connection, and A's.
        let c = connect.clone();
        let (res_tx, res_rx) = std::sync::mpsc::channel();
        let ready = spawn_portal_thread("bus-b", move |ready| async move {
            let own = c().await;
            let _ = ready.send(Ok(()));
            let own = match own {
                Ok(own) => get_id(own).await,
                Err(e) => Err(e),
            };
            std::thread::sleep(Duration::from_millis(100));
            let _ = res_tx.send((own.is_ok(), get_id(from_a).await.is_ok()));
        });
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(ready).unwrap().unwrap();
        let (own_ok, shared_ok) = res_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let _ = daemon.kill();
        let _ = daemon.wait();
        assert!(own_ok, "a connection built on the running portal thread must work");
        assert!(!shared_ok, "zbus changed: a connection outlives the runtime that built it");
    }

    #[tokio::test(start_paused = true)]
    async fn capture_ends_and_session_is_recreated_when_the_ei_stream_closes() {
        local(async {
            let (mut rig, first) = Rig::start();
            rig.cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
            rig.settle().await;
            first.send(activated()).unwrap();
            assert_eq!(rig.next().await, LocalEvent::EdgeHit { side: Side::Right, x: 1920, y: 500 });
            first.send(CapEvent::Input(LocalEvent::Motion { dx: 1.0, dy: 0.0 })).unwrap();
            assert_eq!(rig.next().await, LocalEvent::Motion { dx: 1.0, dy: 0.0 });
            assert_eq!(rig.take_log(), ["barriers 1", "enable"]);

            // The EIS side goes away while input is captured.
            drop(first);
            assert_eq!(rig.next().await, LocalEvent::CaptureEnded);
            rig.settle().await;
            assert_eq!(rig.take_log(), ["close"]);

            // Commands are still processed while the session is down: a
            // Release is a no-op, new edges are remembered.
            rig.cmds.send(CapCmd::Release(None)).unwrap();
            let mut two = right_edge();
            two.push(Edge { side: Side::Left, pos: 0, start: 0, end: 1080, to: NodeId(3) });
            rig.cmds.send(CapCmd::SetEdges(two)).unwrap();
            rig.settle().await;
            assert!(rig.log().is_empty(), "{:?}", rig.log());

            // After the backoff a new session is created and gets the
            // latest edges.
            let second = rig.new_session();
            tokio::time::sleep(RETRY_MIN * 2).await;
            rig.settle().await;
            assert_eq!(rig.take_log(), ["open", "barriers 2", "enable"]);
            second.send(activated()).unwrap();
            assert_eq!(rig.next().await, LocalEvent::EdgeHit { side: Side::Right, x: 1920, y: 500 });
            rig.cmds.send(CapCmd::Release(Some((10, 10)))).unwrap();
            rig.settle().await;
            assert_eq!(rig.take_log(), ["release"]);
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn failed_recreation_is_retried_with_backoff() {
        local(async {
            let (mut rig, first) = Rig::start();
            rig.cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
            drop(first);
            rig.settle().await;
            rig.take_log();
            // First attempt: the portal is not there.
            rig.sessions.send(Err(anyhow!("org.freedesktop.portal.Desktop not found"))).unwrap();
            tokio::time::sleep(RETRY_MIN).await;
            rig.settle().await;
            assert_eq!(rig.take_log(), ["open"]);
            // The next attempt comes after a longer delay.
            tokio::time::sleep(RETRY_MIN * 3 / 2).await;
            rig.settle().await;
            assert!(rig.log().is_empty(), "retried too early: {:?}", rig.log());
            let second = rig.new_session();
            tokio::time::sleep(RETRY_MIN).await;
            rig.settle().await;
            assert_eq!(rig.take_log(), ["open", "barriers 1", "enable"]);
            second.send(activated()).unwrap();
            assert_eq!(rig.next().await, LocalEvent::EdgeHit { side: Side::Right, x: 1920, y: 500 });
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn refused_permission_is_not_asked_again() {
        local(async {
            let (rig, first) = Rig::start();
            drop(first);
            rig.settle().await;
            rig.take_log();
            rig.sessions.send(Err(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled).into())).unwrap();
            tokio::time::sleep(RETRY_MIN).await;
            rig.settle().await;
            assert_eq!(rig.take_log(), ["open"]);
            tokio::time::sleep(RETRY_MAX * 2).await;
            rig.cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
            rig.settle().await;
            assert!(rig.log().is_empty(), "{:?}", rig.log());
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn compositor_disable_ends_capture_and_next_layout_recreates_the_session() {
        local(async {
            let (mut rig, first) = Rig::start();
            rig.cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
            rig.settle().await;
            first.send(activated()).unwrap();
            assert!(matches!(rig.next().await, LocalEvent::EdgeHit { .. }));
            first.send(CapEvent::Disabled).unwrap();
            assert_eq!(rig.next().await, LocalEvent::CaptureEnded);
            rig.settle().await;
            rig.take_log();
            let _second = rig.new_session();
            rig.cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
            rig.settle().await;
            assert_eq!(rig.take_log(), ["open", "barriers 1", "enable"]);
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn a_monitor_change_recreates_the_session_and_sets_the_barriers_again() {
        local(async {
            let (mut rig, first) = Rig::start();
            rig.cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
            rig.settle().await;
            assert_eq!(rig.take_log(), ["barriers 1", "enable"]);
            assert_eq!(rig.report.lock().unwrap().state, "enabled");
            // GNOME: a new zone set, the session silently disabled, only
            // ZonesChanged. Input captured at that moment is given back.
            first.send(activated()).unwrap();
            assert!(matches!(rig.next().await, LocalEvent::EdgeHit { .. }));
            *rig.zone_set.lock().unwrap() = 2;
            first.send(CapEvent::ZonesChanged).unwrap();
            assert_eq!(rig.next().await, LocalEvent::CaptureEnded);
            rig.settle().await;
            assert_eq!(rig.report.lock().unwrap().state, "re-creating");
            assert!(rig.log().is_empty(), "waits for the new layout first: {:?}", rig.log());
            // The core sends the same edges again (layout unchanged): the
            // session is re-created once, right away.
            let second = rig.new_session();
            rig.cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
            rig.settle().await;
            assert_eq!(rig.take_log(), ["open", "barriers 1", "enable"]);
            tokio::time::sleep(ZONES_SETTLE * 2).await;
            rig.settle().await;
            assert!(rig.log().is_empty(), "not re-created a second time: {:?}", rig.log());
            second.send(activated()).unwrap();
            assert!(matches!(rig.next().await, LocalEvent::EdgeHit { .. }));
            let r = rig.report.lock().unwrap().clone();
            assert_eq!((r.state.as_str(), r.barriers, r.zone_set, r.sessions), ("enabled", 1, Some(2), 2));
            assert_eq!(r.since_activation, Some(Duration::ZERO));
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn without_new_edges_the_session_is_recreated_after_a_monitor_change() {
        local(async {
            let (rig, first) = Rig::start();
            rig.cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
            rig.settle().await;
            rig.take_log();
            let _second = rig.new_session();
            *rig.zone_set.lock().unwrap() = 2;
            first.send(CapEvent::ZonesChanged).unwrap();
            // The hub is down, nobody sends edges: healed on its own.
            tokio::time::sleep(ZONES_SETTLE).await;
            rig.settle().await;
            assert_eq!(rig.take_log(), ["open", "barriers 1", "enable"]);
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn a_monitor_change_before_any_barrier_needs_no_new_session() {
        local(async {
            let (rig, first) = Rig::start();
            *rig.zone_set.lock().unwrap() = 2;
            first.send(CapEvent::ZonesChanged).unwrap();
            tokio::time::sleep(ZONES_SETTLE * 2).await;
            rig.settle().await;
            assert!(rig.log().is_empty(), "{:?}", rig.log());
            rig.cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
            rig.settle().await;
            assert_eq!(rig.take_log(), ["barriers 1", "enable"]);
            assert_eq!(rig.report.lock().unwrap().zone_set, Some(2));
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn barriers_for_an_old_zone_set_are_noticed_and_set_again() {
        local(async {
            let (rig, _first) = Rig::start();
            rig.cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
            rig.settle().await;
            rig.take_log();
            // The zone set changes and the signal is lost.
            *rig.zone_set.lock().unwrap() = 5;
            // The same edges again (the hub connection came back) trigger
            // the check.
            let _second = rig.new_session();
            rig.cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
            rig.settle().await;
            assert_eq!(rig.take_log(), ["open", "barriers 1", "enable"]);
            assert_eq!(rig.report.lock().unwrap().zone_set, Some(5));
            // Without any trigger, the periodic check finds it too.
            *rig.zone_set.lock().unwrap() = 6;
            let _third = rig.new_session();
            tokio::time::sleep(HEALTH_CHECK).await;
            rig.settle().await;
            assert_eq!(rig.take_log(), ["open", "barriers 1", "enable"]);
            // Nothing to do while everything matches.
            tokio::time::sleep(HEALTH_CHECK * 3).await;
            rig.settle().await;
            assert!(rig.log().is_empty(), "{:?}", rig.log());
            assert_eq!(rig.report.lock().unwrap().sessions, 3);
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn a_compositor_disable_is_healed_without_a_layout_change() {
        local(async {
            let (rig, first) = Rig::start();
            rig.cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
            rig.settle().await;
            rig.take_log();
            let _second = rig.new_session();
            first.send(CapEvent::Disabled).unwrap();
            tokio::time::sleep(ZONES_SETTLE).await;
            rig.settle().await;
            assert_eq!(rig.take_log(), ["open", "barriers 1", "enable"]);
        })
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn edges_queued_during_the_permission_dialog_do_not_recreate_again() {
        local(async {
            let (mut rig, _first) = Rig::start();
            rig.cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
            rig.settle().await;
            rig.take_log();
            // A layout change re-creates the session; the dialog is open
            // while two more updates arrive.
            rig.cmds.send(CapCmd::SetEdges(vec![])).unwrap();
            rig.settle().await;
            let mut two = right_edge();
            two.push(Edge { side: Side::Left, pos: 0, start: 0, end: 1080, to: NodeId(3) });
            rig.cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
            rig.cmds.send(CapCmd::SetEdges(two.clone())).unwrap();
            let second = rig.new_session();
            rig.settle().await;
            assert_eq!(rig.take_log(), ["open", "barriers 2", "enable"]);
            rig.cmds.send(CapCmd::SetEdges(two)).unwrap();
            rig.settle().await;
            assert!(rig.log().is_empty(), "{:?}", rig.log());
            second.send(CapEvent::Activated { activation_id: None, barrier: Some(2), pos: (0.0, 9.0) }).unwrap();
            assert_eq!(rig.next().await, LocalEvent::EdgeHit { side: Side::Left, x: 0, y: 9 });
        })
        .await
    }
}
