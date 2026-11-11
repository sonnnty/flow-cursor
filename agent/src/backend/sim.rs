//! Simulated backend: no OS access. Tests (and `--backend sim`) drive it
//! through [`SimControl`]: emit local events, observe injected actions,
//! capture state, pointer warps and the clipboard.

use super::{Backend, EventRx, EventTx, LocalEvent};
use flow_layout::Edge;
use flow_net::link::Action;
use flow_proto::{Capabilities, ClipContent, Monitor, Rect};
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
        let state = Arc::new(Mutex::new(SimState { monitors, ..Default::default() }));
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
}

impl Backend for SimBackend {
    fn name(&self) -> &'static str {
        "sim"
    }
    fn caps(&self) -> Capabilities {
        Capabilities { capture: true, inject: true, clipboard: true }
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
        if let Some(w) = warp {
            s.warps.push(w);
            s.pointer = w;
        }
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
