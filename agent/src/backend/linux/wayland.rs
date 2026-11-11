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
//! **Injection** – `org.freedesktop.portal.RemoteDesktop` with
//! `ConnectToEIS` (EI sender context: absolute/relative pointer, buttons,
//! scroll, keyboard with evdev keycodes). The portal restore token is
//! stored (`portal-tokens.json`) so the permission dialog appears only
//! once. Without EIS (older portals) the D-Bus `Notify*` methods are used.
//!
//! **Clipboard** – the portal Clipboard interface on the RemoteDesktop
//! session (xdg-desktop-portal ≥ 1.17, GNOME ≥ 45). If unavailable, the X11
//! CLIPBOARD selection through XWayland is used (mutter syncs it with the
//! Wayland clipboard), which needs XWayland to be running.

use super::x11clip::{self, X11Clipboard};
use super::{button_to_evdev, evdev_to_button, PNG_MIME, TEXT_MIMES};
use crate::backend::{Backend, BackendOptions, EventRx, EventTx, LocalEvent};
use crate::config::{load_portal_token, save_portal_token};
use anyhow::{anyhow, Context, Result};
use ashpd::desktop::clipboard::{Clipboard, SetSelectionOptions};
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
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, info, warn};

const RD_TOKEN: &str = "remote_desktop";

enum InjCmd {
    Inject(Action),
    SetClipboard(ClipContent),
}

enum CapCmd {
    SetEdges(Vec<Edge>),
    Release(Option<(i32, i32)>),
}

pub struct WaylandBackend {
    monitors: Arc<Mutex<Vec<Monitor>>>,
    caps: Capabilities,
    inj: Option<mpsc::UnboundedSender<InjCmd>>,
    cap: Option<mpsc::UnboundedSender<CapCmd>>,
    x11clip: Option<X11Clipboard>,
    portal_clip: bool,
}

pub async fn create(opts: &BackendOptions) -> Result<(Arc<dyn Backend>, EventRx)> {
    let (tx, rx) = mpsc::unbounded_channel();
    let monitors = Arc::new(Mutex::new(Vec::new()));

    let (inj, portal_clip) = match start_injector(opts.state_dir.clone(), tx.clone()).await {
        Ok((s, clip)) => (Some(s), clip),
        Err(e) => {
            warn!("RemoteDesktop portal unavailable, this machine cannot be controlled: {e:#}");
            (None, false)
        }
    };
    let cap = match start_capture(tx.clone(), monitors.clone()).await {
        Ok(s) => Some(s),
        Err(e) => {
            warn!(
                "InputCapture portal unavailable ({e:#}); this machine can be controlled but cannot control others. \
                 InputCapture needs GNOME ≥ 45 / xdg-desktop-portal ≥ 1.18; on older systems use the Xorg session \
                 (backend = \"x11\")."
            );
            None
        }
    };
    let x11clip = if portal_clip {
        None
    } else if std::env::var_os("DISPLAY").is_some() {
        match x11clip::spawn(tx.clone()) {
            Ok(c) => {
                info!("clipboard: portal clipboard unavailable, using the X11 selection via XWayland");
                Some(c)
            }
            Err(e) => {
                warn!("no clipboard access (portal clipboard missing and XWayland not reachable): {e:#}");
                None
            }
        }
    } else {
        warn!("no clipboard access: portal clipboard missing and DISPLAY not set");
        None
    };
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
    let caps =
        Capabilities { capture: cap.is_some(), inject: inj.is_some(), clipboard: portal_clip || x11clip.is_some() };
    info!(?caps, "Wayland portal backend ready");
    Ok((Arc::new(WaylandBackend { monitors, caps, inj, cap, x11clip, portal_clip }), rx))
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
        self.caps
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
    fn set_clipboard(&self, content: &ClipContent) {
        if self.portal_clip {
            if let Some(i) = &self.inj {
                let _ = i.send(InjCmd::SetClipboard(content.clone()));
            }
        } else if let Some(c) = &self.x11clip {
            c.set(content);
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

async fn start_injector(state_dir: PathBuf, tx: EventTx) -> Result<(mpsc::UnboundedSender<InjCmd>, bool)> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let ready = spawn_portal_thread("portal-inject", move |ready| async move {
        match setup_injector(&state_dir).await {
            Ok((rd, session, clipboard, eis)) => {
                let _ = ready.send(Ok(clipboard.is_some()));
                match eis {
                    Some((ctx, conn, events)) => {
                        run_ei_sender(ctx, conn, events, cmd_rx, rd, session, clipboard, tx).await
                    }
                    None => run_dbus_injector(rd, session, cmd_rx, clipboard, tx).await,
                }
            }
            Err(e) => {
                let _ = ready.send(Err(e));
            }
        }
    });
    let clip_on = ready.await.map_err(|_| anyhow!("portal thread died"))??;
    Ok((cmd_tx, clip_on))
}

type EiParts = (ei::Context, reis::event::Connection, reis::tokio::EiConvertEventStream);

async fn setup_injector(
    state_dir: &std::path::Path,
) -> Result<(RemoteDesktop, Session<RemoteDesktop>, Option<Clipboard>, Option<EiParts>)> {
    let rd = RemoteDesktop::new().await.context("RemoteDesktop portal")?;
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
    let clipboard = match Clipboard::new().await {
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
    let clipboard = if clipboard.is_some() && resp.is_clipboard_enabled() {
        info!("clipboard via the portal Clipboard interface");
        clipboard
    } else {
        None
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

#[allow(clippy::too_many_arguments)]
async fn run_ei_sender(
    ctx: ei::Context,
    conn: reis::event::Connection,
    mut events: reis::tokio::EiConvertEventStream,
    mut cmds: mpsc::UnboundedReceiver<InjCmd>,
    _rd: RemoteDesktop,
    session: Session<RemoteDesktop>,
    clipboard: Option<Clipboard>,
    tx: EventTx,
) {
    let mut devs = EiDevices::default();
    let mut sequence = 1u32;
    let mut last_abs: Option<(f32, f32)> = None;
    let clip = clipboard;
    let mut owned: Option<ClipContent> = None;
    let (mut owner_changes, mut transfers) = match &clip {
        Some(c) => (
            c.receive_selection_owner_changed::<RemoteDesktop>().await.ok().map(|s| s.boxed()),
            c.receive_selection_transfer::<RemoteDesktop>().await.ok().map(|s| s.boxed()),
        ),
        None => (None, None),
    };
    loop {
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
                        return;
                    }
                    _ => {}
                },
                Some(Err(e)) => {
                    warn!("EI injection error: {e:?}");
                    return;
                }
                None => return,
            },
            cmd = cmds.recv() => match cmd {
                None => return,
                Some(InjCmd::Inject(a)) => {
                    ei_inject(&devs, &conn, a, &mut last_abs);
                    let _ = ctx.flush();
                }
                Some(InjCmd::SetClipboard(c)) => {
                    if let Some(cs) = &clip {
                        let mimes: Vec<&str> = match &c {
                            ClipContent::Text(_) => TEXT_MIMES.to_vec(),
                            ClipContent::Image { .. } => vec![PNG_MIME],
                        };
                        owned = Some(c);
                        if let Err(e) = cs.set_selection(&session, SetSelectionOptions::default().set_mime_types(&mimes)).await {
                            warn!("setting the clipboard failed: {e}");
                        }
                    }
                }
            },
            Some((_s, change)) = async { match owner_changes.as_mut() { Some(s) => s.next().await, None => std::future::pending().await } } => {
                if change.session_is_owner() == Some(true) {
                    continue;
                }
                let Some(cs) = &clip else { continue };
                let mimes = change.mime_types();
                let pick = if mimes.iter().any(|m| m == PNG_MIME) {
                    Some(PNG_MIME.to_string())
                } else {
                    TEXT_MIMES.iter().find(|t| mimes.iter().any(|m| m == *t)).map(|s| s.to_string())
                };
                if let Some(mime) = pick {
                    match cs.selection_read(&session, &mime).await {
                        Ok(fd) => {
                            let tx = tx.clone();
                            tokio::task::spawn_blocking(move || {
                                use std::io::Read;
                                let mut f = std::fs::File::from(std::os::fd::OwnedFd::from(fd));
                                let mut buf = Vec::new();
                                if f.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                                    let c = if mime == PNG_MIME {
                                        ClipContent::Image { png: buf }
                                    } else {
                                        ClipContent::Text(String::from_utf8_lossy(&buf).into_owned())
                                    };
                                    let _ = tx.send(LocalEvent::Clipboard(c));
                                }
                            });
                        }
                        Err(e) => debug!("clipboard read failed: {e}"),
                    }
                }
            }
            Some((_s, mime, serial)) = async { match transfers.as_mut() { Some(s) => s.next().await, None => std::future::pending().await } } => {
                let Some(cs) = &clip else { continue };
                let data: Option<Vec<u8>> = match &owned {
                    Some(ClipContent::Text(t)) if TEXT_MIMES.contains(&mime.as_str()) => Some(t.as_bytes().to_vec()),
                    Some(ClipContent::Image { png }) if mime == PNG_MIME => Some(png.clone()),
                    _ => None,
                };
                let ok = match (data, cs.selection_write(&session, serial).await) {
                    (Some(d), Ok(fd)) => tokio::task::spawn_blocking(move || {
                        use std::io::Write;
                        std::fs::File::from(std::os::fd::OwnedFd::from(fd)).write_all(&d).is_ok()
                    }).await.unwrap_or(false),
                    _ => false,
                };
                let _ = cs.selection_write_done(&session, serial, ok).await;
            }
        }
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

async fn run_dbus_injector(
    rd: RemoteDesktop,
    session: Session<RemoteDesktop>,
    mut cmds: mpsc::UnboundedReceiver<InjCmd>,
    _clipboard: Option<Clipboard>,
    _tx: EventTx,
) {
    // Without a screencast stream only relative motion is available.
    let mut last: Option<(i32, i32)> = None;
    while let Some(c) = cmds.recv().await {
        let r = match c {
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
    ZonesChanged,
    /// Captured input, already converted.
    Input(LocalEvent),
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
    /// Sets pointer barriers for `edges`; returns barrier id → side.
    async fn set_barriers(&mut self, edges: &[Edge]) -> Result<HashMap<u32, Side>>;
    async fn enable(&mut self) -> Result<()>;
    async fn release(&mut self, activation_id: Option<u32>, pos: Option<(f64, f64)>) -> Result<()>;
    async fn zones(&mut self) -> Result<Vec<Monitor>>;
    /// Next event of the open session, `None` once the session is gone (EI
    /// stream closed or failed, portal signals ended). Must be cancel-safe.
    async fn next_event(&mut self) -> Option<CapEvent>;
}

async fn start_capture(tx: EventTx, monitors: Arc<Mutex<Vec<Monitor>>>) -> Result<mpsc::UnboundedSender<CapCmd>> {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let ready = spawn_portal_thread("portal-capture", move |ready| async move {
        let mut portal = PortalCapture::default();
        match portal.open().await {
            Ok(m) => {
                *monitors.lock().expect("poisoned") = m;
                let _ = ready.send(Ok(()));
                run_capture(portal, cmd_rx, tx, monitors).await;
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
    Ok(s.filter_map(|m| std::future::ready(m.body().deserialize::<T>().ok())).boxed_local())
}

impl PortalCapture {
    fn live(&mut self) -> Result<&mut LiveCapture> {
        self.live.as_mut().ok_or_else(|| anyhow!("no InputCapture session"))
    }
}

impl CapturePortal for PortalCapture {
    async fn open(&mut self) -> Result<Vec<Monitor>> {
        self.close().await;
        let ic = InputCapture::new().await.context("InputCapture portal")?;
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
        self.scroll_acc = (0.0, 0.0);
        self.live =
            Some(LiveCapture { ic, session, ctx, events, zones, activated, deactivated, disabled, zones_changed });
        Ok(mons)
    }

    async fn close(&mut self) {
        if let Some(l) = self.live.take() {
            let _ = bounded("InputCapture.Disable", l.ic.disable(&l.session, Default::default())).await;
            let _ = bounded("Session.Close", l.session.close()).await;
        }
    }

    async fn set_barriers(&mut self, edges: &[Edge]) -> Result<HashMap<u32, Side>> {
        let l = self.live()?;
        let mut map = HashMap::new();
        let barriers: Vec<Barrier> = edges
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let id = BarrierID::new(i as u32 + 1)?;
                map.insert(i as u32 + 1, e.side);
                Some(Barrier::new(id, barrier_for(e)))
            })
            .collect();
        let resp = bounded(
            "InputCapture.SetPointerBarriers",
            l.ic.set_pointer_barriers(&l.session, &barriers, l.zones.zone_set(), Default::default()),
        )
        .await?
        .response()?;
        let failed = resp.failed_barriers();
        if !failed.is_empty() {
            warn!(?failed, "the compositor refused some pointer barriers (they must lie on outer screen edges)");
        }
        info!(barriers = barriers.len() - failed.len(), "pointer barriers set");
        Ok(map)
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

    async fn zones(&mut self) -> Result<Vec<Monitor>> {
        let l = self.live()?;
        l.zones = bounded("InputCapture.GetZones", l.ic.zones(&l.session, Default::default())).await?.response()?;
        Ok(zones_to_monitors(&l.zones))
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

/// State of the capture loop around one [`CapturePortal`].
struct Capture<P> {
    portal: P,
    cmds: mpsc::UnboundedReceiver<CapCmd>,
    tx: EventTx,
    monitors: Arc<Mutex<Vec<Monitor>>>,
    edges: Vec<Edge>,
    ids: HashMap<u32, Side>,
    /// The portal has a live session.
    open: bool,
    enabled: bool,
    /// Disabled by the compositor: GNOME 46 cannot re-enable a session, so
    /// the next barrier update creates a new one.
    stale: bool,
    /// (activation id, side, cursor position) while captured.
    active: Option<(Option<u32>, Side, (f64, f64))>,
    released_by_us: bool,
    backoff: Backoff,
    /// When to re-create a lost session.
    retry_at: Option<Instant>,
    /// When the current session was created; a session that lived longer
    /// than [`RETRY_MAX`] resets the backoff when it is lost.
    opened_at: Instant,
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
) {
    let mut c = Capture {
        portal,
        cmds,
        tx,
        monitors,
        edges: Vec::new(),
        ids: HashMap::new(),
        open: true,
        enabled: false,
        stale: false,
        active: None,
        released_by_us: false,
        backoff: Backoff::default(),
        retry_at: None,
        opened_at: Instant::now(),
    };
    loop {
        let retry_at = c.retry_at;
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
        }
    }
}

impl<P: CapturePortal> Capture<P> {
    async fn set_edges(&mut self, new: Vec<Edge>) {
        if new == self.edges && self.enabled {
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
            self.end_active();
            match self.portal.open().await {
                Ok(m) => {
                    self.enabled = false;
                    self.stale = false;
                    self.opened_at = Instant::now();
                    *self.monitors.lock().expect("poisoned") = m;
                    self.take_queued_edges();
                }
                Err(e) => {
                    self.open = false;
                    self.open_failed(e, "re-creating the capture session failed");
                    return;
                }
            }
        }
        self.apply_edges().await;
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
        if self.edges.is_empty() {
            return;
        }
        match self.portal.set_barriers(&self.edges).await {
            Ok(m) => self.ids = m,
            Err(e) => {
                warn!("setting pointer barriers failed: {e:#}");
                return;
            }
        }
        match self.portal.enable().await {
            Ok(()) => self.enabled = true,
            Err(e) => warn!("enabling input capture failed: {e:#}"),
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
                let Some(side) = barrier.and_then(|b| self.ids.get(&b).copied()) else {
                    // Activated without one of our barriers: give control back.
                    let _ = self.portal.release(activation_id, None).await;
                    return;
                };
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
                warn!(
                    "input capture was disabled by the compositor; a new session is created on the next layout change"
                );
                self.enabled = false;
                self.stale = true;
                self.end_active();
            }
            CapEvent::ZonesChanged => match self.portal.zones().await {
                Ok(m) => {
                    *self.monitors.lock().expect("poisoned") = m.clone();
                    let _ = self.tx.send(LocalEvent::MonitorsChanged(m));
                }
                Err(e) => warn!("reading the capture zones failed: {e:#}"),
            },
            CapEvent::Input(le) => {
                if self.active.is_some() {
                    let _ = self.tx.send(le);
                }
            }
        }
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
        self.ids.clear();
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
            warn!(
                "input capture permission refused in the GNOME dialog: this machine cannot control other machines \
                 until flow-agent is restarted (it can still be controlled)"
            );
        } else {
            warn!("{what}: {e:#}");
            self.schedule_retry();
        }
    }

    async fn reopen(&mut self) {
        match self.portal.open().await {
            Ok(m) => {
                info!("input capture session re-created");
                self.open = true;
                self.opened_at = Instant::now();
                self.take_queued_edges();
                let changed = *self.monitors.lock().expect("poisoned") != m;
                *self.monitors.lock().expect("poisoned") = m.clone();
                if changed {
                    let _ = self.tx.send(LocalEvent::MonitorsChanged(m));
                }
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
        async fn set_barriers(&mut self, edges: &[Edge]) -> Result<HashMap<u32, Side>> {
            self.note(format!("barriers {}", edges.len()));
            Ok(edges.iter().enumerate().map(|(i, e)| (i as u32 + 1, e.side)).collect())
        }
        async fn enable(&mut self) -> Result<()> {
            self.note("enable");
            Ok(())
        }
        async fn release(&mut self, _: Option<u32>, _: Option<(f64, f64)>) -> Result<()> {
            self.note("release");
            Ok(())
        }
        async fn zones(&mut self) -> Result<Vec<Monitor>> {
            Ok(monitor())
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
    }

    impl Rig {
        /// Starts [`run_capture`] on an open session; returns the session's
        /// event sender.
        fn start() -> (Rig, mpsc::UnboundedSender<CapEvent>) {
            let (sessions, sessions_rx) = mpsc::unbounded_channel();
            let (first, first_rx) = mpsc::unbounded_channel();
            let log = Arc::new(Mutex::new(Vec::new()));
            let portal = FakePortal { sessions: sessions_rx, current: Some(first_rx), log: log.clone() };
            let (cmds, cmds_rx) = mpsc::unbounded_channel();
            let (tx, events) = mpsc::unbounded_channel();
            tokio::task::spawn_local(run_capture(portal, cmds_rx, tx, Arc::new(Mutex::new(monitor()))));
            (Rig { cmds, sessions, events, log }, first)
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

    #[tokio::test(start_paused = true)]
    async fn capture_ends_and_session_is_recreated_when_the_ei_stream_closes() {
        local(async {
            let (mut rig, first) = Rig::start();
            rig.cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
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
