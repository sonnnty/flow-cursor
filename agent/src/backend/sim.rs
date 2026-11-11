//! Simulated backend: no OS access. Tests (and `--backend sim`) drive it
//! through [`SimControl`]: emit local events, observe injected actions,
//! capture state, pointer warps, the local cursor and the clipboard.

use super::{Backend, EventRx, EventTx, LocalEvent};
use crate::park::Park;
use flow_layout::Edge;
use flow_net::link::Action;
use flow_proto::{Capabilities, ClipContent, CursorSupport, Monitor, Rect};
use std::sync::{Arc, Mutex};
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
}

impl Backend for SimBackend {
    fn name(&self) -> &'static str {
        "sim"
    }
    fn caps(&self) -> Capabilities {
        Capabilities { capture: true, inject: true, clipboard: true, cursor: self.state.lock().unwrap().cursor_support }
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
        self.state.lock().unwrap().clipboard = Some(content.clone());
        let _ = &self.tx;
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
