//! Clipboard through X11 selections (CLIPBOARD), used by the X11 backend
//! and, through XWayland, as the fallback of the Wayland backend (mutter
//! keeps the X11 CLIPBOARD selection in sync with the Wayland clipboard).
//!
//! Runs on its own thread with its own connection and an invisible window.
//! Changes are detected with XFixes selection notifications; text is read
//! as UTF8_STRING, images as image/png (INCR transfers are supported when
//! reading). When we own the selection we answer TARGETS, text and PNG
//! requests with a single property write (up to the server's maximum
//! request size, ~16 MiB with BIG-REQUESTS).

use super::{PNG_MIME, TEXT_MIMES};
use crate::backend::{EventTx, LocalEvent};
use anyhow::{Context, Result};
use flow_proto::ClipContent;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property, SelectionNotifyEvent,
    Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

pub struct X11Clipboard {
    tx: mpsc::Sender<ClipContent>,
}

impl X11Clipboard {
    pub fn set(&self, c: &ClipContent) {
        let _ = self.tx.send(c.clone());
    }
}

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    incr: Atom,
    prop: Atom,
    png: Atom,
    utf8: Atom,
    text: Vec<Atom>,
}

fn intern(conn: &RustConnection, name: &str) -> Result<Atom> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

/// Connects to `$DISPLAY` and starts the clipboard thread.
pub fn spawn(events: EventTx) -> Result<X11Clipboard> {
    let (conn, screen) = x11rb::connect(None).context("connecting to the X server ($DISPLAY)")?;
    conn.xfixes_query_version(5, 0)?.reply().context("XFixes extension missing")?;
    let root = conn.setup().roots[screen].root;
    let win = conn.generate_id()?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        root,
        -10,
        -10,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    let atoms = Atoms {
        clipboard: intern(&conn, "CLIPBOARD")?,
        targets: intern(&conn, "TARGETS")?,
        incr: intern(&conn, "INCR")?,
        prop: intern(&conn, "FLOWCURSOR_CLIP")?,
        png: intern(&conn, PNG_MIME)?,
        utf8: intern(&conn, "UTF8_STRING")?,
        text: TEXT_MIMES.iter().map(|m| intern(&conn, m)).collect::<Result<_>>()?,
    };
    conn.xfixes_select_selection_input(win, atoms.clipboard, SelectionEventMask::SET_SELECTION_OWNER)?;
    conn.flush()?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new().name("x11-clipboard".into()).spawn(move || {
        let mut w = Worker { conn, win, atoms, events, owned: None, fetch: None };
        if let Err(e) = w.run(rx) {
            warn!("X11 clipboard thread stopped: {e:#}");
        }
    })?;
    info!("clipboard via X11 selections");
    Ok(X11Clipboard { tx })
}

enum Fetch {
    Targets { started: Instant },
    Data { target: Atom, started: Instant },
    Incr { target: Atom, buf: Vec<u8>, started: Instant },
}

struct Worker {
    conn: RustConnection,
    win: Window,
    atoms: Atoms,
    events: EventTx,
    owned: Option<ClipContent>,
    fetch: Option<Fetch>,
}

impl Worker {
    fn run(&mut self, rx: mpsc::Receiver<ClipContent>) -> Result<()> {
        loop {
            while let Some(ev) = self.conn.poll_for_event()? {
                self.on_event(ev)?;
            }
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(c) => self.take_ownership(c)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
            if let Some(f) = &self.fetch {
                let started = match f {
                    Fetch::Targets { started } | Fetch::Data { started, .. } | Fetch::Incr { started, .. } => *started,
                };
                if started.elapsed() > Duration::from_secs(5) {
                    debug!("clipboard read timed out");
                    self.fetch = None;
                }
            }
        }
    }

    fn take_ownership(&mut self, c: ClipContent) -> Result<()> {
        self.owned = Some(c);
        self.conn.set_selection_owner(self.win, self.atoms.clipboard, CURRENT_TIME)?;
        self.conn.flush()?;
        Ok(())
    }

    fn on_event(&mut self, ev: Event) -> Result<()> {
        match ev {
            Event::XfixesSelectionNotify(e) if e.selection == self.atoms.clipboard => {
                if e.owner != self.win && e.owner != x11rb::NONE {
                    self.owned = None;
                    self.fetch = Some(Fetch::Targets { started: Instant::now() });
                    self.conn.convert_selection(
                        self.win,
                        self.atoms.clipboard,
                        self.atoms.targets,
                        self.atoms.prop,
                        CURRENT_TIME,
                    )?;
                    self.conn.flush()?;
                }
            }
            Event::SelectionNotify(e) => self.on_selection_notify(e)?,
            Event::PropertyNotify(e)
                if e.window == self.win && e.atom == self.atoms.prop && e.state == Property::NEW_VALUE =>
            {
                if let Some(Fetch::Incr { .. }) = &self.fetch {
                    let chunk = self.read_prop()?;
                    if let Some(Fetch::Incr { target, buf, .. }) = &mut self.fetch {
                        if chunk.1.is_empty() {
                            let (target, data) = (*target, std::mem::take(buf));
                            self.fetch = None;
                            self.deliver(target, data);
                        } else {
                            buf.extend_from_slice(&chunk.1);
                        }
                    }
                }
            }
            Event::SelectionRequest(e) => self.on_request(e)?,
            Event::SelectionClear(e) if e.selection == self.atoms.clipboard => {
                self.owned = None;
            }
            _ => {}
        }
        Ok(())
    }

    /// Reads and deletes our transfer property: (type, bytes).
    fn read_prop(&self) -> Result<(Atom, Vec<u8>)> {
        let r = self.conn.get_property(true, self.win, self.atoms.prop, AtomEnum::ANY, 0, u32::MAX / 4)?.reply()?;
        self.conn.flush()?;
        Ok((r.type_, r.value))
    }

    fn on_selection_notify(&mut self, e: SelectionNotifyEvent) -> Result<()> {
        if e.requestor != self.win || e.selection != self.atoms.clipboard {
            return Ok(());
        }
        if e.property == x11rb::NONE {
            self.fetch = None;
            return Ok(());
        }
        match self.fetch.take() {
            Some(Fetch::Targets { .. }) => {
                let (_, raw) = self.read_prop()?;
                let targets: Vec<Atom> = raw.as_chunks::<4>().0.iter().map(|c| u32::from_ne_bytes(*c)).collect();
                let want = if targets.contains(&self.atoms.png) {
                    Some(self.atoms.png)
                } else {
                    self.atoms.text.iter().copied().find(|t| targets.contains(t))
                };
                if let Some(target) = want {
                    self.fetch = Some(Fetch::Data { target, started: Instant::now() });
                    self.conn.convert_selection(
                        self.win,
                        self.atoms.clipboard,
                        target,
                        self.atoms.prop,
                        CURRENT_TIME,
                    )?;
                    self.conn.flush()?;
                }
            }
            Some(Fetch::Data { target, .. }) => {
                let (ty, data) = self.read_prop()?;
                if ty == self.atoms.incr {
                    // Deleting the property (done by read_prop) starts the transfer.
                    self.fetch = Some(Fetch::Incr { target, buf: Vec::new(), started: Instant::now() });
                } else {
                    self.deliver(target, data);
                }
            }
            other => self.fetch = other,
        }
        Ok(())
    }

    fn deliver(&self, target: Atom, data: Vec<u8>) {
        let content = if target == self.atoms.png {
            ClipContent::Image { png: data }
        } else {
            ClipContent::Text(String::from_utf8_lossy(&data).into_owned())
        };
        if !content.is_empty() {
            let _ = self.events.send(LocalEvent::Clipboard(content));
        }
    }

    fn on_request(&mut self, e: xproto::SelectionRequestEvent) -> Result<()> {
        let property = if e.property == x11rb::NONE { e.target } else { e.property };
        let mut ok = false;
        if e.selection == self.atoms.clipboard {
            let max = self.conn.maximum_request_bytes().saturating_sub(64);
            match &self.owned {
                Some(c) if e.target == self.atoms.targets => {
                    let mut t = vec![self.atoms.targets];
                    match c {
                        ClipContent::Text(_) => t.extend(self.atoms.text.iter().copied()),
                        ClipContent::Image { .. } => t.push(self.atoms.png),
                    }
                    self.conn.change_property32(PropMode::REPLACE, e.requestor, property, AtomEnum::ATOM, &t)?;
                    ok = true;
                }
                Some(ClipContent::Text(s)) if self.atoms.text.contains(&e.target) && s.len() <= max => {
                    let ty = if e.target == self.atoms.text[0] { e.target } else { self.atoms.utf8 };
                    self.conn.change_property8(PropMode::REPLACE, e.requestor, property, ty, s.as_bytes())?;
                    ok = true;
                }
                Some(ClipContent::Image { png }) if e.target == self.atoms.png && png.len() <= max => {
                    self.conn.change_property8(PropMode::REPLACE, e.requestor, property, self.atoms.png, png)?;
                    ok = true;
                }
                _ => {}
            }
        }
        let reply = SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if ok { property } else { x11rb::NONE },
        };
        self.conn.send_event(false, e.requestor, EventMask::NO_EVENT, reply)?;
        self.conn.flush()?;
        Ok(())
    }
}
