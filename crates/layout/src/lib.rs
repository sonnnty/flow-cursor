//! Layout engine.
//!
//! Every node has one or more monitors in its own desktop coordinates; the
//! hub places each node at an offset in a shared *global* plane. This crate
//! answers three questions:
//!
//! * which edges of a node lead to another node ([`Layout::edges_of`], used
//!   for Wayland pointer barriers and Windows edge detection),
//! * where the pointer enters when it crosses such an edge
//!   ([`Layout::crossing`]),
//! * how a remotely driven (virtual) cursor moves inside a node and when it
//!   leaves it ([`Layout::move_cursor`]).
//!
//! ## Proportional mapping
//!
//! For a crossing from node A (side S) to node B, let `seg_A` be the part
//! of A's edge that belongs to B and `seg_B` the part of B's opposite edge
//! that belongs to A. A side with a single neighbour owns its whole extent
//! (bounding box of all monitors); a side shared by several neighbours is
//! split by the geometric overlap. The crossing point is mapped
//! proportionally from `seg_A` onto `seg_B`. With one neighbour on each
//! side this maps "50 % down A's right edge" to "50 % down B's left edge"
//! regardless of resolutions; with several neighbours on one side it
//! degrades gracefully to geometric mapping inside each overlap. The
//! mapping is symmetric, so crossing back returns to the same spot.

use flow_proto::{Layout, NodeId, NodeLayout, Rect};
use serde::{Deserialize, Serialize};

/// Nodes whose facing edges are closer than this are treated as touching
/// even if they overlap slightly.
pub const OVERLAP_TOLERANCE: i32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub const ALL: [Side; 4] = [Side::Left, Side::Right, Side::Top, Side::Bottom];

    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }

    pub fn is_horizontal(self) -> bool {
        matches!(self, Side::Left | Side::Right)
    }
}

/// A segment of a node's screen edge that leads to another node, in the
/// node's local coordinates. For Left/Right `pos` is an x coordinate and
/// `start..end` a y range; for Top/Bottom vice versa. `pos` is the first
/// coordinate *outside* the monitor (e.g. `right()` for Right).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    pub side: Side,
    pub pos: i32,
    pub start: i32,
    pub end: i32,
    pub to: NodeId,
}

/// Result of moving a virtual cursor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Move {
    /// Still on the same node, at these local coordinates.
    Inside { x: f64, y: f64 },
    /// Crossed into `to`, entering at these coordinates (local to `to`).
    Cross { to: NodeId, x: f64, y: f64, side: Side },
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Interval {
    a: f64,
    b: f64,
}

impl Interval {
    fn len(&self) -> f64 {
        self.b - self.a
    }
    fn intersect(&self, o: &Interval) -> Option<Interval> {
        let a = self.a.max(o.a);
        let b = self.b.min(o.b);
        (b > a).then_some(Interval { a, b })
    }
    fn contains(&self, p: f64) -> bool {
        p >= self.a && p < self.b
    }
}

fn perp_extent(r: &Rect, side: Side) -> Interval {
    if side.is_horizontal() {
        Interval { a: r.y as f64, b: r.bottom() as f64 }
    } else {
        Interval { a: r.x as f64, b: r.right() as f64 }
    }
}

/// Global rectangles of a node's monitors.
pub fn global_monitors(n: &NodeLayout) -> impl Iterator<Item = Rect> + '_ {
    n.monitors.iter().map(move |m| m.rect.translate(n.x, n.y))
}

/// Global bounding box of a node (None if it has no monitors).
pub fn bbox(n: &NodeLayout) -> Option<Rect> {
    global_monitors(n).reduce(|a, b| a.union(&b))
}

fn clamp_into(r: &Rect, x: f64, y: f64) -> (f64, f64) {
    (x.clamp(r.x as f64, (r.right() - 1) as f64), y.clamp(r.y as f64, (r.bottom() - 1) as f64))
}

fn dist_to_rect(r: &Rect, x: f64, y: f64) -> f64 {
    let (cx, cy) = clamp_into(r, x, y);
    (cx - x).abs() + (cy - y).abs()
}

pub trait LayoutExt {
    fn node(&self, id: NodeId) -> Option<&NodeLayout>;
    fn neighbors(&self, id: NodeId, side: Side) -> Vec<NodeId>;
    fn crossing(&self, from: NodeId, gx: f64, gy: f64, side: Side) -> Option<(NodeId, f64, f64)>;
    fn crossing_local(&self, from: NodeId, x: f64, y: f64, side: Side) -> Option<(NodeId, f64, f64)>;
    fn edges_of(&self, id: NodeId) -> Vec<Edge>;
    fn move_cursor(&self, node: NodeId, x: f64, y: f64, dx: f64, dy: f64) -> Move;
    fn place_new_node(&self, monitors: &[flow_proto::Monitor]) -> (i32, i32);
}

impl LayoutExt for Layout {
    fn node(&self, id: NodeId) -> Option<&NodeLayout> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Nodes adjacent to `id` on `side`: their bounding box lies beyond that
    /// side (allowing [`OVERLAP_TOLERANCE`]) and overlaps it perpendicularly.
    /// Sorted by distance, nearest first.
    fn neighbors(&self, id: NodeId, side: Side) -> Vec<NodeId> {
        let Some(a) = self.node(id).and_then(bbox) else { return vec![] };
        let ea = perp_extent(&a, side);
        let mut out: Vec<(i32, NodeId)> = self
            .nodes
            .iter()
            .filter(|n| n.id != id)
            .filter_map(|n| {
                let b = bbox(n)?;
                ea.intersect(&perp_extent(&b, side))?;
                let gap = match side {
                    Side::Right => b.x - a.right(),
                    Side::Left => a.x - b.right(),
                    Side::Bottom => b.y - a.bottom(),
                    Side::Top => a.y - b.bottom(),
                };
                (gap >= -OVERLAP_TOLERANCE).then_some((gap, n.id))
            })
            .collect();
        out.sort();
        out.into_iter().map(|(_, id)| id).collect()
    }

    /// Crossing from node `from` leaving on `side` at global point
    /// (`gx`,`gy`). Returns the target node and the entry point in the
    /// target's *local* coordinates.
    fn crossing(&self, from: NodeId, gx: f64, gy: f64, side: Side) -> Option<(NodeId, f64, f64)> {
        let a_node = self.node(from)?;
        let a = bbox(a_node)?;
        let p = if side.is_horizontal() { gy } else { gx };
        let neigh = self.neighbors(from, side);
        for bid in &neigh {
            let b_node = self.node(*bid)?;
            let b = bbox(b_node)?;
            let ea = perp_extent(&a, side);
            let eb = perp_extent(&b, side);
            let overlap = ea.intersect(&eb)?;
            let seg_a = if neigh.len() == 1 { ea } else { overlap };
            if !seg_a.contains(p) && !(neigh.len() == 1 && p >= seg_a.a && p <= seg_a.b) {
                continue;
            }
            let back = self.neighbors(*bid, side.opposite());
            let seg_b = if back.len() == 1 { eb } else { overlap };
            let f = ((p - seg_a.a) / seg_a.len()).clamp(0.0, 1.0);
            let p2 = (seg_b.a + f * seg_b.len()).min(seg_b.b - 1.0);
            // Entry monitor: the one containing p2 on the facing side.
            let (gx2, gy2) = entry_point(b_node, side, p2)?;
            return Some((*bid, gx2 - b_node.x as f64, gy2 - b_node.y as f64));
        }
        None
    }

    fn crossing_local(&self, from: NodeId, x: f64, y: f64, side: Side) -> Option<(NodeId, f64, f64)> {
        let n = self.node(from)?;
        self.crossing(from, x + n.x as f64, y + n.y as f64, side)
    }

    /// Edge segments (local coordinates) of `id` that lead to another node.
    fn edges_of(&self, id: NodeId) -> Vec<Edge> {
        let Some(n) = self.node(id) else { return vec![] };
        let mons: Vec<Rect> = n.monitors.iter().map(|m| m.rect).collect();
        let mut out = Vec::new();
        for side in Side::ALL {
            for m in &mons {
                // Candidate segment: whole monitor edge, minus parts covered by
                // another own monitor directly beyond it.
                let (pos, ext) = match side {
                    Side::Right => (m.right(), Interval { a: m.y as f64, b: m.bottom() as f64 }),
                    Side::Left => (m.x, Interval { a: m.y as f64, b: m.bottom() as f64 }),
                    Side::Bottom => (m.bottom(), Interval { a: m.x as f64, b: m.right() as f64 }),
                    Side::Top => (m.y, Interval { a: m.x as f64, b: m.right() as f64 }),
                };
                let mut free = vec![ext];
                for o in mons.iter().filter(|o| *o != m) {
                    let touches = match side {
                        Side::Right => o.x == m.right(),
                        Side::Left => o.right() == m.x,
                        Side::Bottom => o.y == m.bottom(),
                        Side::Top => o.bottom() == m.y,
                    };
                    if touches {
                        free = subtract(&free, &perp_extent(o, side));
                    }
                }
                // Walk the free parts with a step and ask `crossing` which
                // neighbour (if any) each sub-range leads to. Split points are
                // the boundaries of neighbour overlaps, so we compute them
                // explicitly instead of sampling.
                let mut cuts: Vec<f64> = Vec::new();
                for f in &free {
                    cuts.push(f.a);
                    cuts.push(f.b);
                }
                for bid in self.neighbors(id, side) {
                    if let Some(b) = self.node(bid).and_then(bbox) {
                        let e = perp_extent(&b, side);
                        let off = if side.is_horizontal() { n.y as f64 } else { n.x as f64 };
                        cuts.push(e.a - off);
                        cuts.push(e.b - off);
                    }
                }
                cuts.sort_by(|a, b| a.partial_cmp(b).unwrap());
                cuts.dedup();
                for w in cuts.windows(2) {
                    let seg = Interval { a: w[0], b: w[1] };
                    if seg.len() <= 0.0 || !free.iter().any(|f| f.contains(seg.a)) {
                        continue;
                    }
                    let mid = (seg.a + seg.b) / 2.0;
                    let probe = if side.is_horizontal() { (pos as f64, mid) } else { (mid, pos as f64) };
                    if let Some((to, _, _)) = self.crossing_local(id, probe.0, probe.1, side) {
                        push_merged(&mut out, Edge { side, pos, start: seg.a as i32, end: seg.b as i32, to });
                    }
                }
            }
        }
        out
    }

    /// Moves a cursor at local (`x`,`y`) of `node` by (`dx`,`dy`).
    fn move_cursor(&self, node: NodeId, x: f64, y: f64, dx: f64, dy: f64) -> Move {
        let Some(n) = self.node(node) else { return Move::Inside { x, y } };
        let mons: Vec<Rect> = n.monitors.iter().map(|m| m.rect).collect();
        if mons.is_empty() {
            return Move::Inside { x, y };
        }
        let (nx, ny) = (x + dx, y + dy);
        if mons.iter().any(|m| m.contains(nx, ny)) {
            return Move::Inside { x: nx, y: ny };
        }
        let m = mons
            .iter()
            .find(|m| m.contains(x, y))
            .or_else(|| mons.iter().min_by(|a, b| dist_to_rect(a, x, y).total_cmp(&dist_to_rect(b, x, y))))
            .copied()
            .expect("non-empty");
        let mut sides: Vec<(f64, Side)> = Vec::new();
        if nx >= m.right() as f64 {
            sides.push((nx - m.right() as f64 + 1.0, Side::Right));
        }
        if nx < m.x as f64 {
            sides.push((m.x as f64 - nx, Side::Left));
        }
        if ny >= m.bottom() as f64 {
            sides.push((ny - m.bottom() as f64 + 1.0, Side::Bottom));
        }
        if ny < m.y as f64 {
            sides.push((m.y as f64 - ny, Side::Top));
        }
        sides.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (_, side) in sides {
            let (ex, ey) = clamp_into(&m, nx, ny);
            // Beyond this edge another own monitor? Then it is not a crossing.
            let (bx, by) = match side {
                Side::Right => (m.right() as f64, ey),
                Side::Left => (m.x as f64 - 1.0, ey),
                Side::Bottom => (ex, m.bottom() as f64),
                Side::Top => (ex, m.y as f64 - 1.0),
            };
            if mons.iter().any(|o| o.contains(bx, by)) {
                continue;
            }
            if let Some((to, tx, ty)) = self.crossing_local(node, ex, ey, side) {
                return Move::Cross { to, x: tx, y: ty, side };
            }
        }
        // No crossing: slide along walls.
        if let Some(o) = mons.iter().find(|o| o.contains(nx, y)) {
            let (_, cy) = clamp_into(o, nx, ny);
            return Move::Inside { x: nx, y: cy };
        }
        if let Some(o) = mons.iter().find(|o| o.contains(x, ny)) {
            let (cx, _) = clamp_into(o, nx, ny);
            return Move::Inside { x: cx, y: ny };
        }
        let (cx, cy) = clamp_into(&m, nx, ny);
        Move::Inside { x: cx, y: cy }
    }

    /// Default position for a newly enrolled node: right of everything,
    /// top-aligned with the first node.
    fn place_new_node(&self, _monitors: &[flow_proto::Monitor]) -> (i32, i32) {
        let boxes: Vec<Rect> = self.nodes.iter().filter_map(bbox).collect();
        match boxes.iter().copied().reduce(|a, b| a.union(&b)) {
            None => (0, 0),
            Some(u) => (u.right(), boxes[0].y),
        }
    }
}

/// Point on the facing side of node `b` (global coords) at perpendicular
/// coordinate `p` when entering through `side` of the *source*.
fn entry_point(b: &NodeLayout, side: Side, p: f64) -> Option<(f64, f64)> {
    let mons: Vec<Rect> = global_monitors(b).collect();
    let in_range: Vec<&Rect> = mons.iter().filter(|m| perp_extent(m, side).contains(p)).collect();
    let pick = |cands: &[&Rect]| -> Option<Rect> {
        cands
            .iter()
            .copied()
            .min_by_key(|m| match side {
                Side::Right => m.x,       // entering from the left: leftmost
                Side::Left => -m.right(), // entering from the right: rightmost
                Side::Bottom => m.y,      // entering from the top: topmost
                Side::Top => -m.bottom(), // entering from the bottom: lowest
            })
            .copied()
    };
    let (m, p) = match pick(&in_range) {
        Some(m) => (m, p),
        None => {
            // Gap in the target's monitors: snap to the closest monitor.
            let m = *mons.iter().min_by(|a, c| {
                let da = interval_dist(&perp_extent(a, side), p);
                let dc = interval_dist(&perp_extent(c, side), p);
                da.total_cmp(&dc)
            })?;
            let e = perp_extent(&m, side);
            (m, p.clamp(e.a, e.b - 1.0))
        }
    };
    Some(match side {
        Side::Right => (m.x as f64, p),
        Side::Left => ((m.right() - 1) as f64, p),
        Side::Bottom => (p, m.y as f64),
        Side::Top => (p, (m.bottom() - 1) as f64),
    })
}

fn interval_dist(i: &Interval, p: f64) -> f64 {
    if p < i.a {
        i.a - p
    } else if p >= i.b {
        p - i.b + 1.0
    } else {
        0.0
    }
}

fn subtract(parts: &[Interval], cut: &Interval) -> Vec<Interval> {
    let mut out = Vec::new();
    for p in parts {
        if p.intersect(cut).is_none() {
            out.push(*p);
            continue;
        }
        if cut.a > p.a {
            out.push(Interval { a: p.a, b: cut.a });
        }
        if cut.b < p.b {
            out.push(Interval { a: cut.b, b: p.b });
        }
    }
    out
}

fn push_merged(out: &mut Vec<Edge>, e: Edge) {
    if let Some(last) = out.last_mut() {
        if last.side == e.side && last.pos == e.pos && last.to == e.to && last.end == e.start {
            last.end = e.end;
            return;
        }
    }
    out.push(e);
}

#[cfg(test)]
mod tests;
