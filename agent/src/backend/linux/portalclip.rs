//! The clipboard of one RemoteDesktop session, driven by its raw D-Bus
//! signals: either the portal Clipboard interface
//! (`org.freedesktop.portal.Clipboard`) or, on the Mutter direct path (see
//! [`super::mutter`]), the clipboard methods of mutter's own
//! `org.gnome.Mutter.RemoteDesktop.Session`, which the portal backend
//! forwards to. Both behave the same; only the call and signal shapes
//! differ (the portal names the session in every call and signal, mutter's
//! session object is the session).
//!
//! ashpd's typed `receive_selection_owner_changed` cannot be used with
//! GNOME: xdg-desktop-portal-gnome (gnome-46 `clipboard.c`) only renames
//! mutter's `mime-types` key to `mime_types` and keeps mutter's type, a
//! struct holding the list (`(as)`), where the portal specification says
//! `as`. ashpd's deserialization of the whole signal then fails and its
//! stream drops the signal without a trace, so a local copy was never seen.
//! The signals are therefore parsed here, accepting both forms (and
//! mutter's dashed key names), and only for this session.
//!
//! A copy in a GNOME app arrives as a burst (mutter sends one signal per
//! owner change, the first often with no MIME types yet): the newest offer
//! within [`OFFER_SETTLE`] is read once, offers without a usable MIME type
//! are ignored.

use super::{PNG_MIME, TEXT_MIMES};
use crate::backend::{EventTx, LocalEvent};
use anyhow::{anyhow, Context, Result};
use ashpd::desktop::clipboard::{Clipboard, SetSelectionOptions};
use ashpd::zbus::{Message, OwnedObjectPath, OwnedVariant, Proxy, Variant};
use flow_proto::ClipContent;
use futures_util::stream::LocalBoxStream;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;
use tracing::{debug, warn};

/// How long the newest offer of a burst of `SelectionOwnerChanged` signals
/// is collected before it is read.
pub const OFFER_SETTLE: Duration = Duration::from_millis(60);
/// Upper bound for one portal call (no dialog is involved).
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest clipboard content read from or written to the portal; the core
/// applies the (smaller) configured limits.
const MAX_CONTENT: u64 = 64 << 20;

/// What the clipboard of the session needs next.
#[derive(Debug, PartialEq)]
pub enum ClipWork {
    /// Another application owns the clipboard: read it as `mime`.
    Read(String),
    /// An application pastes what this session put on the clipboard.
    Transfer { mime: String, serial: u32 },
    /// A signal stream ended (the bus connection is gone).
    Ended(&'static str),
}

/// Which interface the clipboard of the session is reached through.
enum Api {
    /// `org.freedesktop.portal.Clipboard`: calls and signals name the
    /// session handle.
    Portal { proxy: Clipboard, session: OwnedObjectPath },
    /// `org.gnome.Mutter.RemoteDesktop.Session`: a proxy for the session
    /// object itself. mutter emits the signals on the session's path (to
    /// every listener), so the proxy's match rule already selects them.
    Mutter { proxy: Proxy<'static> },
}

/// The clipboard of one RemoteDesktop session.
pub struct PortalClip {
    api: Api,
    owner_changes: LocalBoxStream<'static, Message>,
    transfers: LocalBoxStream<'static, Message>,
    /// Newest offer not read yet and when to read it.
    offer: Option<(String, Instant)>,
    /// What this session put on the clipboard (served on transfers).
    owned: Option<ClipContent>,
}

async fn bounded<T, E: Into<anyhow::Error>>(what: &str, f: impl Future<Output = Result<T, E>>) -> Result<T> {
    match tokio::time::timeout(CALL_TIMEOUT, f).await {
        Ok(r) => r.map_err(Into::into),
        Err(_) => Err(anyhow!("{what}: no answer from the portal within {CALL_TIMEOUT:?}")),
    }
}

impl PortalClip {
    /// Subscribes to the clipboard signals of `session` on the proxy's
    /// connection (the one the session was created on: the portal sends
    /// them to that connection only). An error means no portal clipboard.
    pub async fn subscribe(proxy: Clipboard, session: OwnedObjectPath) -> Result<Self> {
        let owner_changes = proxy
            .receive_signal("SelectionOwnerChanged")
            .await
            .context("subscribing to Clipboard.SelectionOwnerChanged")?
            .boxed_local();
        let transfers =
            proxy.receive_signal("SelectionTransfer").await.context("subscribing to Clipboard.SelectionTransfer")?;
        Ok(PortalClip {
            api: Api::Portal { proxy, session },
            owner_changes,
            transfers: transfers.boxed_local(),
            offer: None,
            owned: None,
        })
    }

    /// Subscribes to the clipboard signals of a mutter RemoteDesktop
    /// session (`proxy` is for the session object). The session's
    /// `EnableClipboard` is called afterwards by its owner: mutter announces
    /// the current owner right away, and that signal must not be missed.
    pub async fn mutter(proxy: Proxy<'static>) -> Result<Self> {
        let owner_changes = proxy
            .receive_signal("SelectionOwnerChanged")
            .await
            .context("subscribing to RemoteDesktop.Session.SelectionOwnerChanged")?
            .boxed_local();
        let transfers = proxy
            .receive_signal("SelectionTransfer")
            .await
            .context("subscribing to RemoteDesktop.Session.SelectionTransfer")?
            .boxed_local();
        Ok(PortalClip { api: Api::Mutter { proxy }, owner_changes, transfers, offer: None, owned: None })
    }

    /// The session's object path (the portal's handle or mutter's session).
    fn session(&self) -> &str {
        match &self.api {
            Api::Portal { session, .. } => session.as_str(),
            Api::Mutter { proxy } => proxy.path().as_str(),
        }
    }

    /// `SelectionOwnerChanged` of this session, None for another one.
    fn owner_change(&self, m: &Message) -> Result<Option<OwnerChange>> {
        let c = match self.api {
            Api::Portal { .. } => parse_owner_changed(m)?,
            Api::Mutter { .. } => parse_mutter_owner_changed(m)?,
        };
        if c.session.as_str() != self.session() {
            debug!(path = %c.session, "clipboard owner change for another session ignored");
            return Ok(None);
        }
        Ok(Some(c))
    }

    /// `SelectionTransfer` of this session as (mime, serial).
    fn transfer_of(&self, m: &Message) -> Result<Option<(String, u32)>> {
        Ok(match self.api {
            Api::Portal { .. } => {
                let (path, mime, serial) = parse_transfer(m)?;
                if path.as_str() != self.session() {
                    debug!(%path, "clipboard transfer for another session ignored");
                    return Ok(None);
                }
                Some((mime, serial))
            }
            Api::Mutter { .. } => {
                if m.header().path().map(|p| p.as_str()) != Some(self.session()) {
                    return Ok(None);
                }
                Some(m.body().deserialize::<(String, u32)>().context("body is not (su)")?)
            }
        })
    }

    /// The next thing to do. Cancel-safe: the pending offer is kept in
    /// `self` and the streams keep unread signals.
    pub async fn next(&mut self) -> ClipWork {
        loop {
            let read_at = self.offer.as_ref().map(|o| o.1);
            tokio::select! {
                m = self.owner_changes.next() => match m {
                    Some(m) => self.on_owner_changed(&m),
                    None => return ClipWork::Ended("SelectionOwnerChanged"),
                },
                m = self.transfers.next() => match m {
                    Some(m) => match self.transfer_of(&m) {
                        Ok(Some((mime, serial))) => return ClipWork::Transfer { mime, serial },
                        Ok(None) => {}
                        Err(e) => warn!("unexpected SelectionTransfer signal: {e:#}"),
                    },
                    None => return ClipWork::Ended("SelectionTransfer"),
                },
                _ = tokio::time::sleep_until(read_at.unwrap_or_else(Instant::now)), if read_at.is_some() => {
                    if let Some((mime, _)) = self.offer.take() {
                        return ClipWork::Read(mime);
                    }
                }
            }
        }
    }

    fn on_owner_changed(&mut self, m: &Message) {
        let change = match self.owner_change(m) {
            Ok(Some(c)) => c,
            Ok(None) => return,
            Err(e) => {
                warn!("unexpected SelectionOwnerChanged signal: {e:#}");
                return;
            }
        };
        debug!(mime_types = ?change.mime_types, session_is_owner = ?change.session_is_owner, "clipboard owner changed");
        if change.session_is_owner == Some(true) {
            // What we set ourselves (from another machine): nothing to read.
            self.offer = None;
            return;
        }
        let Some(mime) = pick_mime(&change.mime_types) else {
            // The first signal of a burst lists no types yet; others offer
            // nothing we share.
            return;
        };
        // The newest offer wins; the first one of a burst sets the time.
        let at = self.offer.as_ref().map(|o| o.1).unwrap_or_else(|| Instant::now() + OFFER_SETTLE);
        self.offer = Some((mime, at));
    }

    /// Reads the clipboard as `mime` and reports it as
    /// [`LocalEvent::Clipboard`] (empty content is not reported). Only the
    /// portal call is awaited: the pipe is drained on a blocking thread so
    /// a slow source does not hold up injection.
    pub async fn read(&self, mime: &str, events: &EventTx) -> Result<()> {
        let fd: ashpd::zbus::OwnedFd = match &self.api {
            Api::Portal { proxy, session } => {
                bounded("Clipboard.SelectionRead", proxy.call("SelectionRead", &(session, mime))).await
            }
            Api::Mutter { proxy } => bounded("SelectionRead", proxy.call("SelectionRead", &(mime,))).await,
        }
        .with_context(|| format!("reading the clipboard as {mime}"))?;
        let fd = std::os::fd::OwnedFd::from(fd);
        let (mime, events) = (mime.to_string(), events.clone());
        tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut buf = Vec::new();
            if let Err(e) = std::fs::File::from(fd).take(MAX_CONTENT).read_to_end(&mut buf) {
                warn!("reading the clipboard as {mime} failed: {e}");
                return;
            }
            if buf.is_empty() {
                debug!(mime, "the clipboard was empty");
                return;
            }
            debug!(mime, bytes = buf.len(), "local clipboard read");
            let c = if mime == PNG_MIME {
                ClipContent::Image { png: buf }
            } else {
                ClipContent::Text(String::from_utf8_lossy(&buf).into_owned())
            };
            let _ = events.send(LocalEvent::Clipboard(c));
        });
        Ok(())
    }

    /// Puts `c` on the clipboard (from another machine); applications
    /// that paste it ask for it with a transfer.
    pub async fn set(&mut self, c: ClipContent) -> Result<()> {
        let mimes: Vec<&str> = match &c {
            ClipContent::Text(_) => TEXT_MIMES.to_vec(),
            ClipContent::Image { .. } => vec![PNG_MIME],
        };
        self.owned = Some(c);
        // A newer offer of another application loses against this one.
        self.offer = None;
        match &self.api {
            Api::Portal { proxy, session } => {
                let opts = SetSelectionOptions::default().set_mime_types(&mimes);
                bounded("Clipboard.SetSelection", proxy.call::<_, _, ()>("SetSelection", &(session, opts))).await
            }
            Api::Mutter { proxy } => {
                // mutter reads the list as `as` under its own key name.
                let opts = HashMap::from([("mime-types", Variant::from(mimes))]);
                bounded("SetSelection", proxy.call::<_, _, ()>("SetSelection", &(opts,))).await
            }
        }
    }

    /// Answers a transfer: writes what this session owns as `mime`.
    pub async fn transfer(&self, mime: &str, serial: u32) {
        let data: Option<Vec<u8>> = match &self.owned {
            Some(ClipContent::Text(t)) if TEXT_MIMES.contains(&mime) => Some(t.as_bytes().to_vec()),
            Some(ClipContent::Image { png }) if mime == PNG_MIME => Some(png.clone()),
            _ => None,
        };
        let ok = match data {
            Some(d) => match self.write(serial, d).await {
                Ok(()) => true,
                Err(e) => {
                    warn!("clipboard transfer as {mime} failed: {e:#}");
                    false
                }
            },
            None => {
                debug!(mime, "clipboard transfer asked for content this machine does not hold");
                false
            }
        };
        let done = match &self.api {
            Api::Portal { proxy, session } => {
                bounded("SelectionWriteDone", proxy.call::<_, _, ()>("SelectionWriteDone", &(session, serial, ok)))
                    .await
            }
            Api::Mutter { proxy } => {
                bounded("SelectionWriteDone", proxy.call::<_, _, ()>("SelectionWriteDone", &(serial, ok))).await
            }
        };
        if let Err(e) = done {
            debug!("SelectionWriteDone failed: {e:#}");
        }
    }

    async fn write(&self, serial: u32, data: Vec<u8>) -> Result<()> {
        let fd: ashpd::zbus::OwnedFd = match &self.api {
            Api::Portal { proxy, session } => {
                bounded("Clipboard.SelectionWrite", proxy.call("SelectionWrite", &(session, serial))).await?
            }
            Api::Mutter { proxy } => bounded("SelectionWrite", proxy.call("SelectionWrite", &(serial,))).await?,
        };
        let fd = std::os::fd::OwnedFd::from(fd);
        // A reader that never drains the pipe must not park the caller:
        // stop waiting (the blocking write is abandoned).
        bounded(
            "writing the clipboard pipe",
            tokio::task::spawn_blocking(move || {
                use std::io::Write;
                std::fs::File::from(fd).write_all(&data)
            }),
        )
        .await??;
        Ok(())
    }
}

/// A parsed `SelectionOwnerChanged`.
#[derive(Debug, PartialEq)]
pub struct OwnerChange {
    pub session: OwnedObjectPath,
    pub mime_types: Vec<String>,
    pub session_is_owner: Option<bool>,
}

/// Parses `SelectionOwnerChanged (o session_handle, a{sv} options)`.
/// `mime_types` may be `as` (portal specification) or `(as)` (GNOME);
/// mutter's own key names (`mime-types`, `session-is-owner`) are accepted
/// too.
pub fn parse_owner_changed(m: &Message) -> Result<OwnerChange> {
    let (session, options): (OwnedObjectPath, HashMap<String, OwnedVariant>) =
        m.body().deserialize().context("body is not (oa{sv})")?;
    Ok(owner_change(session, &options))
}

/// Parses mutter's `SelectionOwnerChanged (a{sv} options)`, emitted on the
/// session's path; mutter 46 sends `mime-types` as `(as)` and
/// `session-is-owner` (both only when there is an owner).
pub fn parse_mutter_owner_changed(m: &Message) -> Result<OwnerChange> {
    let (options,): (HashMap<String, OwnedVariant>,) = m.body().deserialize().context("body is not (a{sv})")?;
    let path = m.header().path().map(|p| p.to_string()).unwrap_or_default();
    let session = OwnedObjectPath::try_from(path).map_err(|e| anyhow!("signal without a path: {e}"))?;
    Ok(owner_change(session, &options))
}

fn owner_change(session: OwnedObjectPath, options: &HashMap<String, OwnedVariant>) -> OwnerChange {
    let get = |a: &str, b: &str| options.get(a).or_else(|| options.get(b));
    let mut mime_types = Vec::new();
    if let Some(v) = get("mime_types", "mime-types") {
        collect_strings(v, &mut mime_types);
    }
    let session_is_owner = match get("session_is_owner", "session-is-owner").map(|v| &**v) {
        Some(Variant::Bool(b)) => Some(*b),
        Some(Variant::Variant(v)) => match **v {
            Variant::Bool(b) => Some(b),
            _ => None,
        },
        _ => None,
    };
    OwnerChange { session, mime_types, session_is_owner }
}

/// Every string in `v`, looking into arrays, structs and variants.
fn collect_strings(v: &Variant<'_>, out: &mut Vec<String>) {
    match v {
        Variant::Str(s) => out.push(s.as_str().to_string()),
        Variant::Array(a) => a.inner().iter().for_each(|v| collect_strings(v, out)),
        Variant::Structure(s) => s.fields().iter().for_each(|v| collect_strings(v, out)),
        Variant::Variant(v) => collect_strings(v, out),
        _ => {}
    }
}

/// Parses `SelectionTransfer (o session_handle, s mime_type, u serial)`.
fn parse_transfer(m: &Message) -> Result<(OwnedObjectPath, String, u32)> {
    m.body().deserialize().context("body is not (osu)")
}

/// The MIME type to read from an offer: PNG first, then the preferred text
/// type; None when nothing in it is shared.
pub fn pick_mime(offered: &[String]) -> Option<String> {
    if offered.iter().any(|m| m == PNG_MIME) {
        return Some(PNG_MIME.to_string());
    }
    TEXT_MIMES.iter().find(|t| offered.iter().any(|m| m == *t)).map(|s| s.to_string())
}

/// The object path of an ashpd session (its `path()` is crate-private; the
/// session serializes as its path).
pub fn session_path<T: ashpd::desktop::SessionPortal>(s: &ashpd::desktop::Session<T>) -> Result<OwnedObjectPath> {
    let path = serde_json::to_value(s)?;
    let path = path.as_str().ok_or_else(|| anyhow!("a portal session did not serialize as its path"))?;
    OwnedObjectPath::try_from(path.to_string()).map_err(|e| anyhow!("invalid session path {path:?}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ashpd::zbus::{Connection, MessageStream, Structure};
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    const SESSION_1: &str = "/org/freedesktop/portal/desktop/session/1_42/flowcursor1";
    const SESSION_2: &str = "/org/freedesktop/portal/desktop/session/1_42/flowcursor2";
    const UTF8: &str = "text/plain;charset=utf-8";

    fn opts(mimes: Option<Variant<'static>>, owner: Option<bool>) -> HashMap<&'static str, Variant<'static>> {
        let mut o = HashMap::new();
        if let Some(m) = mimes {
            o.insert("mime_types", m);
        }
        if let Some(b) = owner {
            o.insert("session_is_owner", Variant::Bool(b));
        }
        o
    }

    /// `(as)`, as xdg-desktop-portal-gnome sends the list.
    fn gnome_mimes(m: &[&str]) -> Variant<'static> {
        let list: Vec<String> = m.iter().map(|s| s.to_string()).collect();
        Variant::Structure(Structure::from((list,)))
    }

    /// `as`, as the portal specification describes it.
    fn spec_mimes(m: &[&str]) -> Variant<'static> {
        Variant::new(m.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    fn owner_signal(path: &str, o: &HashMap<&str, Variant<'_>>) -> Message {
        Message::signal("/org/freedesktop/portal/desktop", "org.freedesktop.portal.Clipboard", "SelectionOwnerChanged")
            .build(&(ashpd::zbus::ObjectPath::try_from(path).unwrap(), o))
            .unwrap()
    }

    #[test]
    fn owner_changed_parses_the_gnome_and_the_spec_forms() {
        let gnome = owner_signal(SESSION_1, &opts(Some(gnome_mimes(&[UTF8, "text/plain"])), Some(false)));
        let c = parse_owner_changed(&gnome).unwrap();
        assert_eq!(c.session.as_str(), SESSION_1);
        assert_eq!(c.mime_types, [UTF8, "text/plain"]);
        assert_eq!(c.session_is_owner, Some(false));
        let spec = owner_signal(SESSION_1, &opts(Some(spec_mimes(&[PNG_MIME])), None));
        let c = parse_owner_changed(&spec).unwrap();
        assert_eq!((c.mime_types.as_slice(), c.session_is_owner), ([PNG_MIME.to_string()].as_slice(), None));
        // mutter's key names, and a signal without any types.
        let mut dashed = HashMap::new();
        dashed.insert("mime-types", gnome_mimes(&["text/plain"]));
        dashed.insert("session-is-owner", Variant::Bool(true));
        let c = parse_owner_changed(&owner_signal(SESSION_1, &dashed)).unwrap();
        assert_eq!((c.mime_types.as_slice(), c.session_is_owner), (["text/plain".to_string()].as_slice(), Some(true)));
        let c = parse_owner_changed(&owner_signal(SESSION_1, &opts(None, Some(false)))).unwrap();
        assert!(c.mime_types.is_empty());
        assert_eq!(pick_mime(&c.mime_types), None);
    }

    /// mutter 46 `generate_owner_changed_variant`: `{"mime-types": <(as)>,
    /// "session-is-owner": <b>}` emitted on the session's path, or an empty
    /// dict without an owner.
    #[test]
    fn mutter_owner_changed_takes_the_session_from_the_path() {
        const PATH: &str = "/org/gnome/Mutter/RemoteDesktop/Session/u4";
        let signal = |o: &HashMap<&str, Variant<'_>>| {
            Message::signal(PATH, "org.gnome.Mutter.RemoteDesktop.Session", "SelectionOwnerChanged")
                .build(&(o,))
                .unwrap()
        };
        let mut o = HashMap::new();
        o.insert("mime-types", gnome_mimes(&[UTF8, "STRING"]));
        o.insert("session-is-owner", Variant::Bool(false));
        let c = parse_mutter_owner_changed(&signal(&o)).unwrap();
        assert_eq!(c.session.as_str(), PATH);
        assert_eq!(c.mime_types, [UTF8, "STRING"]);
        assert_eq!(c.session_is_owner, Some(false));
        let c = parse_mutter_owner_changed(&signal(&HashMap::new())).unwrap();
        assert!(c.mime_types.is_empty() && c.session_is_owner.is_none());
        // The portal's shape is not mutter's.
        assert!(parse_mutter_owner_changed(&owner_signal(SESSION_1, &opts(None, None))).is_err());
    }

    /// The root cause of the silent clipboard:ashpd's typed signal cannot
    /// read GNOME's `(as)` list. If this starts to pass, ashpd accepts it
    /// and its stream could be used again.
    #[test]
    fn ashpd_cannot_parse_gnomes_owner_changed() {
        let gnome = owner_signal(SESSION_1, &opts(Some(gnome_mimes(&[UTF8])), Some(false)));
        type Typed = (OwnedObjectPath, ashpd::desktop::clipboard::SelectionOwnerChanged);
        assert!(gnome.body().deserialize::<Typed>().is_err(), "ashpd now parses GNOME's SelectionOwnerChanged");
        let spec = owner_signal(SESSION_1, &opts(Some(spec_mimes(&[UTF8])), Some(false)));
        assert!(spec.body().deserialize::<Typed>().is_ok());
    }

    #[test]
    fn mime_preference() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(pick_mime(&s(&["text/plain", UTF8])), Some(UTF8.into()));
        assert_eq!(pick_mime(&s(&["text/html", PNG_MIME, UTF8])), Some(PNG_MIME.into()));
        assert_eq!(pick_mime(&s(&["text/html", "application/x-foo"])), None);
    }

    use crate::backend::linux::testbus::Bus;

    /// What the fake portal was asked.
    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Read { session: String, mime: String },
        Set { session: String },
        Write { session: String, serial: u32 },
        WriteDone { serial: u32, ok: bool },
    }

    /// xdg-desktop-portal's Clipboard on the private bus: answers the calls
    /// the agent makes and sends signals to one client connection only,
    /// like the real portal (`g_dbus_connection_emit_signal` to the
    /// session's sender).
    struct FakePortal {
        conn: Connection,
        calls: Arc<Mutex<Vec<Call>>>,
        /// What a `SelectionRead` returns.
        content: Arc<Mutex<Vec<u8>>>,
        /// Data written for transfers.
        written: mpsc::UnboundedReceiver<Vec<u8>>,
    }

    impl FakePortal {
        async fn start(bus: &Bus) -> FakePortal {
            let conn = bus.connect().await;
            conn.request_name("org.freedesktop.portal.Desktop").await.unwrap();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let content = Arc::new(Mutex::new(b"from gnome".to_vec()));
            let (written_tx, written) = mpsc::unbounded_channel();
            let (c, calls2, content2) = (conn.clone(), calls.clone(), content.clone());
            tokio::task::spawn_local(async move {
                let mut incoming = MessageStream::from(&c);
                while let Some(Ok(m)) = incoming.next().await {
                    if m.message_type() != ashpd::zbus::message::Type::MethodCall {
                        continue;
                    }
                    let hdr = m.header();
                    let member = hdr.member().map(|m| m.as_str().to_string()).unwrap_or_default();
                    let note = |call| calls2.lock().unwrap().push(call);
                    let r = match member.as_str() {
                        // ashpd reads the interface version when it builds a
                        // proxy (zbus fetches all properties for its cache).
                        "Get" => c.reply(&hdr, &Variant::U32(1)).await,
                        "GetAll" => c.reply(&hdr, &HashMap::from([("version", Variant::U32(1))])).await,
                        "SelectionRead" => {
                            let (session, mime): (OwnedObjectPath, String) = m.body().deserialize().unwrap();
                            note(Call::Read { session: session.to_string(), mime });
                            let (rd, mut wr) = std::io::pipe().unwrap();
                            std::io::Write::write_all(&mut wr, &content2.lock().unwrap()).unwrap();
                            drop(wr);
                            c.reply(&hdr, &ashpd::zbus::Fd::from(std::os::fd::OwnedFd::from(rd))).await
                        }
                        "SetSelection" => {
                            let (session, _): (OwnedObjectPath, HashMap<String, OwnedVariant>) =
                                m.body().deserialize().unwrap();
                            note(Call::Set { session: session.to_string() });
                            c.reply(&hdr, &()).await
                        }
                        "SelectionWrite" => {
                            let (session, serial): (OwnedObjectPath, u32) = m.body().deserialize().unwrap();
                            note(Call::Write { session: session.to_string(), serial });
                            let (rd, wr) = std::io::pipe().unwrap();
                            let tx = written_tx.clone();
                            std::thread::spawn(move || {
                                let mut buf = Vec::new();
                                let _ = std::io::Read::read_to_end(&mut { rd }, &mut buf);
                                let _ = tx.send(buf);
                            });
                            c.reply(&hdr, &ashpd::zbus::Fd::from(std::os::fd::OwnedFd::from(wr))).await
                        }
                        "SelectionWriteDone" => {
                            let (_, serial, ok): (OwnedObjectPath, u32, bool) = m.body().deserialize().unwrap();
                            note(Call::WriteDone { serial, ok });
                            c.reply(&hdr, &()).await
                        }
                        other => {
                            c.reply_error(&hdr, "org.freedesktop.DBus.Error.UnknownMethod", &other.to_string()).await
                        }
                    };
                    r.unwrap();
                }
            });
            FakePortal { conn, calls, content, written }
        }

        async fn emit(&self, to: &Connection, member: &str, body: &(impl serde::Serialize + ashpd::zbus::DynamicType)) {
            let dest = to.unique_name().unwrap().to_owned();
            self.conn
                .emit_signal(
                    Some(dest),
                    "/org/freedesktop/portal/desktop",
                    "org.freedesktop.portal.Clipboard",
                    member,
                    body,
                )
                .await
                .unwrap();
        }

        /// The burst GNOME sends for one copy in a GNOME app.
        async fn copy_burst(&self, to: &Connection, session: &str) {
            let path = ashpd::zbus::ObjectPath::try_from(session).unwrap();
            for o in [
                opts(None, Some(false)),
                opts(Some(gnome_mimes(&[UTF8])), Some(false)),
                opts(Some(gnome_mimes(&[UTF8, "text/plain", UTF8])), Some(false)),
            ] {
                self.emit(to, "SelectionOwnerChanged", &(&path, o)).await;
            }
        }

        fn take_calls(&self) -> Vec<Call> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }
    }

    /// One RemoteDesktop session of the agent: its own connection (see
    /// `wayland::session_bus`) and the clipboard on it.
    async fn open_session(bus: &Bus, session: &str) -> (Connection, PortalClip) {
        let conn = bus.connect().await;
        let proxy = Clipboard::with_connection(conn.clone()).await.unwrap();
        let clip = PortalClip::subscribe(proxy, OwnedObjectPath::try_from(session).unwrap()).await.unwrap();
        (conn, clip)
    }

    /// Runs the clipboard like the injector loop does until `until` work
    /// items were handled; returns them.
    async fn drive(clip: &mut PortalClip, events: &EventTx, until: usize) -> Vec<ClipWork> {
        let mut done = Vec::new();
        while done.len() < until {
            let w = tokio::time::timeout(Duration::from_secs(5), clip.next()).await.expect("no clipboard work");
            match &w {
                ClipWork::Read(mime) => clip.read(mime, events).await.unwrap(),
                ClipWork::Transfer { mime, serial } => clip.transfer(mime, *serial).await,
                ClipWork::Ended(_) => {}
            }
            done.push(w);
        }
        done
    }

    async fn local<F: Future<Output = ()>>(f: F) {
        tokio::task::LocalSet::new().run_until(f).await
    }

    #[tokio::test]
    async fn a_gnome_copy_burst_is_read_once_also_after_the_session_is_reopened() {
        let Some(bus) = Bus::start() else {
            eprintln!("dbus-daemon not available, skipping");
            return;
        };
        local(async {
            let portal = FakePortal::start(&bus).await;
            let (ev_tx, mut events) = mpsc::unbounded_channel();

            let (conn1, mut clip1) = open_session(&bus, SESSION_1).await;
            portal.copy_burst(&conn1, SESSION_1).await;
            let work = drive(&mut clip1, &ev_tx, 1).await;
            assert_eq!(work, [ClipWork::Read(UTF8.into())]);
            assert_eq!(events.recv().await, Some(LocalEvent::Clipboard(ClipContent::Text("from gnome".into()))));
            assert_eq!(portal.take_calls(), [Call::Read { session: SESSION_1.into(), mime: UTF8.into() }]);

            // The session is closed (its connection with it) and a new one
            // opens on a new connection, like `set_controllable` does.
            drop(clip1);
            drop(conn1);
            let (conn2, mut clip2) = open_session(&bus, SESSION_2).await;
            // A late signal for the old session is ignored; the new
            // session's copies are read.
            let old = ashpd::zbus::ObjectPath::try_from(SESSION_1).unwrap();
            portal.emit(&conn2, "SelectionOwnerChanged", &(&old, opts(Some(gnome_mimes(&[UTF8])), Some(false)))).await;
            *portal.content.lock().unwrap() = b"second copy".to_vec();
            portal.copy_burst(&conn2, SESSION_2).await;
            let work = drive(&mut clip2, &ev_tx, 1).await;
            assert_eq!(work, [ClipWork::Read(UTF8.into())]);
            assert_eq!(events.recv().await, Some(LocalEvent::Clipboard(ClipContent::Text("second copy".into()))));
            assert_eq!(portal.take_calls(), [Call::Read { session: SESSION_2.into(), mime: UTF8.into() }]);

            // What this session set itself is not read back.
            let path = ashpd::zbus::ObjectPath::try_from(SESSION_2).unwrap();
            portal.emit(&conn2, "SelectionOwnerChanged", &(&path, opts(Some(gnome_mimes(&[UTF8])), Some(true)))).await;
            assert!(tokio::time::timeout(OFFER_SETTLE * 5, clip2.next()).await.is_err());
            assert!(portal.take_calls().is_empty());
            assert!(events.try_recv().is_err());
        })
        .await
    }

    #[tokio::test]
    async fn remote_content_is_served_to_a_paste_after_a_reopen() {
        let Some(bus) = Bus::start() else {
            eprintln!("dbus-daemon not available, skipping");
            return;
        };
        local(async {
            let mut portal = FakePortal::start(&bus).await;
            let (ev_tx, _events) = mpsc::unbounded_channel();
            for (n, session) in [SESSION_1, SESSION_2].into_iter().enumerate() {
                let (conn, mut clip) = open_session(&bus, session).await;
                let text = format!("from windows {n}");
                clip.set(ClipContent::Text(text.clone())).await.unwrap();
                let path = ashpd::zbus::ObjectPath::try_from(session).unwrap();
                portal.emit(&conn, "SelectionTransfer", &(&path, UTF8, 7u32 + n as u32)).await;
                let work = drive(&mut clip, &ev_tx, 1).await;
                assert_eq!(work, [ClipWork::Transfer { mime: UTF8.into(), serial: 7 + n as u32 }]);
                assert_eq!(portal.written.recv().await.unwrap(), text.as_bytes());
                assert_eq!(
                    portal.take_calls(),
                    [
                        Call::Set { session: session.into() },
                        Call::Write { session: session.into(), serial: 7 + n as u32 },
                        Call::WriteDone { serial: 7 + n as u32, ok: true },
                    ]
                );
            }
        })
        .await
    }
}
