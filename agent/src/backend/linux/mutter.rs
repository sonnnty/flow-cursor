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

use super::portalclip::PortalClip;
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

/// Whether mutter's RemoteDesktop interface is on the session bus (checked
/// without creating a session, which would show GNOME's indicator).
pub async fn remote_desktop_available(bus: &MutterBus) -> Result<()> {
    let conn = bus.connect().await?;
    if !on_bus(&conn, RD_NAME).await? {
        bail!("{RD_NAME} is not on the session bus");
    }
    Ok(())
}

/// One open mutter RemoteDesktop session.
pub struct MutterRd {
    /// Its own connection: dropping it closes the session in any case.
    _conn: Connection,
    session: Proxy<'static>,
}

impl MutterRd {
    /// `Stop` closes the session (and its EIS); the connection goes too.
    pub async fn close(self) {
        if let Err(e) = call("RemoteDesktop.Session.Stop", self.session.call::<_, _, ()>("Stop", &())).await {
            debug!("{e:#}");
        }
    }
}

/// EI sender context, connection and event stream of a session.
pub type EiSender = (ei::Context, reis::event::Connection, reis::tokio::EiConvertEventStream);

/// Opens a RemoteDesktop session the way xdg-desktop-portal-gnome 46 does
/// after its dialog: `CreateSession`, `Start`, `EnableClipboard`,
/// `ConnectToEIS` with the device types. The clipboard is subscribed
/// before `EnableClipboard`, which announces the current owner at once; it
/// is None when mutter refuses it.
pub async fn open_remote_desktop(bus: &MutterBus) -> Result<(MutterRd, Option<PortalClip>, EiSender)> {
    let conn = bus.connect().await?;
    if !on_bus(&conn, RD_NAME).await? {
        bail!("{RD_NAME} is not on the session bus");
    }
    let rd = proxy(&conn, RD_NAME, RD_PATH, RD_NAME).await?;
    let path: OwnedObjectPath = call("RemoteDesktop.CreateSession", rd.call("CreateSession", &())).await?;
    let session = proxy(&conn, RD_NAME, path.as_str(), RD_SESSION).await?;
    let s = MutterRd { _conn: conn, session: session.clone() };
    let clip = match PortalClip::mutter(session.clone()).await {
        Ok(c) => Some(c),
        Err(e) => {
            warn!("mutter clipboard signals unavailable: {e:#}");
            None
        }
    };
    // A failed Start closes the session on mutter's side.
    call("RemoteDesktop.Session.Start", session.call::<_, _, ()>("Start", &())).await?;
    let clip = match clip {
        Some(c) => {
            let opts: HashMap<&str, Variant> = HashMap::new();
            match call("RemoteDesktop.Session.EnableClipboard", session.call::<_, _, ()>("EnableClipboard", &(opts,)))
                .await
            {
                Ok(()) => Some(c),
                Err(e) => {
                    info!("mutter did not enable the clipboard for this session: {e:#}");
                    None
                }
            }
        }
        None => None,
    };
    let eis = async {
        let opts = HashMap::from([("device-types", Variant::from(RD_DEVICES))]);
        let fd = call("RemoteDesktop.Session.ConnectToEIS", session.call("ConnectToEIS", &(opts,))).await?;
        let ctx = ei_context(fd)?;
        let (conn, events) = ctx
            .handshake_tokio("flowcursor-inject", ei::handshake::ContextType::Sender)
            .await
            .map_err(|e| anyhow!("EI handshake: {e:?}"))?;
        anyhow::Ok((ctx, conn, events))
    };
    match eis.await {
        Ok(eis) => {
            info!(session = %session.path(), clipboard = clip.is_some(), "mutter remote desktop session open");
            Ok((s, clip, eis))
        }
        Err(e) => {
            s.close().await;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::linux::portalclip::ClipWork;
    use crate::backend::linux::testbus::Bus;
    use crate::backend::linux::wayland::{
        mutter_capture, mutter_injection, run_capture, CapCmd, CaptureShared, DisplayTick,
    };
    use crate::backend::linux::TEXT_MIMES;
    use crate::backend::{EventRx, LocalEvent};
    use ashpd::zbus::{MessageStream, OwnedVariant, Structure};
    use flow_layout::Side;
    use flow_proto::{ClipContent, NodeId};
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    const UTF8: &str = "text/plain;charset=utf-8";
    const ZONE: (u32, u32, i32, i32) = (1920, 1080, 0, 0);

    /// mutter's EIS for one client: the server side of the handshake, then
    /// reading until the client goes away (no seat or devices: the tests
    /// below are about the D-Bus side).
    fn fake_eis(sock: UnixStream) {
        std::thread::spawn(move || {
            let Ok(ctx) = reis::eis::Context::new(sock) else { return };
            let mut handshake = Some(reis::handshake::EisHandshaker::new(&ctx, 0));
            loop {
                match ctx.read() {
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
                while let Some(r) = ctx.pending_request() {
                    let reis::PendingRequestResult::Request(r) = r else { return };
                    if let Some(h) = handshake.as_mut() {
                        match h.handle_request(r) {
                            Ok(Some(_)) => {
                                handshake = None;
                                let _ = ctx.flush();
                            }
                            Ok(None) => {}
                            Err(_) => return,
                        }
                    }
                }
            }
        });
    }

    #[derive(Default)]
    struct IcSession {
        owner: String,
        enabled: bool,
        barriers: u32,
    }

    #[derive(Default)]
    struct State {
        /// Every call on a session, as "<session> <Method> [details]", and
        /// "ic create <session>" / "rd create <session>".
        log: Vec<String>,
        serial: u32,
        zones: Vec<(u32, u32, i32, i32)>,
        ic: HashMap<String, IcSession>,
        next: u32,
        /// CreateSession fails (e.g. remote access inhibited).
        refuse: bool,
        /// Another application's clipboard: MIME types and content.
        clip: Option<(Vec<String>, Vec<u8>)>,
    }

    /// mutter 46's InputCapture and RemoteDesktop on the private bus, as
    /// far as the agent uses them: sessions belong to their creator's
    /// connection, barriers need the current zone serial and a session that
    /// is not enabled, a monitor change bumps the serial, disables every
    /// session and emits ZonesChanged, EnableClipboard announces the
    /// current owner with `mime-types` as `(as)`.
    struct FakeMutter {
        conn: Connection,
        state: Arc<Mutex<State>>,
        written: mpsc::UnboundedReceiver<Vec<u8>>,
    }

    fn short(path: &str) -> String {
        path.rsplit('/').next().unwrap_or_default().to_string()
    }

    fn eis_fd() -> zbus::Fd<'static> {
        let (ours, theirs) = UnixStream::pair().unwrap();
        fake_eis(ours);
        zbus::Fd::from(std::os::fd::OwnedFd::from(theirs))
    }

    impl FakeMutter {
        async fn start(bus: &Bus) -> FakeMutter {
            let conn = bus.connect().await;
            conn.request_name(IC_NAME).await.unwrap();
            conn.request_name(RD_NAME).await.unwrap();
            let state = Arc::new(Mutex::new(State { serial: 1, zones: vec![ZONE], ..Default::default() }));
            let (written_tx, written) = mpsc::unbounded_channel();
            let (c, st) = (conn.clone(), state.clone());
            tokio::task::spawn_local(async move {
                let mut incoming = MessageStream::from(&c);
                while let Some(Ok(m)) = incoming.next().await {
                    if m.message_type() != zbus::message::Type::MethodCall {
                        continue;
                    }
                    serve(&c, &st, &written_tx, &m).await;
                }
            });
            FakeMutter { conn, state, written }
        }

        fn take_log(&self) -> Vec<String> {
            std::mem::take(&mut self.state.lock().unwrap().log)
        }

        /// Waits until `entry` was logged; returns the log up to it.
        async fn wait_for(&self, entry: &str) -> Vec<String> {
            for _ in 0..1000 {
                if self.state.lock().unwrap().log.iter().any(|l| l == entry) {
                    return self.take_log();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("{entry:?} never happened; log: {:?}", self.state.lock().unwrap().log);
        }

        async fn emit(
            &self,
            session: &str,
            iface: &str,
            member: &str,
            body: &(impl serde::Serialize + zbus::DynamicType),
        ) {
            let path = format!("{}/Session/{session}", if iface == IC_SESSION { IC_PATH } else { RD_PATH });
            self.conn.emit_signal(None::<&str>, path.as_str(), iface, member, body).await.unwrap();
        }

        /// A monitor change, as mutter's `on_monitors_changed`.
        async fn change_monitors(&self, zones: Vec<(u32, u32, i32, i32)>) {
            let paths: Vec<String> = {
                let mut st = self.state.lock().unwrap();
                st.serial += 1;
                st.zones = zones;
                st.ic.iter_mut().for_each(|(_, s)| s.enabled = false);
                st.ic.keys().cloned().collect()
            };
            for p in paths {
                self.emit(&short(&p), IC_SESSION, "ZonesChanged", &()).await;
            }
        }
    }

    /// Answers one method call.
    async fn serve(c: &Connection, st: &Mutex<State>, written: &mpsc::UnboundedSender<Vec<u8>>, m: &Message) {
        let hdr = m.header();
        let path = hdr.path().map(|p| p.to_string()).unwrap_or_default();
        let member = hdr.member().map(|m| m.to_string()).unwrap_or_default();
        let sender = hdr.sender().map(|s| s.to_string()).unwrap_or_default();
        let s = short(&path);
        let note = |l: String| st.lock().unwrap().log.push(l);
        let hdr = &hdr;
        let deny = |msg: &'static str| async move {
            c.reply_error(hdr, "org.freedesktop.DBus.Error.AccessDenied", &msg).await.unwrap();
        };
        if member == "CreateSession" {
            let refuse = st.lock().unwrap().refuse;
            if refuse {
                return c
                    .reply_error(hdr, "org.freedesktop.DBus.Error.Failed", &"Session creation inhibited")
                    .await
                    .unwrap();
            }
            let n = {
                let mut st = st.lock().unwrap();
                st.next += 1;
                st.next
            };
            let (kind, base) = if path == IC_PATH { ("ic", IC_PATH) } else { ("rd", RD_PATH) };
            let new = format!("{base}/Session/u{n}");
            if kind == "ic" {
                st.lock().unwrap().ic.insert(new.clone(), IcSession { owner: sender, ..Default::default() });
            }
            note(format!("{kind} create u{n}"));
            return c.reply(hdr, &OwnedObjectPath::try_from(new).unwrap()).await.unwrap();
        }
        if path.starts_with(IC_PATH) {
            if st.lock().unwrap().ic.get(&path).map(|i| i.owner != sender).unwrap_or(true) {
                return deny("Permission denied").await;
            }
            note(format!("{s} {member}"));
            let r = match member.as_str() {
                "GetZones" => {
                    let body = {
                        let st = st.lock().unwrap();
                        (st.serial, st.zones.clone())
                    };
                    c.reply(hdr, &body).await
                }
                "ClearBarriers" => {
                    st.lock().unwrap().ic.get_mut(&path).unwrap().barriers = 0;
                    c.reply(hdr, &()).await
                }
                "AddBarrier" => {
                    let (serial, pos): (u32, (i32, i32, i32, i32)) = m.body().deserialize().unwrap();
                    st.lock().unwrap().log.last_mut().unwrap().push_str(&format!(" {pos:?}"));
                    // Err: out of date (true) or enabled (false).
                    let id = {
                        let mut g = st.lock().unwrap();
                        let current = g.serial;
                        let sess = g.ic.get_mut(&path).unwrap();
                        if serial != current {
                            Err(true)
                        } else if sess.enabled {
                            Err(false)
                        } else {
                            sess.barriers += 1;
                            Ok(sess.barriers)
                        }
                    };
                    match id {
                        Ok(id) => c.reply(hdr, &id).await,
                        Err(true) => {
                            c.reply_error(hdr, "org.freedesktop.DBus.Error.BadAddress", &"State out of date").await
                        }
                        Err(false) => return deny("Session already enabled").await,
                    }
                }
                "Enable" => {
                    st.lock().unwrap().ic.get_mut(&path).unwrap().enabled = true;
                    c.reply(hdr, &()).await
                }
                "Disable" => {
                    let was = std::mem::replace(&mut st.lock().unwrap().ic.get_mut(&path).unwrap().enabled, false);
                    if was {
                        c.reply(hdr, &()).await
                    } else {
                        c.reply_error(hdr, "org.freedesktop.DBus.Error.Failed", &"Session not enabled").await
                    }
                }
                "Release" => {
                    let (opts,): (HashMap<String, OwnedVariant>,) = m.body().deserialize().unwrap();
                    let pos = opts.get("cursor_position").and_then(|v| <(f64, f64)>::try_from(&**v).ok());
                    st.lock().unwrap().log.last_mut().unwrap().push_str(&format!(" {pos:?}"));
                    c.reply(hdr, &()).await
                }
                "ConnectToEIS" => c.reply(hdr, &eis_fd()).await,
                "Close" => {
                    st.lock().unwrap().ic.remove(&path);
                    c.reply(hdr, &()).await.unwrap();
                    c.emit_signal(None::<&str>, path.as_str(), IC_SESSION, "Closed", &()).await
                }
                _ => c.reply_error(hdr, "org.freedesktop.DBus.Error.UnknownMethod", &member).await,
            };
            return r.unwrap();
        }
        note(format!("{s} {member}"));
        let r = match member.as_str() {
            "Start" | "SelectionWriteDone" => {
                if member == "SelectionWriteDone" {
                    let (serial, ok): (u32, bool) = m.body().deserialize().unwrap();
                    st.lock().unwrap().log.last_mut().unwrap().push_str(&format!(" {serial} {ok}"));
                }
                c.reply(hdr, &()).await
            }
            "Stop" => {
                c.reply(hdr, &()).await.unwrap();
                c.emit_signal(None::<&str>, path.as_str(), RD_SESSION, "Closed", &()).await
            }
            "EnableClipboard" => {
                // mutter announces the current owner during the call.
                let clip = st.lock().unwrap().clip.clone();
                if let Some((mimes, _)) = clip {
                    let opts = HashMap::from([
                        ("mime-types", Variant::Structure(Structure::from((mimes,)))),
                        ("session-is-owner", Variant::Bool(false)),
                    ]);
                    c.emit_signal(None::<&str>, path.as_str(), RD_SESSION, "SelectionOwnerChanged", &(opts,))
                        .await
                        .unwrap();
                }
                c.reply(hdr, &()).await
            }
            "SetSelection" => {
                let (opts,): (HashMap<String, OwnedVariant>,) = m.body().deserialize().unwrap();
                // mutter wants a plain `as`.
                let mimes: Option<Vec<String>> = match opts.get("mime-types").map(|v| &**v) {
                    Some(Variant::Array(a)) => Some(
                        a.inner()
                            .iter()
                            .filter_map(|v| match v {
                                Variant::Str(s) => Some(s.to_string()),
                                _ => None,
                            })
                            .collect(),
                    ),
                    _ => None,
                };
                st.lock().unwrap().log.last_mut().unwrap().push_str(&format!(" {mimes:?}"));
                c.reply(hdr, &()).await.unwrap();
                let opts = HashMap::from([
                    ("mime-types", Variant::Structure(Structure::from((mimes.unwrap_or_default(),)))),
                    ("session-is-owner", Variant::Bool(true)),
                ]);
                c.emit_signal(None::<&str>, path.as_str(), RD_SESSION, "SelectionOwnerChanged", &(opts,)).await
            }
            "SelectionRead" => {
                let (mime,): (String,) = m.body().deserialize().unwrap();
                st.lock().unwrap().log.last_mut().unwrap().push_str(&format!(" {mime}"));
                let data = st.lock().unwrap().clip.clone().map(|c| c.1).unwrap_or_default();
                let (rd, mut wr) = std::io::pipe().unwrap();
                std::io::Write::write_all(&mut wr, &data).unwrap();
                drop(wr);
                c.reply(hdr, &zbus::Fd::from(std::os::fd::OwnedFd::from(rd))).await
            }
            "SelectionWrite" => {
                let (serial,): (u32,) = m.body().deserialize().unwrap();
                st.lock().unwrap().log.last_mut().unwrap().push_str(&format!(" {serial}"));
                let (rd, wr) = std::io::pipe().unwrap();
                let tx = written.clone();
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let _ = std::io::Read::read_to_end(&mut { rd }, &mut buf);
                    let _ = tx.send(buf);
                });
                c.reply(hdr, &zbus::Fd::from(std::os::fd::OwnedFd::from(wr))).await
            }
            "ConnectToEIS" => {
                let (opts,): (HashMap<String, OwnedVariant>,) = m.body().deserialize().unwrap();
                let types = opts.get("device-types").and_then(|v| u32::try_from(&**v).ok());
                st.lock().unwrap().log.last_mut().unwrap().push_str(&format!(" {types:?}"));
                c.reply(hdr, &eis_fd()).await
            }
            _ => c.reply_error(hdr, "org.freedesktop.DBus.Error.UnknownMethod", &member).await,
        };
        r.unwrap();
    }

    async fn local<F: Future<Output = ()>>(f: F) {
        tokio::task::LocalSet::new().run_until(f).await
    }

    async fn next(events: &mut EventRx) -> LocalEvent {
        tokio::time::timeout(Duration::from_secs(10), events.recv()).await.expect("no event").unwrap()
    }

    fn right_edge() -> Vec<Edge> {
        vec![Edge { side: Side::Right, pos: 1920, start: 0, end: 1080, to: NodeId(2) }]
    }

    /// Starts the capture loop on mutter's session like `start_capture`.
    async fn start_capture(bus: &Bus) -> (mpsc::UnboundedSender<CapCmd>, EventRx, Arc<Mutex<CaptureShared>>) {
        let (cap, mons) = mutter_capture(true, &MutterBus::Address(bus.addr.clone())).await.expect("mutter direct");
        assert_eq!(mons, rects_to_monitors([Rect::new(0, 0, 1920, 1080)].into_iter()));
        let (cmds, cmds_rx) = mpsc::unbounded_channel();
        let (tx, events) = mpsc::unbounded_channel();
        let report = Arc::new(Mutex::new(CaptureShared::default()));
        // No monitor hints but the compositor's own ZonesChanged.
        let display = futures_util::stream::pending::<DisplayTick>().boxed_local();
        let monitors = Arc::new(Mutex::new(mons));
        tokio::task::spawn_local(run_capture(cap, cmds_rx, tx, monitors, display, report.clone()));
        (cmds, events, report)
    }

    const BARRIER: &str = " (1920, 0, 1920, 1079)";

    #[tokio::test]
    async fn capture_is_rebuilt_without_any_dialog_after_a_monitor_change() {
        let Some(bus) = Bus::start() else {
            eprintln!("dbus-daemon not available, skipping");
            return;
        };
        local(async {
            let fake = FakeMutter::start(&bus).await;
            let (cmds, mut events, report) = start_capture(&bus).await;
            assert_eq!(fake.take_log(), ["ic create u1", "u1 ConnectToEIS", "u1 GetZones"]);
            cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
            let log = fake.wait_for("u1 Enable").await;
            assert_eq!(log, ["u1 GetZones", "u1 ClearBarriers", &format!("u1 AddBarrier{BARRIER}"), "u1 Enable"]);

            // A barrier hit, and the release that enters where the
            // pointer comes back.
            fake.emit("u1", IC_SESSION, "Activated", &(1u32, 1u32, (1920.0f64, 500.0f64))).await;
            assert_eq!(next(&mut events).await, LocalEvent::EdgeHit { side: Side::Right, x: 1920, y: 500 });
            cmds.send(CapCmd::Release(Some((1900, 500)))).unwrap();
            assert_eq!(fake.wait_for("u1 Release Some((1900.0, 500.0))").await.len(), 1);
            fake.emit("u1", IC_SESSION, "Deactivated", &(1u32,)).await;

            // mutter disables the session with a monitor change: a new
            // session with the same barriers, nothing else.
            fake.change_monitors(vec![ZONE]).await;
            let log = fake.wait_for("u2 Enable").await;
            assert_eq!(
                log,
                [
                    "u1 GetZones",
                    "u1 Disable",
                    "u1 Close",
                    "ic create u2",
                    "u2 ConnectToEIS",
                    "u2 GetZones",
                    "u2 GetZones",
                    "u2 ClearBarriers",
                    &format!("u2 AddBarrier{BARRIER}"),
                    "u2 Enable",
                ]
            );
            let r = report.lock().unwrap().report();
            assert_eq!((r.path.as_str(), r.state.as_str(), r.barriers, r.sessions), ("mutter direct", "enabled", 1, 2));
            assert!(events.try_recv().is_err(), "nothing was captured: nothing to end");

            // mutter's cancel shortcut: deactivated without a release of
            // ours. Capture ends at once, the session is rebuilt.
            fake.emit("u2", IC_SESSION, "Activated", &(1u32, 2u32, (1920.0f64, 10.0f64))).await;
            assert!(matches!(next(&mut events).await, LocalEvent::EdgeHit { .. }));
            fake.emit("u2", IC_SESSION, "Deactivated", &(2u32,)).await;
            assert_eq!(next(&mut events).await, LocalEvent::CaptureEnded);
            let log = fake.wait_for("u3 Enable").await;
            assert!(log.contains(&"u2 Close".to_string()) && log.contains(&"ic create u3".to_string()), "{log:?}");
        })
        .await
    }

    #[tokio::test]
    async fn a_closed_session_is_replaced() {
        let Some(bus) = Bus::start() else {
            eprintln!("dbus-daemon not available, skipping");
            return;
        };
        local(async {
            let fake = FakeMutter::start(&bus).await;
            let (cmds, _events, _report) = start_capture(&bus).await;
            cmds.send(CapCmd::SetEdges(right_edge())).unwrap();
            fake.wait_for("u1 Enable").await;
            // E.g. remote access turned off in GNOME's settings.
            fake.emit("u1", IC_SESSION, "Closed", &()).await;
            let log = fake.wait_for("u2 Enable").await;
            assert!(log.contains(&"ic create u2".to_string()), "{log:?}");
        })
        .await
    }

    #[tokio::test]
    async fn remote_desktop_session_and_clipboard_both_ways() {
        let Some(bus) = Bus::start() else {
            eprintln!("dbus-daemon not available, skipping");
            return;
        };
        local(async {
            let mut fake = FakeMutter::start(&bus).await;
            fake.state.lock().unwrap().clip = Some((vec![UTF8.into(), "text/plain".into()], b"from gnome".to_vec()));
            let (rd, clip, _eis) = open_remote_desktop(&MutterBus::Address(bus.addr.clone())).await.unwrap();
            assert_eq!(fake.take_log(), ["rd create u1", "u1 Start", "u1 EnableClipboard", "u1 ConnectToEIS Some(3)"]);
            let mut clip = clip.expect("clipboard enabled");
            let (ev_tx, mut events) = mpsc::unbounded_channel();

            // GNOME → other machines: the owner EnableClipboard announced.
            let w = tokio::time::timeout(Duration::from_secs(5), clip.next()).await.unwrap();
            assert_eq!(w, ClipWork::Read(UTF8.into()));
            clip.read(UTF8, &ev_tx).await.unwrap();
            assert_eq!(next(&mut events).await, LocalEvent::Clipboard(ClipContent::Text("from gnome".into())));
            assert_eq!(fake.take_log(), [format!("u1 SelectionRead {UTF8}")]);

            // Other machines → GNOME: offered, then pasted. mutter's echo
            // of our own offer is not read back.
            clip.set(ClipContent::Text("from windows".into())).await.unwrap();
            let mimes: Vec<String> = TEXT_MIMES.iter().map(|s| s.to_string()).collect();
            assert_eq!(fake.take_log(), [format!("u1 SetSelection Some({mimes:?})")]);
            fake.emit("u1", RD_SESSION, "SelectionTransfer", &(UTF8, 7u32)).await;
            let w = tokio::time::timeout(Duration::from_secs(5), clip.next()).await.unwrap();
            assert_eq!(w, ClipWork::Transfer { mime: UTF8.into(), serial: 7 });
            clip.transfer(UTF8, 7).await;
            assert_eq!(fake.written.recv().await.unwrap(), b"from windows");
            assert_eq!(fake.take_log(), ["u1 SelectionWrite 7", "u1 SelectionWriteDone 7 true"]);

            rd.close().await;
            assert_eq!(fake.take_log(), ["u1 Stop"]);
        })
        .await
    }

    #[tokio::test]
    async fn the_portal_is_used_without_mutter_when_it_fails_or_when_turned_off() {
        let Some(bus) = Bus::start() else {
            eprintln!("dbus-daemon not available, skipping");
            return;
        };
        local(async {
            let addr = MutterBus::Address(bus.addr.clone());
            // KDE and others: mutter's names are not on the bus.
            assert!(mutter_capture(true, &addr).await.is_none());
            assert!(!mutter_injection(true, &addr).await);
            assert!(open_remote_desktop(&addr).await.is_err());

            let fake = FakeMutter::start(&bus).await;
            // gnome_direct = "off": not even tried.
            assert!(mutter_capture(false, &addr).await.is_none());
            assert!(!mutter_injection(false, &addr).await);
            // Checking for RemoteDesktop opens no session (and no indicator).
            assert!(mutter_injection(true, &addr).await);
            assert!(fake.take_log().is_empty());
            // CreateSession fails: the portal again.
            fake.state.lock().unwrap().refuse = true;
            assert!(mutter_capture(true, &addr).await.is_none());
            assert!(open_remote_desktop(&addr).await.is_err());
            assert!(fake.take_log().is_empty());
        })
        .await
    }
}
