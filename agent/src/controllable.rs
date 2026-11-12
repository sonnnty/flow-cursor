//! When this machine may be controlled by others, and therefore when a
//! backend that opens its remote control session on demand should keep it
//! open (GNOME/Wayland: the RemoteDesktop portal session, during which
//! GNOME shows its "someone is controlling this computer" indicator).
//!
//! The session is open while the agent has a full session with the hub and
//! sharing is on (not away, not paused). It closes at once when sharing
//! stops, the agent is revoked, the hub only offers an update, or the agent
//! stops. When the hub connection drops it stays open for [`HUB_GRACE`]:
//! the agent reconnects within that time after a hub restart or a short
//! network blip, and closing and reopening the session for that would only
//! flash the indicator (and, on GNOME, cost a portal round trip during
//! which the machine cannot be controlled). The grace only keeps an open
//! session open; it never opens one.
//!
//! The decision is backend independent ([`SessionGate`]); the core applies
//! it with [`crate::backend::Backend::set_controllable`], which is a no-op
//! on backends without such a session (X11, Windows).

use std::time::{Duration, Instant};

/// How long the session stays open after the hub connection dropped.
///
/// The agent retries the hub 0.5 s after a session that was up for a while
/// drops, then after 1, 2 and 4 s more (see `crate::hub`): attempts at
/// about 0.5, 1.5, 3.5 and 7.5 s. Ten seconds covers all four, i.e. a hub
/// container restart or a Wi-Fi roam, while a hub that is really down
/// stops being a reason to keep the indicator for long.
pub const HUB_GRACE: Duration = Duration::from_secs(10);

/// The agent's relation to the hub, as far as being controlled goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HubState {
    /// Full session with the hub.
    Connected,
    /// Not connected (never yet, or the connection dropped); the agent keeps
    /// trying.
    Lost,
    /// Connected or not, nothing may control this machine until something
    /// changes on purpose: revoked, update-only session, agent stopping.
    Unusable,
}

/// Decides whether the session should be open (see the module docs).
#[derive(Clone, Debug)]
pub struct SessionGate {
    grace: Duration,
    open: bool,
    /// When the hub connection was found lost while the session was open.
    lost_at: Option<Instant>,
}

impl SessionGate {
    pub fn new(grace: Duration) -> Self {
        SessionGate { grace, open: false, lost_at: None }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Re-evaluates with the current state; returns the new decision when
    /// it changes.
    pub fn update(&mut self, now: Instant, hub: HubState, away: bool) -> Option<bool> {
        let hub_ok = match hub {
            HubState::Connected => {
                self.lost_at = None;
                true
            }
            HubState::Lost => {
                let since = *self.lost_at.get_or_insert(now);
                self.open && now.saturating_duration_since(since) < self.grace
            }
            HubState::Unusable => {
                self.lost_at = None;
                false
            }
        };
        let open = hub_ok && !away;
        if !open {
            self.lost_at = None;
        }
        (open != self.open).then(|| {
            self.open = open;
            open
        })
    }

    /// When [`SessionGate::update`] has to run again for a grace period to
    /// end (None: nothing pending).
    pub fn deadline(&self) -> Option<Instant> {
        self.lost_at.filter(|_| self.open).map(|t| t + self.grace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use HubState::*;

    const G: Duration = Duration::from_secs(10);

    fn at(t0: Instant, secs: f64) -> Instant {
        t0 + Duration::from_secs_f64(secs)
    }

    #[test]
    fn opens_only_with_a_full_hub_session_and_sharing_on() {
        let t0 = Instant::now();
        let mut g = SessionGate::new(G);
        assert_eq!(g.update(t0, Lost, false), None, "not connected yet: stays closed");
        assert_eq!(g.update(t0, Unusable, false), None, "update-only or revoked: stays closed");
        assert_eq!(g.update(t0, Connected, true), None, "away or paused: stays closed");
        assert_eq!(g.update(t0, Connected, false), Some(true));
        assert!(g.is_open());
        assert_eq!(g.update(t0, Connected, false), None, "no change, nothing to do");
        assert_eq!(g.deadline(), None);
    }

    #[test]
    fn away_paused_revoked_and_update_only_close_at_once() {
        let t0 = Instant::now();
        let mut g = SessionGate::new(G);
        g.update(t0, Connected, false);
        assert_eq!(g.update(t0, Connected, true), Some(false), "away / paused");
        assert_eq!(g.update(t0, Connected, false), Some(true), "back home");
        assert_eq!(g.update(t0, Unusable, false), Some(false), "revoked / update-only / stopping");
        assert_eq!(g.update(t0, Connected, false), Some(true));
    }

    #[test]
    fn a_lost_hub_closes_after_the_grace_period() {
        let t0 = Instant::now();
        let mut g = SessionGate::new(G);
        g.update(t0, Connected, false);
        let lost = at(t0, 5.0);
        assert_eq!(g.update(lost, Lost, false), None, "still open during the grace period");
        assert_eq!(g.deadline(), Some(lost + G));
        assert_eq!(g.update(at(lost, 9.9), Lost, false), None);
        assert_eq!(g.update(lost + G, Lost, false), Some(false));
        assert_eq!(g.deadline(), None);
        // Reconnecting later opens it again.
        assert_eq!(g.update(at(lost, 30.0), Connected, false), Some(true));
    }

    #[test]
    fn a_quick_reconnect_keeps_the_session_and_restarts_the_grace() {
        let t0 = Instant::now();
        let mut g = SessionGate::new(G);
        g.update(t0, Connected, false);
        assert_eq!(g.update(at(t0, 1.0), Lost, false), None);
        assert_eq!(g.update(at(t0, 3.0), Connected, false), None, "reconnected within the grace: no flapping");
        assert_eq!(g.deadline(), None);
        // A second drop gets a full grace period of its own.
        assert_eq!(g.update(at(t0, 4.0), Lost, false), None);
        assert_eq!(g.update(at(t0, 13.0), Lost, false), None);
        assert_eq!(g.update(at(t0, 14.0), Lost, false), Some(false));
    }

    #[test]
    fn the_grace_never_opens_a_closed_session() {
        let t0 = Instant::now();
        let mut g = SessionGate::new(G);
        g.update(t0, Connected, false);
        assert_eq!(g.update(at(t0, 1.0), Lost, false), None);
        assert_eq!(g.update(at(t0, 2.0), Lost, true), Some(false), "away while the hub is lost: closes at once");
        assert_eq!(g.update(at(t0, 3.0), Lost, false), None, "back home, hub still lost: stays closed");
        assert_eq!(g.update(at(t0, 4.0), Connected, false), Some(true));
        // Revoked right after a drop: no grace.
        assert_eq!(g.update(at(t0, 5.0), Lost, false), None);
        assert_eq!(g.update(at(t0, 5.5), Unusable, false), Some(false));
        assert_eq!(g.update(at(t0, 6.0), Lost, false), None);
    }
}
