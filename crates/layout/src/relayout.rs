//! Keeping the layout usable when a node's monitors change (a laptop is
//! docked or undocked, a screen is rotated), and finding layouts in which
//! crossing cannot work.
//!
//! ## The rule ([`apply_monitors`])
//!
//! The changed node keeps its own position (the origin of its desktop).
//!
//! 1. **Shared edges are kept.** A node whose bounding box touched one side
//!    of the changed node's old bounding box (facing edges at most
//!    [`OVERLAP_TOLERANCE`] apart, overlapping along the edge) moves with
//!    that side: the PC that was right of the laptop stays right of the
//!    laptop's new width, a screen below stays below its new height.
//!    Everything attached to such a node (touching it, transitively, but
//!    not through the changed node) moves along as one block, so a row of
//!    machines stays a row. If the node no longer overlaps the changed node
//!    along that edge after the move (the changed node got much smaller),
//!    the block also slides along the edge until their starts line up
//!    (tops for left/right, left sides for top/bottom). A block that would
//!    have to move two different ways (it touched two sides) stays where it
//!    is.
//! 2. **Nothing overlaps.** Then every node that overlaps the changed node,
//!    or a node moved by this rule, is pushed out the shortest way (right,
//!    left, down or up, in that order on a tie) so that it just touches,
//!    preferring a way that does not run into another placed node. Pushed
//!    nodes count as placed, so a push can cascade.
//!
//! Nodes that neither touched the changed node nor overlap anything do not
//! move. Overlaps between two nodes that this rule did not touch (placed
//! that way in the dashboard) are left alone; [`overlaps`] reports them.

use crate::{bbox, LayoutExt, OVERLAP_TOLERANCE};
use flow_proto::{Layout, Monitor, NodeId, Rect};
use std::collections::{BTreeMap, BTreeSet};

/// A node the rule moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Moved {
    pub id: NodeId,
    pub from: (i32, i32),
    pub to: (i32, i32),
}

/// Two nodes overlap when their bounding boxes share more than
/// [`OVERLAP_TOLERANCE`] pixels in both directions (less counts as
/// touching, like [`LayoutExt::neighbors`]).
pub fn boxes_overlap(a: &Rect, b: &Rect) -> bool {
    let w = a.right().min(b.right()) - a.x.max(b.x);
    let h = a.bottom().min(b.bottom()) - a.y.max(b.y);
    w > OVERLAP_TOLERANCE && h > OVERLAP_TOLERANCE
}

/// The side of `a` that `b` touches: facing edges at most
/// [`OVERLAP_TOLERANCE`] apart and a positive overlap along them.
pub fn touching_side(a: &Rect, b: &Rect) -> Option<crate::Side> {
    use crate::Side::*;
    let along_y = a.bottom().min(b.bottom()) > a.y.max(b.y);
    let along_x = a.right().min(b.right()) > a.x.max(b.x);
    let near = |d: i32| d.abs() <= OVERLAP_TOLERANCE;
    if along_y && near(b.x - a.right()) {
        Some(Right)
    } else if along_y && near(a.x - b.right()) {
        Some(Left)
    } else if along_x && near(b.y - a.bottom()) {
        Some(Bottom)
    } else if along_x && near(a.y - b.bottom()) {
        Some(Top)
    } else {
        None
    }
}

/// Pairs of nodes that overlap (each pair once, lower id first).
pub fn overlaps(layout: &Layout) -> Vec<(NodeId, NodeId)> {
    let boxes: Vec<(NodeId, Rect)> = layout.nodes.iter().filter_map(|n| Some((n.id, bbox(n)?))).collect();
    let mut out = Vec::new();
    for (i, (a, ra)) in boxes.iter().enumerate() {
        for (b, rb) in &boxes[i + 1..] {
            if boxes_overlap(ra, rb) {
                out.push(if a < b { (*a, *b) } else { (*b, *a) });
            }
        }
    }
    out.sort();
    out
}

/// Nodes of a layout with at least two nodes that have no edge leading to
/// another node: the pointer can neither leave nor enter them.
pub fn isolated(layout: &Layout) -> Vec<NodeId> {
    let with_screens: Vec<NodeId> = layout.nodes.iter().filter(|n| bbox(n).is_some()).map(|n| n.id).collect();
    if with_screens.len() < 2 {
        return Vec::new();
    }
    with_screens.into_iter().filter(|id| layout.edges_of(*id).is_empty()).collect()
}

/// Replaces the monitors of node `id` and moves other nodes so that the
/// layout stays usable (see the module documentation). Returns the nodes
/// that moved. A node that is not in the layout only gets nothing: the
/// caller adds it.
pub fn apply_monitors(layout: &mut Layout, id: NodeId, monitors: Vec<Monitor>) -> Vec<Moved> {
    let Some(idx) = layout.nodes.iter().position(|n| n.id == id) else { return Vec::new() };
    let old_box = bbox(&layout.nodes[idx]);
    layout.nodes[idx].monitors = monitors;
    let Some(new_box) = bbox(&layout.nodes[idx]) else { return Vec::new() };
    let start: BTreeMap<NodeId, (i32, i32)> = layout.nodes.iter().map(|n| (n.id, (n.x, n.y))).collect();
    let mut placed = vec![id];
    if let Some(old_box) = old_box.filter(|b| *b != new_box) {
        placed.extend(follow_shared_edges(layout, id, &old_box, &new_box));
    }
    separate(layout, placed);
    layout
        .nodes
        .iter()
        .filter_map(|n| {
            let from = start[&n.id];
            ((n.x, n.y) != from).then_some(Moved { id: n.id, from, to: (n.x, n.y) })
        })
        .collect()
}

/// Pushes nodes that overlap node `id` out of the way (rule step 2 alone),
/// e.g. when an agent reports the monitors the hub already had but the
/// layout was saved with an overlap.
pub fn separate_from(layout: &mut Layout, id: NodeId) -> Vec<Moved> {
    let start: BTreeMap<NodeId, (i32, i32)> = layout.nodes.iter().map(|n| (n.id, (n.x, n.y))).collect();
    separate(layout, vec![id]);
    layout
        .nodes
        .iter()
        .filter_map(|n| {
            let from = start[&n.id];
            ((n.x, n.y) != from).then_some(Moved { id: n.id, from, to: (n.x, n.y) })
        })
        .collect()
}

/// Step 1: moves the blocks that touched `old` with the side they touched.
/// Returns the nodes it moved.
fn follow_shared_edges(layout: &mut Layout, id: NodeId, old: &Rect, new: &Rect) -> Vec<NodeId> {
    use crate::Side::*;
    let boxes: BTreeMap<NodeId, Rect> =
        layout.nodes.iter().filter(|n| n.id != id).filter_map(|n| Some((n.id, bbox(n)?))).collect();
    // Blocks: nodes connected by shared edges, not through the changed node.
    let mut block_of: BTreeMap<NodeId, usize> = BTreeMap::new();
    let mut blocks: Vec<Vec<NodeId>> = Vec::new();
    for &start in boxes.keys() {
        if block_of.contains_key(&start) {
            continue;
        }
        let mut members = vec![start];
        block_of.insert(start, blocks.len());
        let mut i = 0;
        while i < members.len() {
            let a = boxes[&members[i]];
            for (&o, b) in &boxes {
                if !block_of.contains_key(&o) && touching_side(&a, b).is_some() {
                    block_of.insert(o, blocks.len());
                    members.push(o);
                }
            }
            i += 1;
        }
        blocks.push(members);
    }
    // The move each block wants, from each of its nodes that touched `old`.
    let mut wanted: BTreeMap<usize, BTreeSet<(i32, i32)>> = BTreeMap::new();
    for (&n, b) in &boxes {
        let Some(side) = touching_side(old, b) else { continue };
        let (dx, dy) = match side {
            Right => (new.right() - old.right(), 0),
            Left => (new.x - old.x, 0),
            Bottom => (0, new.bottom() - old.bottom()),
            Top => (0, new.y - old.y),
        };
        // Still facing the changed node along the shared edge?
        let moved = b.translate(dx, dy);
        let (dx, dy) = if side.is_horizontal() {
            let along = moved.bottom().min(new.bottom()) > moved.y.max(new.y);
            (dx, if along { dy } else { new.y - moved.y })
        } else {
            let along = moved.right().min(new.right()) > moved.x.max(new.x);
            (if along { dx } else { new.x - moved.x }, dy)
        };
        wanted.entry(block_of[&n]).or_default().insert((dx, dy));
    }
    let mut out = Vec::new();
    for (block, moves) in wanted {
        if moves.len() != 1 {
            continue; // touched two sides: ambiguous, step 2 decides
        }
        let (dx, dy) = *moves.iter().next().expect("one");
        if (dx, dy) == (0, 0) {
            continue;
        }
        for n in layout.nodes.iter_mut().filter(|n| blocks[block].contains(&n.id)) {
            n.x += dx;
            n.y += dy;
            out.push(n.id);
        }
    }
    out
}

/// Step 2: pushes every node that overlaps a placed node out of it, the
/// shortest way, until no placed node overlaps an unplaced one.
fn separate(layout: &mut Layout, mut placed: Vec<NodeId>) {
    // Each push places one more node, so this ends.
    for _ in 0..layout.nodes.len() {
        let boxes: Vec<(NodeId, Rect)> = layout.nodes.iter().filter_map(|n| Some((n.id, bbox(n)?))).collect();
        let rect = |id: NodeId| boxes.iter().find(|(n, _)| *n == id).map(|(_, r)| *r);
        let hit = placed.iter().find_map(|&p| {
            let a = rect(p)?;
            boxes.iter().find(|(o, b)| !placed.contains(o) && boxes_overlap(&a, b)).map(|(o, b)| (a, *o, *b))
        });
        let Some((a, other, b)) = hit else { return };
        let mut ways = [(a.right() - b.x, 0), (a.x - b.right(), 0), (0, a.bottom() - b.y), (0, a.y - b.bottom())];
        // Stable sort: the order above breaks ties.
        ways.sort_by_key(|(dx, dy)| dx.abs() + dy.abs());
        let clear = |&(dx, dy): &(i32, i32)| {
            let moved = b.translate(dx, dy);
            placed.iter().filter_map(|&p| rect(p)).all(|r| !boxes_overlap(&r, &moved))
        };
        let (dx, dy) = ways.iter().copied().find(clear).unwrap_or(ways[0]);
        if let Some(n) = layout.nodes.iter_mut().find(|n| n.id == other) {
            n.x += dx;
            n.y += dy;
        }
        placed.push(other);
    }
}
