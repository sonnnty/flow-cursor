//! Simulated backend: no OS access. Tests (and `--backend sim`) drive it
//! through [`SimControl`]: emit local events, observe injected actions,
//! capture state, pointer warps, the local cursor and the clipboard.
//!
//! With [`SimControl::set_on_demand`] it models a backend whose remote
//! control session is opened on demand (GNOME/Wayland's RemoteDesktop
//! portal session): it opens after [`SimState::open_delay`] once the core
//! asks for it, closes at once, and injection or clipboard updates while it
//! is closed never reach the "OS" (they are recorded apart).

use super::{Backend, EventRx, EventTx, LocalEvent};
use crate::park::Park;
use flow_layout::Edge;
use flow_net::link::Action;
use flow_proto::{Capabilities, ClipContent, CursorSupport, Monitor, Rect};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

pub fn default_monitors() -> Vec<Monitor> {
    vec![Monitor { name: "SIM-1".into(), rect: Rect::new(0, 0, 1920, 1080), primary: true }]
}

#[derive(Debug, Default)]
pub struct SimState {
    pub monitors: Vec<Monitor>,
    pub edges: Vec<Edge>,
    pub capturing: bool,
    pub warps: Vec<(i32, i32)>,
    pub injected: Vec<Action>,
    pub clipboard: Option<ClipContent>,
    /// Absolute pointer position after injected moves.
    pub pointer: (i32, i32),
    /// Optional observer notified of every injected action (latency bench).
    pub observer: Option<mpsc::UnboundedSender<Action>>,
    /// Behave like Wayland: an edge hit starts capture, keys are only
    /// visible while capturing.
    pub wayland_like: bool,
    /// Crossing combo set by the core.
    pub crossing_key: Option<flow_keymap::Combo>,
    /// Number of `end_capture` calls (refused crossings included).
    pub end_captures: usize,
    /// What the simulated backend can do with its cursor (reported in its
    /// capabilities; set before the agent starts). Default: everything.
    pub cursor_support: CursorSupport,
    /// The local cursor is hidden. Modelled like hiding that outlives the
    /// process (e.g. replaced system cursors): only `end_capture` or
    /// `restore_cursor` shows it again.
    pub cursor_hidden: bool,
    /// Every `park_cursor` request.
    pub parks: Vec<Park>,
    /// Number of `restore_cursor` calls.
    pub restores: usize,
    /// The remote control session is opened on demand (set before the
    /// agent starts). Off: always open, like X11 and Windows.
    pub on_demand: bool,
    /// The on-demand session is open (injection works).
    pub session_open: bool,
    /// Last `set_controllable` from the core.
    pub session_wanted: bool,
    /// How long opening the session takes (the portal round trip).
    pub open_delay: Duration,
    /// Every open (true) and close (false) of the session, in order.
    pub session_log: Vec<bool>,
    /// Actions the core injected while the session was closed (they
    /// never reach the "OS").
    pub injected_while_closed: Vec<Action>,
    /// Bumped on every `set_controllable`, so an open that is still
    /// pending when the core changes its mind is dropped.
    session_gen: u64,
}

pub struct SimBackend {
    state: Arc<Mutex<SimState>>,
    tx: EventTx,
}

/// Test handle to a [`SimBackend`].
#[derive(Clone)]
pub struct SimControl {
    pub state: Arc<Mutex<SimState>>,
    tx: EventTx,
}

impl SimBackend {
    pub fn create(monitors: Vec<Monitor>) -> (Arc<dyn Backend>, EventRx) {
        let (b, _ctl, rx) = Self::with_control(monitors);
        (b, rx)
    }

    pub fn with_control(monitors: Vec<Monitor>) -> (Arc<dyn Backend>, SimControl, EventRx) {
        let (tx, rx) = mpsc::unbounded_channel();
        let state =
            Arc::new(Mutex::new(SimState { monitors, cursor_support: CursorSupport::FULL, ..Default::default() }));
        let b = Arc::new(SimBackend { state: state.clone(), tx: tx.clone() });
        (b, SimControl { state, tx }, rx)
    }
}

impl SimControl {
    pub fn emit(&self, ev: LocalEvent) {
        let _ = self.tx.send(ev);
    }
    /// The user copies something on this machine.
    pub fn copy(&self, c: ClipContent) {
        self.state.lock().unwrap().clipboard = Some(c.clone());
        self.emit(LocalEvent::Clipboard(c));
    }
    /// The user plugs in or unplugs a monitor (or rearranges them): the
    /// backend reports the new set like the real ones do.
    pub fn set_monitors(&self, m: Vec<Monitor>) {
        self.state.lock().unwrap().monitors = m.clone();
        self.emit(LocalEvent::MonitorsChanged(m));
    }
    pub fn take_injected(&self) -> Vec<Action> {
        std::mem::take(&mut self.state.lock().unwrap().injected)
    }
    pub fn capturing(&self) -> bool {
        self.state.lock().unwrap().capturing
    }
    pub fn clipboard(&self) -> Option<ClipContent> {
        self.state.lock().unwrap().clipboard.clone()
    }
    pub fn edges(&self) -> Vec<Edge> {
        self.state.lock().unwrap().edges.clone()
    }
    /// Streams every injected action as it happens.
    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<Action> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.state.lock().unwrap().observer = Some(tx);
        rx
    }
    pub fn last_warp(&self) -> Option<(i32, i32)> {
        self.state.lock().unwrap().warps.last().copied()
    }
    /// Switches to Wayland-like behaviour (see [`SimState::wayland_like`]).
    pub fn set_wayland_like(&self, on: bool) {
        self.state.lock().unwrap().wayland_like = on;
    }
    /// The pointer pushes against an edge (on a Wayland-like backend this
    /// activates capture first, like a pointer barrier).
    pub fn push_edge(&self, side: flow_layout::Side, x: i32, y: i32) {
        {
            let mut s = self.state.lock().unwrap();
            s.pointer = (x, y);
            if s.wayland_like {
                s.capturing = true;
            }
        }
        self.emit(LocalEvent::EdgeHit { side, x, y });
    }
    /// A physical key press/release, delivered the way the backend would:
    /// captured while capturing, otherwise reported only for keys of the
    /// crossing combo and only when not Wayland-like.
    pub fn key(&self, usage: u32, down: bool) {
        let ev = {
            let s = self.state.lock().unwrap();
            if s.capturing {
                Some(LocalEvent::Key { usage, down })
            } else if !s.wayland_like && s.crossing_key.as_ref().is_some_and(|c| c.contains(usage)) {
                Some(LocalEvent::LocalKey { usage, down })
            } else {
                None
            }
        };
        if let Some(ev) = ev {
            self.emit(ev);
        }
    }
    pub fn crossing_key(&self) -> Option<flow_keymap::Combo> {
        self.state.lock().unwrap().crossing_key.clone()
    }
    pub fn end_captures(&self) -> usize {
        self.state.lock().unwrap().end_captures
    }
    /// Where the local cursor is (last warp, park or injected position).
    pub fn pointer(&self) -> (i32, i32) {
        self.state.lock().unwrap().pointer
    }
    pub fn cursor_hidden(&self) -> bool {
        self.state.lock().unwrap().cursor_hidden
    }
    pub fn parks(&self) -> Vec<Park> {
        self.state.lock().unwrap().parks.clone()
    }
    /// Simulates a backend that cannot move or hide its cursor (GNOME)
    /// or only some of it. Takes effect for capabilities reported later,
    /// i.e. call it before the agent starts.
    pub fn set_cursor_support(&self, s: CursorSupport) {
        self.state.lock().unwrap().cursor_support = s;
    }
    /// Models an on-demand remote control session (see the module docs)
    /// that takes `open_delay` to open. Call it before the agent starts.
    pub fn set_on_demand(&self, open_delay: Duration) {
        let mut s = self.state.lock().unwrap();
        s.on_demand = true;
        s.open_delay = open_delay;
    }
    pub fn session_open(&self) -> bool {
        self.state.lock().unwrap().session_open
    }
    pub fn session_log(&self) -> Vec<bool> {
        self.state.lock().unwrap().session_log.clone()
    }
    pub fn injected_while_closed(&self) -> Vec<Action> {
        self.state.lock().unwrap().injected_while_closed.clone()
    }
    /// The session is lost on the backend's side (portal gone, EI
    /// disconnect); the core hears about it like from the real backend.
    pub fn lose_session(&self) {
        {
            let mut s = self.state.lock().unwrap();
            if !s.session_open {
                return;
            }
            s.session_open = false;
            s.session_log.push(false);
        }
        self.emit(LocalEvent::CapsChanged);
    }
}

impl SimState {
    fn can_inject(&self) -> bool {
        !self.on_demand || self.session_open
    }
}

impl Backend for SimBackend {
    fn name(&self) -> &'static str {
        "sim"
    }
    fn caps(&self) -> Capabilities {
        let s = self.state.lock().unwrap();
        let on = s.can_inject();
        Capabilities { capture: true, inject: on, clipboard: on, cursor: s.cursor_support }
    }
    fn monitors(&self) -> Vec<Monitor> {
        self.state.lock().unwrap().monitors.clone()
    }
    fn set_edges(&self, edges: Vec<Edge>) {
        self.state.lock().unwrap().edges = edges;
    }
    fn begin_capture(&self) {
        self.state.lock().unwrap().capturing = true;
    }
    fn end_capture(&self, warp: Option<(i32, i32)>) {
        let mut s = self.state.lock().unwrap();
        s.capturing = false;
        s.end_captures += 1;
        s.cursor_hidden = false;
        if let Some(w) = warp {
            s.warps.push(w);
            s.pointer = w;
        }
    }
    fn park_cursor(&self, park: Park) {
        let mut s = self.state.lock().unwrap();
        s.parks.push(park);
        s.pointer = park.at;
        if park.hide {
            s.cursor_hidden = true;
        }
    }
    fn restore_cursor(&self) {
        let mut s = self.state.lock().unwrap();
        s.restores += 1;
        s.cursor_hidden = false;
    }
    fn inject(&self, action: &Action) {
        let mut s = self.state.lock().unwrap();
        if !s.can_inject() {
            s.injected_while_closed.push(*action);
            return;
        }
        match action {
            Action::MoveAbs { x, y, .. } | Action::Entered { x, y } => s.pointer = (*x, *y),
            _ => {}
        }
        if let Some(o) = &s.observer {
            let _ = o.send(*action);
        }
        s.injected.push(*action);
    }
    fn set_clipboard(&self, content: &ClipContent) {
        let mut s = self.state.lock().unwrap();
        // Like the portal clipboard, which belongs to the session.
        if s.can_inject() {
            s.clipboard = Some(content.clone());
        }
    }
    fn session_support(&self) -> crate::controllable::SessionSupport {
        if self.state.lock().unwrap().on_demand {
            crate::controllable::SessionSupport::OnDemand
        } else {
            crate::controllable::SessionSupport::Always
        }
    }
    fn set_controllable(&self, on: bool) {
        let (generation, delay) = {
            let mut s = self.state.lock().unwrap();
            if !s.on_demand {
                return;
            }
            s.session_wanted = on;
            s.session_gen += 1;
            if !on {
                if s.session_open {
                    s.session_open = false;
                    s.session_log.push(false);
                }
                return;
            }
            if s.session_open {
                return;
            }
            (s.session_gen, s.open_delay)
        };
        let (state, tx) = (self.state.clone(), self.tx.clone());
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            {
                let mut s = state.lock().unwrap();
                if s.session_gen != generation || s.session_open {
                    return;
                }
                s.session_open = true;
                s.session_log.push(true);
            }
            let _ = tx.send(LocalEvent::CapsChanged);
        });
    }
    fn observes_keys(&self) -> bool {
        !self.state.lock().unwrap().wayland_like
    }
    fn edge_hit_captures(&self) -> bool {
        self.state.lock().unwrap().wayland_like
    }
    fn set_crossing_key(&self, combo: Option<flow_keymap::Combo>) {
        self.state.lock().unwrap().crossing_key = combo;
    }
}
