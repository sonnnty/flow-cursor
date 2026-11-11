//! Where the local cursor goes while this machine's devices drive another
//! machine (the `cursor_park` setting). Only the decision lives here;
//! backends move, hide and show the cursor ([`crate::backend::Backend::park_cursor`]).
//!
//! * leaving: the setting, reduced to what the backend can do
//!   ([`CursorPark::nearest`]), says whether the cursor stays at the edge or
//!   jumps to the centre of the monitor it left from, and whether it is
//!   hidden;
//! * coming back over an edge: the cursor enters at the position mapped from
//!   where it left the other machine (never at the centre) and is shown;
//! * every other way back (emergency hotkey, lost link, away, pause, hub
//!   request, agent stop): the cursor is shown where it is (the centre, or
//!   the edge it left from).

use flow_proto::{CursorPark, CursorSupport, Monitor, Rect};

/// What to do with the local cursor when control leaves this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Park {
    /// Move the cursor here (local desktop coordinates).
    pub at: (i32, i32),
    /// Hide it until control comes back.
    pub hide: bool,
}

/// Decides for a crossing out of this machine at the local edge position
/// (`x`, `y`). `None`: leave the cursor where it is, at the edge.
pub fn plan(want: CursorPark, support: CursorSupport, monitors: &[Monitor], x: i32, y: i32) -> Option<Park> {
    let mode = want.nearest(support);
    if !mode.moves() {
        return None;
    }
    let r = monitor_left_from(monitors, x, y)?;
    Some(Park { at: center(&r), hide: mode.hides() })
}

/// The monitor the pointer left from: the one containing the edge
/// position, else the nearest one (backends report the edge pixel itself
/// or the barrier line, which can lie just outside a monitor).
fn monitor_left_from(monitors: &[Monitor], x: i32, y: i32) -> Option<Rect> {
    let dist = |r: &Rect| {
        let dx = (r.x - x).max(0).max(x - (r.right() - 1)) as i64;
        let dy = (r.y - y).max(0).max(y - (r.bottom() - 1)) as i64;
        dx * dx + dy * dy
    };
    monitors.iter().map(|m| m.rect).filter(|r| r.w > 0 && r.h > 0).min_by_key(dist)
}

fn center(r: &Rect) -> (i32, i32) {
    (r.x + r.w / 2, r.y + r.h / 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mons() -> Vec<Monitor> {
        // A 2560x1440 primary with a 1920x1080 monitor to its right,
        // bottom-aligned.
        vec![
            Monitor { name: "a".into(), rect: Rect::new(0, 0, 2560, 1440), primary: true },
            Monitor { name: "b".into(), rect: Rect::new(2560, 360, 1920, 1080), primary: false },
        ]
    }

    #[test]
    fn edge_leaves_the_cursor_alone() {
        assert_eq!(plan(CursorPark::Edge, CursorSupport::FULL, &mons(), 4479, 800), None);
    }

    #[test]
    fn centre_of_the_monitor_it_left_from() {
        let p = plan(CursorPark::Center, CursorSupport::FULL, &mons(), 4479, 800).unwrap();
        assert_eq!(p, Park { at: (2560 + 960, 360 + 540), hide: false });
        let p = plan(CursorPark::CenterHide, CursorSupport::FULL, &mons(), 0, 100).unwrap();
        assert_eq!(p, Park { at: (1280, 720), hide: true });
    }

    #[test]
    fn edge_positions_just_outside_pick_the_nearest_monitor() {
        // A barrier line at x = 4480 (right of monitor b) and the top edge
        // of the primary at y = -1.
        assert_eq!(plan(CursorPark::Center, CursorSupport::FULL, &mons(), 4480, 500).unwrap().at, (3520, 900));
        assert_eq!(plan(CursorPark::Center, CursorSupport::FULL, &mons(), 100, -1).unwrap().at, (1280, 720));
    }

    #[test]
    fn unsupported_choices_fall_back() {
        let move_only = CursorSupport { center: true, hide: false };
        let p = plan(CursorPark::CenterHide, move_only, &mons(), 0, 100).unwrap();
        assert!(!p.hide, "cannot hide: centre only");
        // GNOME/Wayland: neither is possible, the cursor stays at the edge.
        assert_eq!(plan(CursorPark::CenterHide, CursorSupport::default(), &mons(), 0, 100), None);
        // No monitors known: nothing to aim at.
        assert_eq!(plan(CursorPark::CenterHide, CursorSupport::FULL, &[], 0, 100), None);
    }
}
