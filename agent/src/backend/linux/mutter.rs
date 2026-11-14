//! The "Mutter direct" path of the Wayland backend: GNOME's compositor
//! (mutter) exports its own D-Bus interfaces on the session bus,
//! `org.gnome.Mutter.InputCapture` and `org.gnome.Mutter.RemoteDesktop`.
//! xdg-desktop-portal-gnome drives exactly these behind the InputCapture
//! and RemoteDesktop portals, after its permission dialog. Calling them
//! directly shows no dialog: mutter only checks that later calls on a
//! session come from the connection that created it, and closes a session
//! when that connection goes away. So any program in the user's session
//! may use them; they are private to GNOME ("no compatibility between
//! versions is promised" in their XML), may change in a future GNOME, and
//! the backend falls back to the portals when they are missing or fail.
//!
//! Everything below follows mutter 46 (`meta-input-capture-session.c`,
//! `meta-remote-desktop-session.c`) and how xdg-desktop-portal-gnome 46
//! (`inputcapture.c`, `remotedesktop.c`) calls it.
//!
//! **Input capture.** `CreateSession(capabilities)` → session object;
//! `GetZones` → (serial, `a(uuii)` width, height, x, y per logical
//! monitor); `AddBarrier(serial, (x1, y1, x2, y2))` → mutter's barrier id
//! (only before `Enable`, only for the current serial); `Enable`;
//! `ConnectToEIS` → the libei receiver socket; signals `Activated(barrier,
//! activation, (x, y))`, `Deactivated`, `ZonesChanged`, `Closed`.
//! On a monitor change mutter bumps the serial and disables the session
//! (barriers destroyed, and the EIS seat and devices dropped) before it
//! sends `ZonesChanged`; it never sends `Disabled`. A disabled session can
//! be enabled again with new barriers, but its EI client then has no seat
//! any more, and mutter refuses a second EI client while the first one is
//! connected: captured input would not flow. So the capture loop
//! re-creates the session (`Close`, `CreateSession`), as it does with the
//! portal, only here without a dialog.
//!
//! **Remote control.** `CreateSession` → session; `Start`;
//! `EnableClipboard`; `ConnectToEIS({device-types})` → the libei sender
//! socket. Without a ScreenCast session (the portal opens none either when
//! no screen is selected) mutter gives the EI client one shared absolute
//! pointer with a region per logical monitor in global layout coordinates
//! (`add_logical_monitor_viewports`), updated on monitor changes: absolute
//! motion is as accurate as through the portal. `Stop` closes the session.

use super::wayland::{barrier_for, capture_ei_event, rects_to_monitors, BarrierSet, CapEvent, CapturePortal};
use anyhow::{anyhow, bail, Context, Result};
use ashpd::zbus::{self, Connection, Message, OwnedObjectPath, Proxy, Variant};
use flow_layout::Edge;
use flow_proto::{Monitor, Rect};
use futures_util::stream::LocalBoxStream;
use futures_util::StreamExt;
use reis::ei;
use std::collections::HashMap;
use std::future::Future;
use std::os::unix::net::UnixStream;
use std::time::Duration;
use tracing::{debug, info, warn};

pub const IC_NAME: &str = "org.gnome.Mutter.InputCapture";
pub const IC_PATH: &str = "/org/gnome/Mutter/InputCapture";
pub const IC_SESSION: &str = "org.gnome.Mutter.InputCapture.Session";
pub const RD_NAME: &str = "org.gnome.Mutter.RemoteDesktop";
pub const RD_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
pub const RD_SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";

/// `MetaInputCaptureCapabilities`: keyboard | pointer.
const IC_CAPS: u32 = 1 | 2;
/// `MetaRemoteDesktopDeviceTypes`: keyboard | pointer (no touchscreen).
pub const RD_DEVICES: u32 = 1 | 2;

/// Upper bound for one call; mutter answers at once (no dialog).
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) async fn call<T>(what: &str, f: impl Future<Output = zbus::Result<T>>) -> Result<T> {
    match tokio::time::timeout(CALL_TIMEOUT, f).await {
        Ok(r) => r.with_context(|| format!("mutter {what}")),
        Err(_) => Err(anyhow!("mutter {what}: no answer within {CALL_TIMEOUT:?}")),
    }
}

/// Which session bus the sessions live on (tests use a private one).
#[derive(Clone, Debug)]
pub enum MutterBus {
    Session,
    #[cfg_attr(not(test), allow(dead_code))]
    Address(String),
}

impl MutterBus {
    /// A new connection: a mutter session belongs to the connection that
    /// created it, and dropping the connection closes it for good.
    pub async fn connect(&self) -> Result<Connection> {
        match self {
            MutterBus::Session => Connection::session().await.context("connecting to the D-Bus session bus"),
            MutterBus::Address(a) => Ok(zbus::connection::Builder::address(a.as_str()).build().await?),
        }
    }
}

/// Whether `name` has an owner on the bus of `conn`.
pub async fn on_bus(conn: &Connection, name: &str) -> Result<bool> {
    let dbus = zbus::fdo::DBusProxy::new(conn).await?;
    let name = zbus::names::BusName::try_from(name.to_string())?;
    call("NameHasOwner", async { Ok(dbus.name_has_owner(name).await?) }).await
}

/// A proxy without the property cache (mutter's session objects have no
/// properties worth a round trip, and zbus would fetch them first).
pub async fn proxy(conn: &Connection, name: &str, path: impl Into<String>, iface: &str) -> Result<Proxy<'static>> {
    let path = OwnedObjectPath::try_from(path.into())?;
    Ok(zbus::proxy::Builder::<Proxy>::new(conn)
        .destination(name.to_string())
        .path(path)
        .interface(iface.to_string())
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?)
}

/// `CreateSession` on mutter's InputCapture (connection, session proxy).
pub async fn create_capture_session(bus: &MutterBus) -> Result<(Connection, Proxy<'static>)> {
    let conn = bus.connect().await?;
    if !on_bus(&conn, IC_NAME).await? {
        bail!("{IC_NAME} is not on the session bus");
    }
    let ic = proxy(&conn, IC_NAME, IC_PATH, IC_NAME).await?;
    let path: OwnedObjectPath = call("InputCapture.CreateSession", ic.call("CreateSession", &(IC_CAPS,))).await?;
    let session = proxy(&conn, IC_NAME, path.as_str(), IC_SESSION).await?;
    Ok((conn, session))
}

/// `ConnectToEIS` result as an EI context on a non-blocking socket.
pub fn ei_context(fd: zbus::OwnedFd) -> Result<ei::Context> {
    let stream = UnixStream::from(std::os::fd::OwnedFd::from(fd));
    stream.set_nonblocking(true)?;
    Ok(ei::Context::new(stream)?)
}

/// `GetZones` → (serial, monitors).
pub async fn zones(session: &Proxy<'static>) -> Result<(u32, Vec<Monitor>)> {
    let (serial, zones): (u32, Vec<(u32, u32, i32, i32)>) =
        call("InputCapture.Session.GetZones", session.call("GetZones", &())).await?;
    let rects = zones.into_iter().map(|(w, h, x, y)| Rect::new(x, y, w as i32, h as i32));
    Ok((serial, rects_to_monitors(rects)))
}

/// Raw signal stream of a session proxy.
async fn signal(p: &Proxy<'static>, name: &'static str) -> Result<LocalBoxStream<'static, Message>> {
    Ok(p.receive_signal(name).await.with_context(|| format!("subscribing to {name}"))?.boxed_local())
}

/// One live mutter InputCapture session.
struct Live {
    /// Its own connection (see [`MutterBus::connect`]).
    _conn: Connection,
    session: Proxy<'static>,
    ctx: ei::Context,
    events: reis::tokio::EiConvertEventStream,
    zone_set: u32,
    activated: LocalBoxStream<'static, Message>,
    deactivated: LocalBoxStream<'static, Message>,
    zones_changed: LocalBoxStream<'static, Message>,
    closed: LocalBoxStream<'static, Message>,
}

/// [`CapturePortal`] on mutter's own InputCapture interface.
pub struct MutterCapture {
    bus: MutterBus,
    live: Option<Live>,
    scroll_acc: (f32, f32),
}

impl MutterCapture {
    pub fn new(bus: MutterBus) -> Self {
        MutterCapture { bus, live: None, scroll_acc: (0.0, 0.0) }
    }

    fn live(&mut self) -> Result<&mut Live> {
        self.live.as_mut().ok_or_else(|| anyhow!("no mutter InputCapture session"))
    }
}

/// True when mutter refused a call because the zone serial is not the
/// current one ("State out of date"): nothing set on it can work.
fn out_of_date(e: &zbus::Error) -> bool {
    matches!(e, zbus::Error::MethodError(name, ..) if name.as_str() == "org.freedesktop.DBus.Error.BadAddress")
}

impl CapturePortal for MutterCapture {
    fn asks_permission(&self) -> bool {
        false
    }

    fn label(&self) -> &'static str {
        "mutter direct"
    }

    async fn open(&mut self) -> Result<Vec<Monitor>> {
        self.close().await;
        let (conn, session) = create_capture_session(&self.bus).await?;
        // Subscribe before anything can happen on the session.
        let activated = signal(&session, "Activated").await?;
        let deactivated = signal(&session, "Deactivated").await?;
        let zones_changed = signal(&session, "ZonesChanged").await?;
        let closed = signal(&session, "Closed").await?;
        let fd = call("InputCapture.Session.ConnectToEIS", session.call("ConnectToEIS", &())).await?;
        let ctx = ei_context(fd)?;
        let (_conn, events) = ctx
            .handshake_tokio("flowcursor-capture", ei::handshake::ContextType::Receiver)
            .await
            .map_err(|e| anyhow!("EI handshake: {e:?}"))?;
        let (zone_set, mons) = zones(&session).await?;
        info!(session = %session.path(), zone_set, zones = mons.len(), "mutter input capture session created");
        self.scroll_acc = (0.0, 0.0);
        self.live =
            Some(Live { _conn: conn, session, ctx, events, zone_set, activated, deactivated, zones_changed, closed });
        Ok(mons)
    }

    async fn close(&mut self) {
        if let Some(l) = self.live.take() {
            // Disable fails on a session that is not enabled: harmless.
            let _ = call("InputCapture.Session.Disable", l.session.call::<_, _, ()>("Disable", &())).await;
            if let Err(e) = call("InputCapture.Session.Close", l.session.call::<_, _, ()>("Close", &())).await {
                debug!("{e:#}");
            }
            // Dropping the connection closes it in any case.
            info!("mutter input capture session closed");
        }
    }

    async fn set_barriers(&mut self, edges: &[Edge]) -> Result<BarrierSet> {
        let l = self.live()?;
        // Barriers of an earlier attempt on this session (enable failed).
        call("InputCapture.Session.ClearBarriers", l.session.call::<_, _, ()>("ClearBarriers", &())).await?;
        let mut set = BarrierSet { zone_set: l.zone_set, ..Default::default() };
        for e in edges {
            let pos = barrier_for(e).as_tuple();
            match tokio::time::timeout(CALL_TIMEOUT, l.session.call::<_, _, u32>("AddBarrier", &(l.zone_set, pos)))
                .await
            {
                Ok(Ok(id)) => {
                    set.ids.insert(id, e.side);
                }
                Ok(Err(err)) if out_of_date(&err) => {
                    return Err(anyhow!("mutter AddBarrier: {err} (zone set {})", l.zone_set));
                }
                Ok(Err(err)) => {
                    warn!(?pos, "mutter refused a pointer barrier (it must lie on an outer screen edge): {err}");
                    set.refused += 1;
                }
                Err(_) => bail!("mutter AddBarrier: no answer within {CALL_TIMEOUT:?}"),
            }
        }
        info!(barriers = set.ids.len(), zone_set = set.zone_set, "pointer barriers set");
        Ok(set)
    }

    async fn enable(&mut self) -> Result<()> {
        let l = self.live()?;
        call("InputCapture.Session.Enable", l.session.call::<_, _, ()>("Enable", &())).await
    }

    async fn disable(&mut self) -> Result<()> {
        let l = self.live()?;
        call("InputCapture.Session.Disable", l.session.call::<_, _, ()>("Disable", &())).await
    }

    async fn release(&mut self, _activation_id: Option<u32>, pos: Option<(f64, f64)>) -> Result<()> {
        let l = self.live()?;
        let mut opts: HashMap<&str, Variant> = HashMap::new();
        if let Some(p) = pos {
            opts.insert("cursor_position", Variant::from(p));
        }
        call("InputCapture.Session.Release", l.session.call::<_, _, ()>("Release", &(opts,))).await
    }

    async fn zones(&mut self) -> Result<(u32, Vec<Monitor>)> {
        let l = self.live()?;
        let (serial, mons) = zones(&l.session).await?;
        l.zone_set = serial;
        Ok((serial, mons))
    }

    async fn next_event(&mut self) -> Option<CapEvent> {
        let l = self.live.as_mut()?;
        loop {
            tokio::select! {
                m = l.activated.next() => {
                    let m = m?;
                    match m.body().deserialize::<(u32, u32, (f64, f64))>() {
                        Ok((barrier, activation, pos)) => {
                            self.scroll_acc = (0.0, 0.0);
                            return Some(CapEvent::Activated { activation_id: Some(activation), barrier: Some(barrier), pos });
                        }
                        Err(e) => warn!("unexpected InputCapture.Session.Activated signal: {e}"),
                    }
                }
                m = l.deactivated.next() => { m?; return Some(CapEvent::Deactivated); }
                m = l.zones_changed.next() => { m?; return Some(CapEvent::ZonesChanged); }
                m = l.closed.next() => {
                    m?;
                    warn!("mutter closed the input capture session");
                    return None;
                }
                ev = l.events.next() => match capture_ei_event(ev, &l.ctx, &mut self.scroll_acc) {
                    Ok(Some(ev)) => return Some(ev),
                    Ok(None) => {}
                    Err(()) => return None,
                },
            }
        }
    }
}
