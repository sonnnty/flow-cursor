//! Crossing key: the edge only switches machines while a configured key
//! combo is held (so a game that drives the pointer into a screen edge
//! does not throw the user onto another machine).
//!
//! The decision is backend independent and lives here:
//!
//! * [`required_for_edge`] — at the edge, with the keys known to be held,
//!   switch, refuse, or (backends where hitting the edge already captured
//!   input) hold the pointer at the edge and wait.
//! * [`EdgeHold`] — the wait. On GNOME/Wayland a pointer barrier activates
//!   input capture the moment the pointer touches it, and GNOME does not
//!   tell an InputCapture client which keys were already held. Instead of
//!   releasing at once (the next push would activate again: a stream of
//!   activations while the user pushes) the capture stays active while the
//!   user keeps pushing outward. The cursor is visibly where it would be
//!   anyway (stuck at the screen edge). Key presses arrive through the
//!   capture, so pressing the crossing key while pushing switches. Moving
//!   inward or along the edge, any other input, or a pause in pushing
//!   gives the pointer back right there.
//!
//! Which key gates a crossing: the one of the screen being *left* — this
//! machine's own setting at its own edges, the target's setting when the
//! virtual cursor leaves a controlled machine (also on the way back), so a
//! game running on a controlled machine is protected as well.

use flow_keymap::Combo;
use flow_layout::Side;
use std::collections::HashSet;
use std::time::{Duration, Instant};

/// A hold ends this long after the last outward push.
pub const HOLD_IDLE: Duration = Duration::from_millis(300);

/// Parses a crossing-key setting; empty or invalid = no key required (the
/// hub validates, so invalid only happens with a mismatched hub).
pub fn parse_crossing_key(s: &str) -> Option<Combo> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    match flow_keymap::parse_combo(s) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!("invalid crossing key {s:?} ({e}); crossing without a key");
            None
        }
    }
}

/// Whether the crossing key (if any) is held.
pub fn key_held(required: Option<&Combo>, held: &HashSet<u32>) -> bool {
    required.is_none_or(|c| c.is_held(|k| held.contains(&k)))
}

/// What to do when the pointer pushes against an edge that leads to a
/// reachable machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeDecision {
    /// Switch (subject to the edge timing gate).
    Cross,
    /// Stay on this machine; the backend gives the pointer back.
    Refuse,
    /// Stay, but keep the capture that hitting the edge started and wait
    /// for the key ([`EdgeHold`]).
    Hold,
}

/// The decision at an edge. `hit_captures`: the backend already captures
/// input when it reports the hit (Wayland), so keys are only visible from
/// now on.
pub fn required_for_edge(required: Option<&Combo>, held: &HashSet<u32>, hit_captures: bool) -> EdgeDecision {
    if key_held(required, held) {
        EdgeDecision::Cross
    } else if hit_captures {
        EdgeDecision::Hold
    } else {
        EdgeDecision::Refuse
    }
}

/// What a hold does with an event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HoldStep {
    /// Keep holding.
    Stay,
    /// An outward push: cross now if the key is held.
    Push,
    /// Give the pointer back at this local position (None = at the edge).
    Release(Option<(i32, i32)>),
}

/// The pointer is held at an edge while capture is active (see module docs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeHold {
    pub side: Side,
    /// Where the edge was hit (local desktop coordinates).
    pub x: f64,
    pub y: f64,
    last_push: Instant,
}

impl EdgeHold {
    pub fn new(side: Side, x: f64, y: f64, now: Instant) -> Self {
        EdgeHold { side, x, y, last_push: now }
    }

    /// Motion while held: outward = pushing, anything else releases the
    /// pointer where that motion takes it (at least one pixel inside, so
    /// the cursor continues without a jump).
    pub fn on_motion(&mut self, now: Instant, dx: f64, dy: f64) -> HoldStep {
        let (out, along) = match self.side {
            Side::Right => (dx, dy),
            Side::Left => (-dx, dy),
            Side::Bottom => (dy, dx),
            Side::Top => (-dy, dx),
        };
        if out < 0.0 || along.abs() >= 1.0 {
            return HoldStep::Release(Some(self.inside(dx, dy)));
        }
        if out > 0.0 {
            self.last_push = now;
            return HoldStep::Push;
        }
        HoldStep::Stay
    }

    /// A key event while held: keys of the crossing combo keep the hold
    /// (and may complete it), anything else ends it — the user is using
    /// this machine.
    pub fn on_key(&self, required: Option<&Combo>, usage: u32) -> HoldStep {
        if required.is_some_and(|c| c.contains(usage)) {
            HoldStep::Push
        } else {
            HoldStep::Release(None)
        }
    }

    /// True once the user stopped pushing for [`HOLD_IDLE`].
    pub fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.last_push) >= HOLD_IDLE
    }

    /// Hit point moved by (`dx`,`dy`), kept at least one pixel inside.
    fn inside(&self, dx: f64, dy: f64) -> (i32, i32) {
        let (mut x, mut y) = (self.x + dx, self.y + dy);
        match self.side {
            Side::Right => x = x.min(self.x - 1.0),
            Side::Left => x = x.max(self.x + 1.0),
            Side::Bottom => y = y.min(self.y - 1.0),
            Side::Top => y = y.max(self.y + 1.0),
        }
        (x.round() as i32, y.round() as i32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flow_keymap::keys;

    fn held(k: &[u32]) -> HashSet<u32> {
        k.iter().copied().collect()
    }

    #[test]
    fn no_key_configured_crosses_freely() {
        assert_eq!(required_for_edge(None, &held(&[]), false), EdgeDecision::Cross);
        assert_eq!(required_for_edge(None, &held(&[]), true), EdgeDecision::Cross);
        assert!(parse_crossing_key("  ").is_none());
        assert!(parse_crossing_key("NotAKey").is_none());
    }

    #[test]
    fn key_must_be_held() {
        let ctrl = parse_crossing_key("Ctrl");
        let c = ctrl.as_ref();
        // Backends that see keys outside capture: refuse until held.
        assert_eq!(required_for_edge(c, &held(&[]), false), EdgeDecision::Refuse);
        assert_eq!(required_for_edge(c, &held(&[keys::A]), false), EdgeDecision::Refuse);
        assert_eq!(required_for_edge(c, &held(&[keys::RIGHT_CTRL]), false), EdgeDecision::Cross);
        // Wayland: the hit already captured, hold and wait for the key.
        assert_eq!(required_for_edge(c, &held(&[]), true), EdgeDecision::Hold);
        assert_eq!(required_for_edge(c, &held(&[keys::LEFT_CTRL]), true), EdgeDecision::Cross);

        let combo = parse_crossing_key("LeftCtrl+LeftAlt");
        let c = combo.as_ref();
        assert_eq!(required_for_edge(c, &held(&[keys::LEFT_CTRL]), false), EdgeDecision::Refuse);
        assert_eq!(required_for_edge(c, &held(&[keys::RIGHT_CTRL, keys::LEFT_ALT]), false), EdgeDecision::Refuse);
        assert_eq!(required_for_edge(c, &held(&[keys::LEFT_CTRL, keys::LEFT_ALT]), false), EdgeDecision::Cross);

        let caps = parse_crossing_key("CapsLock");
        assert!(key_held(caps.as_ref(), &held(&[keys::CAPS_LOCK])));
        assert!(!key_held(caps.as_ref(), &held(&[keys::LEFT_SHIFT])));
    }

    #[test]
    fn hold_keeps_outward_pushes_and_releases_inward() {
        let t = Instant::now();
        let mut h = EdgeHold::new(Side::Right, 1920.0, 500.0, t);
        assert_eq!(h.on_motion(t, 5.0, 0.0), HoldStep::Push);
        assert_eq!(h.on_motion(t, 3.0, 0.4), HoldStep::Push, "sub-pixel jitter along the edge");
        assert_eq!(h.on_motion(t, 0.0, 0.0), HoldStep::Stay);
        // Moving back in: the cursor continues from the edge.
        assert_eq!(h.on_motion(t, -4.0, 1.0), HoldStep::Release(Some((1916, 501))));
        // A tiny inward move still lands one pixel inside, never on the barrier.
        assert_eq!(h.on_motion(t, -0.3, 0.0), HoldStep::Release(Some((1919, 500))));
        // Sliding along the edge.
        assert_eq!(h.on_motion(t, 2.0, -6.0), HoldStep::Release(Some((1919, 494))));

        let mut h = EdgeHold::new(Side::Left, 0.0, 300.0, t);
        assert_eq!(h.on_motion(t, -2.0, 0.0), HoldStep::Push);
        assert_eq!(h.on_motion(t, 3.0, 0.0), HoldStep::Release(Some((3, 300))));
        let mut h = EdgeHold::new(Side::Top, 700.0, 0.0, t);
        assert_eq!(h.on_motion(t, 0.0, -2.0), HoldStep::Push);
        assert_eq!(h.on_motion(t, 0.0, 0.5), HoldStep::Release(Some((700, 1))));
        let mut h = EdgeHold::new(Side::Bottom, 700.0, 1080.0, t);
        assert_eq!(h.on_motion(t, 0.0, 9.0), HoldStep::Push);
        assert_eq!(h.on_motion(t, 0.0, -9.0), HoldStep::Release(Some((700, 1071))));
    }

    #[test]
    fn hold_expires_without_pushing() {
        let t = Instant::now();
        let mut h = EdgeHold::new(Side::Right, 1920.0, 500.0, t);
        assert!(!h.expired(t + Duration::from_millis(100)));
        h.on_motion(t + Duration::from_millis(250), 1.0, 0.0);
        assert!(!h.expired(t + Duration::from_millis(500)), "the push renewed it");
        assert!(h.expired(t + Duration::from_millis(550)));
    }

    #[test]
    fn hold_keys() {
        let t = Instant::now();
        let h = EdgeHold::new(Side::Right, 1920.0, 500.0, t);
        let ca = parse_crossing_key("Ctrl+Alt");
        assert_eq!(h.on_key(ca.as_ref(), keys::LEFT_CTRL), HoldStep::Push);
        assert_eq!(h.on_key(ca.as_ref(), keys::RIGHT_ALT), HoldStep::Push);
        assert_eq!(h.on_key(ca.as_ref(), keys::A), HoldStep::Release(None), "typing ends the hold");
        assert_eq!(h.on_key(None, keys::LEFT_CTRL), HoldStep::Release(None));
    }
}
