//! Receiving side of input links: exactly-once, in-order application of
//! reliable events per source, motion filtering, held-state tracking and
//! release-all. Pure state machine (no I/O) so it can be tested exhaustively.

use flow_proto::{Button, InputEvent, Motion, NodeId};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Max number of out-of-order events buffered per source.
pub const MAX_PENDING: usize = 4096;
/// A controlled session with no traffic (events, motion or pings) for this
/// long is considered dead: everything it holds is released.
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_millis(2500);

/// What the injector backend must do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// A source started controlling this node.
    Entered {
        x: i32,
        y: i32,
    },
    /// The source stopped controlling this node (after its keys were released).
    Left,
    MoveAbs {
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    },
    Key {
        usage: u32,
        down: bool,
    },
    Button {
        button: Button,
        down: bool,
    },
    Wheel {
        dx: i32,
        dy: i32,
    },
}

#[derive(Debug, Default)]
struct SourceState {
    session: u64,
    next_expected: u64,
    pending: BTreeMap<u64, InputEvent>,
    last_mseq: u64,
    /// Newest motion waiting for the reliable events it was sent after.
    pending_motion: Option<Motion>,
    held_keys: BTreeSet<u32>,
    held_buttons: BTreeSet<Button>,
    entered: bool,
    last_seen: Option<Instant>,
    retired: VecDeque<u64>,
}

impl SourceState {
    fn release_all(&mut self, out: &mut Vec<Action>) {
        for usage in std::mem::take(&mut self.held_keys) {
            out.push(Action::Key { usage, down: false });
        }
        for button in std::mem::take(&mut self.held_buttons) {
            out.push(Action::Button { button, down: false });
        }
    }

    fn leave(&mut self, out: &mut Vec<Action>) {
        self.release_all(out);
        if self.entered {
            self.entered = false;
            out.push(Action::Left);
        }
    }

    fn apply(&mut self, ev: InputEvent, out: &mut Vec<Action>) {
        match ev {
            InputEvent::Enter { x, y } => {
                self.entered = true;
                out.push(Action::Entered { x, y });
            }
            InputEvent::Leave => self.leave(out),
            InputEvent::Key { usage, down } => {
                if down && self.entered {
                    // A repeated down (auto-repeat) is forwarded too.
                    self.held_keys.insert(usage);
                    out.push(Action::Key { usage, down });
                } else if !down && self.held_keys.remove(&usage) {
                    out.push(Action::Key { usage, down });
                }
            }
            InputEvent::Button { button, down } => {
                if down && self.entered {
                    if self.held_buttons.insert(button) {
                        out.push(Action::Button { button, down });
                    }
                } else if !down && self.held_buttons.remove(&button) {
                    out.push(Action::Button { button, down });
                }
            }
            InputEvent::Wheel { dx, dy } => {
                if self.entered {
                    out.push(Action::Wheel { dx, dy });
                }
            }
            InputEvent::StateSync { keys, buttons } => {
                let keys: BTreeSet<u32> = keys.into_iter().collect();
                let buttons: BTreeSet<Button> = buttons.into_iter().collect();
                for k in self.held_keys.difference(&keys).copied().collect::<Vec<_>>() {
                    self.held_keys.remove(&k);
                    out.push(Action::Key { usage: k, down: false });
                }
                for b in self.held_buttons.difference(&buttons).copied().collect::<Vec<_>>() {
                    self.held_buttons.remove(&b);
                    out.push(Action::Button { button: b, down: false });
                }
                if self.entered {
                    for k in keys {
                        if self.held_keys.insert(k) {
                            out.push(Action::Key { usage: k, down: true });
                        }
                    }
                    for b in buttons {
                        if self.held_buttons.insert(b) {
                            out.push(Action::Button { button: b, down: true });
                        }
                    }
                }
            }
        }
    }
}

/// Per-source receive state for all links into this node.
#[derive(Debug, Default)]
pub struct ReceiverState {
    sources: HashMap<NodeId, SourceState>,
}

/// Result of handling a reliable event.
#[derive(Debug, PartialEq, Eq)]
pub struct EventOutcome {
    /// Cumulative ack to send back (`None` for ignored stale sessions).
    pub ack: Option<u64>,
    pub actions: Vec<Action>,
}

impl ReceiverState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handles a reliable event from `from` (already authenticated).
    pub fn on_event(
        &mut self,
        from: NodeId,
        session: u64,
        seq: u64,
        base: u64,
        ev: InputEvent,
        now: Instant,
    ) -> EventOutcome {
        let st = self.sources.entry(from).or_default();
        let mut actions = Vec::new();
        if st.session != session {
            if st.retired.contains(&session) {
                // Late packet from a session that was already replaced.
                return EventOutcome { ack: None, actions };
            }
            // New sender session (sender restarted or reset): release what
            // the old one held, then start at the sender's base.
            st.leave(&mut actions);
            if st.session != 0 {
                st.retired.push_back(st.session);
                if st.retired.len() > 8 {
                    st.retired.pop_front();
                }
            }
            st.session = session;
            st.next_expected = base.max(1);
            st.pending.clear();
            st.last_mseq = 0;
            // Motion of this session may have arrived before its first event.
            st.pending_motion = st.pending_motion.filter(|m| m.session == session);
        }
        st.last_seen = Some(now);
        if seq >= st.next_expected && (st.pending.len() < MAX_PENDING || st.pending.contains_key(&seq)) {
            st.pending.insert(seq, ev);
        }
        while let Some(ev) = st.pending.remove(&st.next_expected) {
            st.apply(ev, &mut actions);
            st.next_expected += 1;
        }
        if let Some(m) = st.pending_motion.filter(|m| m.session == session && m.after_seq < st.next_expected) {
            st.pending_motion = None;
            if st.entered && m.mseq > st.last_mseq {
                st.last_mseq = m.mseq;
                actions.push(Action::MoveAbs { x: m.x, y: m.y, dx: m.dx, dy: m.dy });
            }
        }
        EventOutcome { ack: Some(st.next_expected - 1), actions }
    }

    /// Handles a motion datagram. Returns the injection action if the
    /// motion is current and its prerequisites were applied.
    pub fn on_motion(&mut self, from: NodeId, m: &Motion, now: Instant) -> Option<Action> {
        let st = self.sources.entry(from).or_default();
        if st.session != m.session {
            // Datagrams can overtake the first reliable event of a new
            // session (its Enter): keep the newest one for when it arrives.
            // Never displace a held-back motion of the current session.
            let replace = match st.pending_motion {
                None => true,
                Some(p) if p.session == st.session && st.session != 0 => false,
                Some(p) => p.session != m.session || p.mseq < m.mseq,
            };
            if replace && !st.retired.contains(&m.session) {
                st.pending_motion = Some(*m);
            }
            return None;
        }
        st.last_seen = Some(now);
        if m.mseq <= st.last_mseq {
            return None;
        }
        if m.after_seq >= st.next_expected {
            // The datagram overtook a reliable event (e.g. a click); keep the
            // newest one and apply it once that event is in.
            if st.pending_motion.is_none_or(|p| p.session != m.session || p.mseq < m.mseq) {
                st.pending_motion = Some(*m);
            }
            return None;
        }
        if !st.entered {
            return None;
        }
        st.last_mseq = m.mseq;
        Some(Action::MoveAbs { x: m.x, y: m.y, dx: m.dx, dy: m.dy })
    }

    /// A ping keeps a controlled session alive while the user is idle.
    pub fn on_ping(&mut self, from: NodeId, session: u64, now: Instant) {
        if let Some(st) = self.sources.get_mut(&from) {
            if st.session == session {
                st.last_seen = Some(now);
            }
        }
    }

    /// Releases everything held by sources that went silent.
    pub fn check_timeouts(&mut self, now: Instant, timeout: Duration) -> Vec<(NodeId, Vec<Action>)> {
        let mut out = Vec::new();
        for (id, st) in self.sources.iter_mut() {
            let stale = st.last_seen.is_none_or(|t| now.duration_since(t) > timeout);
            if stale && (st.entered || !st.held_keys.is_empty() || !st.held_buttons.is_empty()) {
                let mut a = Vec::new();
                st.leave(&mut a);
                out.push((*id, a));
            }
        }
        out
    }

    /// Releases everything held by `from` (e.g. it was revoked or went offline).
    pub fn release_source(&mut self, from: NodeId) -> Vec<Action> {
        let mut a = Vec::new();
        if let Some(st) = self.sources.get_mut(&from) {
            st.leave(&mut a);
        }
        a
    }

    /// Releases everything held by every source.
    pub fn release_everything(&mut self) -> Vec<(NodeId, Vec<Action>)> {
        let ids: Vec<NodeId> = self.sources.keys().copied().collect();
        ids.into_iter().map(|id| (id, self.release_source(id))).filter(|(_, a)| !a.is_empty()).collect()
    }

    /// Sources currently controlling this node.
    pub fn controllers(&self) -> Vec<NodeId> {
        self.sources.iter().filter(|(_, s)| s.entered).map(|(id, _)| *id).collect()
    }

    pub fn held_keys(&self, from: NodeId) -> Vec<u32> {
        self.sources.get(&from).map(|s| s.held_keys.iter().copied().collect()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: NodeId = NodeId(1);
    const K: u32 = 0x0007_0004;

    fn key(down: bool) -> InputEvent {
        InputEvent::Key { usage: K, down }
    }

    #[test]
    fn in_order_and_dedup() {
        let mut r = ReceiverState::new();
        let t = Instant::now();
        let o = r.on_event(S, 9, 1, 1, InputEvent::Enter { x: 1, y: 2 }, t);
        assert_eq!(o, EventOutcome { ack: Some(1), actions: vec![Action::Entered { x: 1, y: 2 }] });
        let o = r.on_event(S, 9, 2, 1, key(true), t);
        assert_eq!(o.actions, vec![Action::Key { usage: K, down: true }]);
        // Duplicate (resent over the other path) is acked but not applied.
        let o = r.on_event(S, 9, 2, 1, key(true), t);
        assert_eq!(o, EventOutcome { ack: Some(2), actions: vec![] });
        let o = r.on_event(S, 9, 1, 1, InputEvent::Enter { x: 1, y: 2 }, t);
        assert_eq!(o, EventOutcome { ack: Some(2), actions: vec![] });
    }

    #[test]
    fn reorders_across_paths() {
        let mut r = ReceiverState::new();
        let t = Instant::now();
        r.on_event(S, 9, 1, 1, InputEvent::Enter { x: 0, y: 0 }, t);
        // 3 arrives before 2 (new path faster than old one).
        let o = r.on_event(S, 9, 3, 2, key(false), t);
        assert_eq!(o, EventOutcome { ack: Some(1), actions: vec![] });
        let o = r.on_event(S, 9, 2, 2, key(true), t);
        assert_eq!(o.ack, Some(3));
        assert_eq!(o.actions, vec![Action::Key { usage: K, down: true }, Action::Key { usage: K, down: false }]);
    }

    #[test]
    fn leave_and_new_session_release_everything() {
        let mut r = ReceiverState::new();
        let t = Instant::now();
        r.on_event(S, 9, 1, 1, InputEvent::Enter { x: 0, y: 0 }, t);
        r.on_event(S, 9, 2, 1, key(true), t);
        r.on_event(S, 9, 3, 1, InputEvent::Button { button: Button::Left, down: true }, t);
        assert_eq!(r.controllers(), vec![S]);
        // Sender restarted: new session; base tells where it starts.
        let o = r.on_event(S, 10, 5, 5, InputEvent::Enter { x: 3, y: 3 }, t);
        assert_eq!(
            o.actions,
            vec![
                Action::Key { usage: K, down: false },
                Action::Button { button: Button::Left, down: false },
                Action::Left,
                Action::Entered { x: 3, y: 3 }
            ]
        );
        assert_eq!(o.ack, Some(5));
        // A late packet from the retired session is ignored.
        let o = r.on_event(S, 9, 4, 1, key(true), t);
        assert_eq!(o, EventOutcome { ack: None, actions: vec![] });
        let o = r.on_event(S, 10, 6, 5, key(true), t);
        assert_eq!(o.actions, vec![Action::Key { usage: K, down: true }]);
        let o = r.on_event(S, 10, 7, 5, InputEvent::Leave, t);
        assert_eq!(o.actions, vec![Action::Key { usage: K, down: false }, Action::Left]);
        // Keys while not entered are not injected.
        let o = r.on_event(S, 10, 8, 5, key(true), t);
        assert!(o.actions.is_empty());
    }

    #[test]
    fn state_sync_reconciles() {
        let mut r = ReceiverState::new();
        let t = Instant::now();
        r.on_event(S, 9, 1, 1, InputEvent::Enter { x: 0, y: 0 }, t);
        r.on_event(S, 9, 2, 1, key(true), t);
        let shift = 0x0007_00E1;
        let o = r.on_event(S, 9, 3, 1, InputEvent::StateSync { keys: vec![shift], buttons: vec![Button::Right] }, t);
        assert_eq!(
            o.actions,
            vec![
                Action::Key { usage: K, down: false },
                Action::Key { usage: shift, down: true },
                Action::Button { button: Button::Right, down: true }
            ]
        );
        // Identical sync is a no-op.
        let o = r.on_event(S, 9, 4, 1, InputEvent::StateSync { keys: vec![shift], buttons: vec![Button::Right] }, t);
        assert!(o.actions.is_empty());
    }

    #[test]
    fn motion_filtering() {
        let mut r = ReceiverState::new();
        let t = Instant::now();
        let m = |mseq, after_seq| Motion { session: 9, mseq, after_seq, x: 5, y: 6, dx: 1, dy: 1 };
        // Motion overtaking the Enter of a new session is held back…
        assert_eq!(r.on_motion(S, &m(1, 1), t), None);
        let o = r.on_event(S, 9, 1, 1, InputEvent::Enter { x: 0, y: 0 }, t);
        assert_eq!(o.actions, vec![Action::Entered { x: 0, y: 0 }, Action::MoveAbs { x: 5, y: 6, dx: 1, dy: 1 }]);
        assert_eq!(r.on_motion(S, &m(1, 1), t), None, "already applied");
        // Older datagram.
        assert_eq!(r.on_motion(S, &m(1, 1), t), None);
        // Motion sent after an event we have not applied yet: held back…
        assert_eq!(r.on_motion(S, &m(2, 2), t), None);
        // Wrong session.
        assert_eq!(r.on_motion(S, &Motion { session: 8, ..m(3, 1) }, t), None);
        // …and applied right after that event.
        let o = r.on_event(S, 9, 2, 1, key(true), t);
        assert_eq!(o.actions, vec![Action::Key { usage: K, down: true }, Action::MoveAbs { x: 5, y: 6, dx: 1, dy: 1 }]);
        assert_eq!(r.on_motion(S, &m(2, 1), t), None, "already applied");
        assert!(r.on_motion(S, &m(3, 1), t).is_some());
    }

    #[test]
    fn heartbeat_timeout_releases() {
        let mut r = ReceiverState::new();
        let t = Instant::now();
        r.on_event(S, 9, 1, 1, InputEvent::Enter { x: 0, y: 0 }, t);
        r.on_event(S, 9, 2, 1, key(true), t);
        r.on_ping(S, 9, t + Duration::from_secs(2));
        assert!(r.check_timeouts(t + Duration::from_secs(3), HEARTBEAT_TIMEOUT).is_empty());
        let out = r.check_timeouts(t + Duration::from_secs(5), HEARTBEAT_TIMEOUT);
        assert_eq!(out, vec![(S, vec![Action::Key { usage: K, down: false }, Action::Left])]);
        assert!(r.check_timeouts(t + Duration::from_secs(9), HEARTBEAT_TIMEOUT).is_empty());
    }

    #[test]
    fn pending_buffer_is_bounded() {
        let mut r = ReceiverState::new();
        let t = Instant::now();
        r.on_event(S, 9, 1, 1, InputEvent::Enter { x: 0, y: 0 }, t);
        for seq in 3..(3 + MAX_PENDING as u64 + 100) {
            r.on_event(S, 9, seq, 1, key(true), t);
        }
        let st = &r.sources[&S];
        assert_eq!(st.pending.len(), MAX_PENDING);
    }
}
