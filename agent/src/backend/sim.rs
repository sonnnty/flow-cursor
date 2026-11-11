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
    pub fn last_warp(&self) -> Option<(i32, i32)> {
        self.state.lock().unwrap().warps.last().copied()
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
        s.injected.push(*action);
    }
    fn set_clipboard(&self, content: &ClipContent) {
        self.state.lock().unwrap().clipboard = Some(content.clone());
        let _ = &self.tx;
    }
}
