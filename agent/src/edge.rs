//! Edge behaviour: switch delay, double bump and dead corners.
//!
//! Backends report every time the pointer pushes against an edge that leads
//! to another machine (`EdgeHit`). Consecutive hits less than [`STREAK_GAP`]
//! apart form one *push*. A switch is allowed when
//! * the hit is not within `dead_corner_px` of a monitor corner,
//! * the current push lasted at least `switch_delay_ms`, and
//! * with `double_bump`, a previous push ended at most
//!   `double_bump_window_ms` before the current one started.

use flow_layout::Side;
use flow_proto::{EdgeSettings, Rect};
use std::time::{Duration, Instant};

pub const STREAK_GAP: Duration = Duration::from_millis(150);

#[derive(Debug, Default)]
pub struct EdgeGate {
    pub cfg: EdgeSettings,
    side: Option<Side>,
    streak_start: Option<Instant>,
    last_hit: Option<Instant>,
    prev_streak_end: Option<Instant>,
}

impl EdgeGate {
    pub fn new(cfg: EdgeSettings) -> Self {
        EdgeGate { cfg, ..Default::default() }
    }

    pub fn reset(&mut self) {
        *self = EdgeGate::new(self.cfg.clone());
    }

    /// Records a hit at local (`x`,`y`) on monitor `mon`; true = switch now.
    pub fn hit(&mut self, now: Instant, side: Side, x: i32, y: i32, mon: Option<Rect>) -> bool {
        if let (Some(m), c) = (mon, self.cfg.dead_corner_px as i32) {
            if c > 0 {
                let along =
                    if side.is_horizontal() { (y - m.y, m.bottom() - 1 - y) } else { (x - m.x, m.right() - 1 - x) };
                if along.0 < c || along.1 < c {
                    return false;
                }
            }
        }
        let continuing = self.side == Some(side) && self.last_hit.is_some_and(|t| now.duration_since(t) <= STREAK_GAP);
        if !continuing {
            self.prev_streak_end = if self.side == Some(side) { self.last_hit } else { None };
            self.streak_start = Some(now);
            self.side = Some(side);
        }
        self.last_hit = Some(now);
        let start = self.streak_start.expect("set above");
        if self.cfg.double_bump {
            let window = Duration::from_millis(self.cfg.double_bump_window_ms as u64);
            let ok = self.prev_streak_end.is_some_and(|end| start.duration_since(end) <= window);
            if !ok {
                return false;
            }
        }
        now.duration_since(start) >= Duration::from_millis(self.cfg.switch_delay_ms as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    const MON: Option<Rect> = Some(Rect::new(0, 0, 1920, 1080));

    #[test]
    fn immediate_by_default() {
        let mut g = EdgeGate::new(EdgeSettings::default());
        assert!(g.hit(Instant::now(), Side::Right, 1919, 500, MON));
    }

    #[test]
    fn delay_requires_continuous_push() {
        let mut g = EdgeGate::new(EdgeSettings { switch_delay_ms: 300, ..Default::default() });
        let t = Instant::now();
        assert!(!g.hit(t, Side::Right, 1919, 500, MON));
        assert!(!g.hit(t + ms(100), Side::Right, 1919, 500, MON));
        assert!(!g.hit(t + ms(200), Side::Right, 1919, 500, MON));
        assert!(g.hit(t + ms(310), Side::Right, 1919, 500, MON));
        // A pause restarts the delay.
        g.reset();
        assert!(!g.hit(t, Side::Right, 1919, 500, MON));
        assert!(!g.hit(t + ms(400), Side::Right, 1919, 500, MON));
    }

    #[test]
    fn double_bump() {
        let mut g = EdgeGate::new(EdgeSettings { double_bump: true, double_bump_window_ms: 500, ..Default::default() });
        let t = Instant::now();
        assert!(!g.hit(t, Side::Left, 0, 500, MON));
        assert!(!g.hit(t + ms(50), Side::Left, 0, 500, MON), "same push");
        assert!(g.hit(t + ms(400), Side::Left, 0, 500, MON), "second bump");
        g.reset();
        assert!(!g.hit(t, Side::Left, 0, 500, MON));
        assert!(!g.hit(t + ms(900), Side::Left, 0, 500, MON), "too slow");
        assert!(!g.hit(t + ms(1000), Side::Right, 1919, 500, MON), "other side does not count");
    }

    #[test]
    fn dead_corners() {
        let mut g = EdgeGate::new(EdgeSettings { dead_corner_px: 50, ..Default::default() });
        let t = Instant::now();
        assert!(!g.hit(t, Side::Right, 1919, 10, MON));
        assert!(!g.hit(t, Side::Right, 1919, 1070, MON));
        assert!(g.hit(t, Side::Right, 1919, 500, MON));
        assert!(!g.hit(t, Side::Top, 20, 0, MON));
        assert!(g.hit(t, Side::Top, 200, 0, MON));
    }
}
