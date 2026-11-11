//! X11 (Xorg session) backend, pure Rust via x11rb.
//!
//! * Edge detection: XInput2 raw motion events on the root window, then
//!   `QueryPointer`; pushing outward on an edge segment → `EdgeHit`.
//! * Capture: active pointer + keyboard grabs on the root window, cursor
//!   hidden with XFixes and parked in the middle of the primary monitor;
//!   motion comes from raw (accelerated) XI2 deltas, keys/buttons from the
//!   grab.
//! * Injection: XTEST fake input (X keycode = evdev code + 8).
//! * Clipboard: [`super::x11clip`].
//!
//! Needs only access to `$DISPLAY` (no `input` group, no uinput).

use super::x11clip::{self, X11Clipboard};
use crate::backend::{Backend, BackendOptions, EventRx, EventTx, LocalEvent};
use anyhow::{Context, Result};
use flow_layout::{Edge, Side};
use flow_net::link::Action;
use flow_proto::{Button, Capabilities, ClipContent, Monitor, Rect};
use std::sync::{mpsc, Arc, Mutex};
use tokio::sync::mpsc as tmpsc;
use tracing::{debug, info, warn};
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, EventMask, GrabMode, Window};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::CURRENT_TIME;

enum Cmd {
    Begin,
    End(Option<(i32, i32)>),
    Inject(Action),
}

#[derive(Default)]
struct Shared {
    edges: Vec<Edge>,
    monitors: Vec<Monitor>,
    capturing: bool,
}

pub struct X11Backend {
    shared: Arc<Mutex<Shared>>,
    cmd: mpsc::Sender<Cmd>,
    clip: Option<X11Clipboard>,
}

pub async fn create(_opts: &BackendOptions) -> Result<(Arc<dyn Backend>, EventRx)> {
    let (tx, rx) = tmpsc::unbounded_channel();
    let (conn, screen) = x11rb::connect(None).context("connecting to the X server ($DISPLAY)")?;
    let conn = Arc::new(conn);
    let root = conn.setup().roots[screen].root;
    conn.xinput_xi_query_version(2, 2)?.reply().context("XInput2 ≥ 2.2 required")?;
    conn.xtest_get_version(2, 2)?.reply().context("XTEST extension required")?;
    conn.xfixes_query_version(5, 0)?.reply().context("XFixes extension required")?;
    conn.xinput_xi_select_events(
        root,
        &[xinput::EventMask { deviceid: 1, mask: vec![xinput::XIEventMask::RAW_MOTION] }],
    )?;
    conn.randr_select_input(root, x11rb::protocol::randr::NotifyMask::SCREEN_CHANGE)?;
    conn.flush()?;
    let shared = Arc::new(Mutex::new(Shared { monitors: read_monitors(&conn, root)?, ..Default::default() }));

    let (cmd_tx, cmd_rx) = mpsc::channel();
    {
        let (conn, shared, tx) = (conn.clone(), shared.clone(), tx.clone());
        std::thread::Builder::new().name("x11-events".into()).spawn(move || {
            if let Err(e) = event_loop(&conn, root, &shared, &tx) {
                warn!("X11 event loop stopped: {e:#}");
                let _ = tx.send(LocalEvent::CaptureEnded);
            }
        })?;
    }
    {
        let (conn, shared) = (conn.clone(), shared.clone());
        std::thread::Builder::new().name("x11-commands".into()).spawn(move || {
            let mut w = Commands { conn, root, shared, wheel: (0, 0), parked: (0, 0) };
            while let Ok(c) = cmd_rx.recv() {
                if let Err(e) = w.handle(c) {
                    warn!("X11 command failed: {e:#}");
                }
            }
        })?;
    }
    let clip = match x11clip::spawn(tx.clone()) {
        Ok(c) => Some(c),
        Err(e) => {
            warn!("X11 clipboard unavailable: {e:#}");
            None
        }
    };
    info!("X11 backend ready (XInput2 + XTEST)");
    Ok((Arc::new(X11Backend { shared, cmd: cmd_tx, clip }), rx))
}

fn read_monitors(conn: &RustConnection, root: Window) -> Result<Vec<Monitor>> {
    let r = conn.randr_get_monitors(root, true)?.reply()?;
    let mut out = Vec::new();
    for m in r.monitors {
        let name = conn
            .get_atom_name(m.name)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| String::from_utf8_lossy(&r.name).into_owned())
            .unwrap_or_else(|| "monitor".into());
        out.push(Monitor {
            name,
            rect: Rect::new(m.x as i32, m.y as i32, m.width as i32, m.height as i32),
            primary: m.primary,
        });
    }
    if out.is_empty() {
        let s = &conn.setup().roots[0];
        out.push(Monitor {
            name: "screen".into(),
            rect: Rect::new(0, 0, s.width_in_pixels as i32, s.height_in_pixels as i32),
            primary: true,
        });
    }
    Ok(out)
}

fn fp3232(v: &xinput::Fp3232) -> f64 {
    v.integral as f64 + v.frac as f64 / 4_294_967_296.0
}

/// Relative motion (dx, dy) of a raw event (valuators 0 and 1).
fn raw_delta(e: &xinput::RawMotionEvent) -> (f64, f64) {
    let mut vals = e.axisvalues.iter();
    let mut out = (0.0, 0.0);
    let mask = e.valuator_mask.first().copied().unwrap_or(0);
    for bit in 0..2 {
        if mask & (1 << bit) != 0 {
            let v = vals.next().map(fp3232).unwrap_or(0.0);
            if bit == 0 {
                out.0 = v;
            } else {
                out.1 = v;
            }
        }
    }
    out
}

fn edge_hit(edges: &[Edge], x: i32, y: i32, dx: f64, dy: f64) -> Option<Side> {
    edges.iter().find_map(|e| {
        let (along, hit) = match e.side {
            Side::Right => (y, x >= e.pos - 1 && dx > 0.0),
            Side::Left => (y, x <= e.pos && dx < 0.0),
            Side::Bottom => (x, y >= e.pos - 1 && dy > 0.0),
            Side::Top => (x, y <= e.pos && dy < 0.0),
        };
        (hit && along >= e.start && along < e.end).then_some(e.side)
    })
}

fn x_button(detail: u32) -> Option<Result<Button, (i32, i32)>> {
    Some(match detail {
        1 => Ok(Button::Left),
        2 => Ok(Button::Middle),
        3 => Ok(Button::Right),
        8 => Ok(Button::Back),
        9 => Ok(Button::Forward),
        4 => Err((0, 120)),
        5 => Err((0, -120)),
        6 => Err((-120, 0)),
        7 => Err((120, 0)),
        _ => return None,
    })
}

fn event_loop(conn: &RustConnection, root: Window, shared: &Mutex<Shared>, tx: &EventTx) -> Result<()> {
    loop {
        let ev = conn.wait_for_event()?;
        let capturing = shared.lock().expect("poisoned").capturing;
        match ev {
            Event::XinputRawMotion(e) => {
                let (dx, dy) = raw_delta(&e);
                if capturing {
                    let _ = tx.send(LocalEvent::Motion { dx, dy });
                } else {
                    let p = conn.query_pointer(root)?.reply()?;
                    let (x, y) = (p.root_x as i32, p.root_y as i32);
                    let side = edge_hit(&shared.lock().expect("poisoned").edges, x, y, dx, dy);
                    if let Some(side) = side {
                        let _ = tx.send(LocalEvent::EdgeHit { side, x, y });
                    }
                }
            }
            Event::KeyPress(e) | Event::KeyRelease(e) if capturing => {
                let down = e.response_type & 0x7f == xproto::KEY_PRESS_EVENT;
                let evdev = (e.detail as u16).saturating_sub(8);
                if let Some(usage) = flow_keymap::evdev_to_hid(evdev) {
                    let _ = tx.send(LocalEvent::Key { usage, down });
                }
            }
            Event::ButtonPress(e) | Event::ButtonRelease(e) if capturing => {
                let down = e.response_type & 0x7f == xproto::BUTTON_PRESS_EVENT;
                match x_button(e.detail as u32) {
                    Some(Ok(button)) => {
                        let _ = tx.send(LocalEvent::Button { button, down });
                    }
                    Some(Err((dx, dy))) if down => {
                        let _ = tx.send(LocalEvent::Wheel { dx, dy });
                    }
                    _ => {}
                }
            }
            Event::RandrScreenChangeNotify(_) => {
                let m = read_monitors(conn, root)?;
                shared.lock().expect("poisoned").monitors = m.clone();
                let _ = tx.send(LocalEvent::MonitorsChanged(m));
            }
            _ => {}
        }
    }
}

struct Commands {
    conn: Arc<RustConnection>,
    root: Window,
    shared: Arc<Mutex<Shared>>,
    wheel: (i32, i32),
    parked: (i16, i16),
}

impl Commands {
    fn handle(&mut self, c: Cmd) -> Result<()> {
        let conn = self.conn.clone();
        match c {
            Cmd::Begin => {
                let mask = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION;
                let g = conn
                    .grab_pointer(
                        false,
                        self.root,
                        mask,
                        GrabMode::ASYNC,
                        GrabMode::ASYNC,
                        x11rb::NONE,
                        x11rb::NONE,
                        CURRENT_TIME,
                    )?
                    .reply()?;
                let k =
                    conn.grab_keyboard(false, self.root, CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC)?.reply()?;
                if g.status != xproto::GrabStatus::SUCCESS || k.status != xproto::GrabStatus::SUCCESS {
                    warn!(pointer = ?g.status, keyboard = ?k.status, "X11 grab failed (another app holds a grab?)");
                }
                conn.xfixes_hide_cursor(self.root)?;
                // Park the pointer in the middle of the primary monitor so it
                // cannot hit a screen edge while hidden.
                let m = {
                    let s = self.shared.lock().expect("poisoned");
                    s.monitors.iter().find(|m| m.primary).or(s.monitors.first()).map(|m| m.rect)
                };
                if let Some(r) = m {
                    self.parked = ((r.x + r.w / 2) as i16, (r.y + r.h / 2) as i16);
                    conn.warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, self.parked.0, self.parked.1)?;
                }
                self.shared.lock().expect("poisoned").capturing = true;
                conn.flush()?;
            }
            Cmd::End(warp) => {
                self.shared.lock().expect("poisoned").capturing = false;
                conn.ungrab_pointer(CURRENT_TIME)?;
                conn.ungrab_keyboard(CURRENT_TIME)?;
                // HideCursor calls nest per client; ShowCursor until visible is
                // not observable, one call matches our one HideCursor.
                let _ = conn.xfixes_show_cursor(self.root);
                if let Some((x, y)) = warp {
                    conn.warp_pointer(x11rb::NONE, self.root, 0, 0, 0, 0, x as i16, y as i16)?;
                }
                conn.flush()?;
            }
            Cmd::Inject(a) => {
                self.inject(a)?;
                conn.flush()?;
            }
        }
        Ok(())
    }

    fn fake(&self, ty: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn.xtest_fake_input(ty, detail, CURRENT_TIME, self.root, x, y, 0)?;
        Ok(())
    }

    fn inject(&mut self, a: Action) -> Result<()> {
        match a {
            Action::Entered { x, y } | Action::MoveAbs { x, y, .. } => {
                self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x as i16, y as i16)?;
            }
            Action::Key { usage, down } => {
                if let Some(code) = flow_keymap::hid_to_evdev(usage) {
                    let ty = if down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
                    self.fake(ty, (code + 8).min(255) as u8, 0, 0)?;
                } else {
                    debug!(usage, "no X keycode for usage");
                }
            }
            Action::Button { button, down } => {
                let b = match button {
                    Button::Left => 1,
                    Button::Middle => 2,
                    Button::Right => 3,
                    Button::Back => 8,
                    Button::Forward => 9,
                };
                let ty = if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
                self.fake(ty, b, 0, 0)?;
            }
            Action::Wheel { dx, dy } => {
                self.wheel.0 += dx;
                self.wheel.1 += dy;
                while self.wheel.1.abs() >= 120 {
                    let b = if self.wheel.1 > 0 { 4 } else { 5 };
                    self.fake(xproto::BUTTON_PRESS_EVENT, b, 0, 0)?;
                    self.fake(xproto::BUTTON_RELEASE_EVENT, b, 0, 0)?;
                    self.wheel.1 -= 120 * self.wheel.1.signum();
                }
                while self.wheel.0.abs() >= 120 {
                    let b = if self.wheel.0 > 0 { 7 } else { 6 };
                    self.fake(xproto::BUTTON_PRESS_EVENT, b, 0, 0)?;
                    self.fake(xproto::BUTTON_RELEASE_EVENT, b, 0, 0)?;
                    self.wheel.0 -= 120 * self.wheel.0.signum();
                }
            }
            Action::Left => self.wheel = (0, 0),
        }
        Ok(())
    }
}

impl Backend for X11Backend {
    fn name(&self) -> &'static str {
        "x11"
    }
    fn caps(&self) -> Capabilities {
        Capabilities { capture: true, inject: true, clipboard: self.clip.is_some() }
    }
    fn monitors(&self) -> Vec<Monitor> {
        self.shared.lock().expect("poisoned").monitors.clone()
    }
    fn set_edges(&self, edges: Vec<Edge>) {
        self.shared.lock().expect("poisoned").edges = edges;
    }
    fn begin_capture(&self) {
        let _ = self.cmd.send(Cmd::Begin);
    }
    fn end_capture(&self, warp: Option<(i32, i32)>) {
        if self.shared.lock().expect("poisoned").capturing || warp.is_some() {
            let _ = self.cmd.send(Cmd::End(warp));
        }
    }
    fn inject(&self, action: &Action) {
        let _ = self.cmd.send(Cmd::Inject(*action));
    }
    fn set_clipboard(&self, content: &ClipContent) {
        if let Some(c) = &self.clip {
            c.set(content);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flow_proto::NodeId;

    #[test]
    fn edges_need_outward_push() {
        let e = [Edge { side: Side::Right, pos: 1920, start: 0, end: 1080, to: NodeId(2) }];
        assert_eq!(edge_hit(&e, 1919, 500, 3.0, 0.0), Some(Side::Right));
        assert_eq!(edge_hit(&e, 1919, 500, -3.0, 0.0), None);
        assert_eq!(edge_hit(&e, 1900, 500, 3.0, 0.0), None);
        assert_eq!(edge_hit(&e, 1919, 1080, 3.0, 0.0), None);
    }

    #[test]
    fn x_buttons() {
        assert_eq!(x_button(1), Some(Ok(Button::Left)));
        assert_eq!(x_button(4), Some(Err((0, 120))));
        assert_eq!(x_button(42), None);
    }
}
