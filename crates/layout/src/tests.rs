use super::*;
use flow_proto::Monitor;

fn mon(x: i32, y: i32, w: i32, h: i32) -> Monitor {
    Monitor { name: format!("{w}x{h}@{x},{y}"), rect: Rect::new(x, y, w, h), primary: false }
}

fn node(id: u32, x: i32, y: i32, monitors: Vec<Monitor>) -> NodeLayout {
    NodeLayout { id: NodeId(id), x, y, monitors }
}

const A: NodeId = NodeId(1);
const B: NodeId = NodeId(2);
const C: NodeId = NodeId(3);

fn approx(a: (NodeId, f64, f64), b: (NodeId, f64, f64)) {
    assert_eq!(a.0, b.0);
    assert!((a.1 - b.1).abs() < 1.0 && (a.2 - b.2).abs() < 1.0, "{a:?} != {b:?}");
}

#[test]
fn side_by_side_same_resolution() {
    let l = Layout {
        nodes: vec![node(1, 0, 0, vec![mon(0, 0, 1920, 1080)]), node(2, 1920, 0, vec![mon(0, 0, 1920, 1080)])],
    };
    assert_eq!(l.neighbors(A, Side::Right), vec![B]);
    assert_eq!(l.neighbors(A, Side::Left), vec![]);
    assert_eq!(l.neighbors(B, Side::Left), vec![A]);
    approx(l.crossing_local(A, 1919.0, 540.0, Side::Right).unwrap(), (B, 0.0, 540.0));
    approx(l.crossing_local(B, 0.0, 540.0, Side::Left).unwrap(), (A, 1919.0, 540.0));
    assert!(l.crossing_local(A, 0.0, 540.0, Side::Left).is_none());
    assert!(l.crossing_local(A, 500.0, 0.0, Side::Top).is_none());
}

#[test]
fn proportional_mapping_between_different_resolutions() {
    // 1080p next to a 1440p screen, top aligned.
    let l = Layout {
        nodes: vec![node(1, 0, 0, vec![mon(0, 0, 1920, 1080)]), node(2, 1920, 0, vec![mon(0, 0, 2560, 1440)])],
    };
    approx(l.crossing_local(A, 1919.0, 540.0, Side::Right).unwrap(), (B, 0.0, 720.0));
    approx(l.crossing_local(A, 1919.0, 0.0, Side::Right).unwrap(), (B, 0.0, 0.0));
    approx(l.crossing_local(A, 1919.0, 1079.0, Side::Right).unwrap(), (B, 0.0, 1438.7));
    // Symmetric: crossing back lands where we came from.
    approx(l.crossing_local(B, 0.0, 720.0, Side::Left).unwrap(), (A, 1919.0, 540.0));
    approx(l.crossing_local(B, 0.0, 1439.0, Side::Left).unwrap(), (A, 1919.0, 1079.25));
}

#[test]
fn proportional_mapping_with_offset_nodes() {
    // B is a 4K screen (logical 3840x2160) placed lower than A.
    let l = Layout {
        nodes: vec![node(1, 0, 0, vec![mon(0, 0, 1920, 1080)]), node(2, 1920, 500, vec![mon(0, 0, 3840, 2160)])],
    };
    approx(l.crossing_local(A, 1919.0, 270.0, Side::Right).unwrap(), (B, 0.0, 540.0));
    approx(l.crossing_local(B, 0.0, 1080.0, Side::Left).unwrap(), (A, 1919.0, 540.0));
}

#[test]
fn multi_monitor_source_and_target() {
    // A: 1080p + 1440p side by side (different heights). B: 1366x768 laptop
    // above-left monitor arrangement on the right of A.
    let l = Layout {
        nodes: vec![
            node(1, 0, 0, vec![mon(0, 0, 1920, 1080), mon(1920, 0, 2560, 1440)]),
            node(2, 4480, 0, vec![mon(0, 0, 1366, 768), mon(0, 768, 1366, 768)]),
        ],
    };
    // Leaving A's first monitor to the right is not a crossing (own monitor).
    let mv = l.move_cursor(A, 1919.0, 100.0, 5.0, 0.0);
    assert_eq!(mv, Move::Inside { x: 1924.0, y: 100.0 });
    // A bbox height 1440, B bbox height 1536 → proportional.
    approx(l.crossing_local(A, 4479.0, 720.0, Side::Right).unwrap(), (B, 0.0, 768.0));
    // Entering lands on B's second monitor when below 768.
    let (to, x, y) = l.crossing_local(A, 4479.0, 1200.0, Side::Right).unwrap();
    assert_eq!(to, B);
    assert!(x == 0.0 && y > 768.0);
    // From B back to A, lands on A's 1440p monitor.
    let (to, x, _) = l.crossing_local(B, 0.0, 768.0, Side::Left).unwrap();
    assert_eq!(to, A);
    assert_eq!(x, 4479.0);
    // Moving within A from the tall monitor into the gap below the short one
    // slides along the wall instead of escaping.
    let mv = l.move_cursor(A, 1930.0, 1200.0, -20.0, 0.0);
    assert_eq!(mv, Move::Inside { x: 1920.0, y: 1200.0 });
}

#[test]
fn two_neighbours_on_one_side_split_geometrically() {
    let l = Layout {
        nodes: vec![
            node(1, 0, 0, vec![mon(0, 0, 1920, 2160)]),
            node(2, 1920, 0, vec![mon(0, 0, 1920, 1080)]),
            node(3, 1920, 1080, vec![mon(0, 0, 1920, 1080)]),
        ],
    };
    assert_eq!(l.neighbors(A, Side::Right).len(), 2);
    approx(l.crossing_local(A, 1919.0, 500.0, Side::Right).unwrap(), (B, 0.0, 500.0));
    approx(l.crossing_local(A, 1919.0, 1500.0, Side::Right).unwrap(), (C, 0.0, 420.0));
    // B and C each have only A on their left → their whole edge maps to their overlap.
    approx(l.crossing_local(C, 0.0, 420.0, Side::Left).unwrap(), (A, 1919.0, 1500.0));
    // B is above C.
    approx(l.crossing_local(B, 960.0, 1079.0, Side::Bottom).unwrap(), (C, 960.0, 0.0));
    let edges = l.edges_of(A);
    assert_eq!(
        edges,
        vec![
            Edge { side: Side::Right, pos: 1920, start: 0, end: 1080, to: B },
            Edge { side: Side::Right, pos: 1920, start: 1080, end: 2160, to: C },
        ]
    );
}

#[test]
fn vertical_stack_and_edges() {
    let l = Layout {
        nodes: vec![node(1, 0, 0, vec![mon(0, 0, 1920, 1080)]), node(2, 320, 1080, vec![mon(0, 0, 1280, 800)])],
    };
    approx(l.crossing_local(A, 960.0, 1079.0, Side::Bottom).unwrap(), (B, 640.0, 0.0));
    approx(l.crossing_local(B, 640.0, 0.0, Side::Top).unwrap(), (A, 960.0, 1079.0));
    assert_eq!(l.edges_of(A), vec![Edge { side: Side::Bottom, pos: 1080, start: 0, end: 1920, to: B }]);
    assert_eq!(l.edges_of(B), vec![Edge { side: Side::Top, pos: 0, start: 0, end: 1280, to: A }]);
}

#[test]
fn edges_skip_internal_monitor_boundaries() {
    let l = Layout {
        nodes: vec![
            node(1, 0, 0, vec![mon(0, 0, 1920, 1080), mon(1920, 0, 1920, 1080)]),
            node(2, 3840, 0, vec![mon(0, 0, 1920, 1080)]),
            node(3, -1920, 0, vec![mon(0, 0, 1920, 1080)]),
        ],
    };
    let e = l.edges_of(A);
    assert_eq!(
        e,
        vec![
            Edge { side: Side::Left, pos: 0, start: 0, end: 1080, to: C },
            Edge { side: Side::Right, pos: 3840, start: 0, end: 1080, to: B },
        ]
    );
}

#[test]
fn virtual_cursor_moves_and_crosses() {
    let l = Layout {
        nodes: vec![node(1, 0, 0, vec![mon(0, 0, 1920, 1080)]), node(2, 1920, 0, vec![mon(0, 0, 2560, 1440)])],
    };
    // Inside.
    assert_eq!(l.move_cursor(B, 100.0, 100.0, 10.0, -5.0), Move::Inside { x: 110.0, y: 95.0 });
    // Clamped at a wall (top of B has no neighbour).
    assert_eq!(l.move_cursor(B, 100.0, 3.0, 0.0, -10.0), Move::Inside { x: 100.0, y: 0.0 });
    // Right wall of B.
    assert_eq!(l.move_cursor(B, 2555.0, 3.0, 50.0, 0.0), Move::Inside { x: 2559.0, y: 3.0 });
    // Leaving B to the left crosses to A with proportional y.
    match l.move_cursor(B, 2.0, 720.0, -5.0, 0.0) {
        Move::Cross { to, x, y, side } => {
            assert_eq!((to, side), (A, Side::Left));
            assert_eq!(x, 1919.0);
            assert!((y - 540.0).abs() < 1.0);
        }
        other => panic!("expected crossing, got {other:?}"),
    }
    // Diagonal motion into the corner: dominant axis decides.
    assert!(matches!(l.move_cursor(B, 1.0, 1.0, -30.0, -2.0), Move::Cross { to: A, .. }));
}

#[test]
fn unknown_or_empty_nodes_are_harmless() {
    let l = Layout { nodes: vec![node(1, 0, 0, vec![]), node(2, 0, 0, vec![mon(0, 0, 10, 10)])] };
    assert!(l.crossing_local(NodeId(9), 0.0, 0.0, Side::Right).is_none());
    assert_eq!(l.move_cursor(A, 1.0, 1.0, 5.0, 5.0), Move::Inside { x: 1.0, y: 1.0 });
    assert!(l.edges_of(A).is_empty());
    assert!(l.edges_of(NodeId(9)).is_empty());
}

#[test]
fn new_nodes_go_to_the_right() {
    let mut l = Layout::default();
    assert_eq!(l.place_new_node(&[]), (0, 0));
    l.nodes.push(node(1, 0, 0, vec![mon(0, 0, 1920, 1080)]));
    assert_eq!(l.place_new_node(&[]), (1920, 0));
}

#[test]
fn triple_monitor_and_4k_proportional_crossing() {
    // Node A: Dual monitor (1920x1080 + 2560x1440, bbox 4480x1440).
    // Node B: Triple monitor (1920x1080 + 3840x2160 + 1920x1080, bbox 7680x2160).
    // Mon 1 on B is centered vertically (y: 540..1620).
    let l = Layout {
        nodes: vec![
            node(1, 0, 0, vec![mon(0, 0, 1920, 1080), mon(1920, 0, 2560, 1440)]),
            node(2, 4480, 0, vec![mon(0, 540, 1920, 1080), mon(1920, 0, 3840, 2160), mon(5760, 540, 1920, 1080)]),
        ],
    };

    // A and B are adjacent
    assert_eq!(l.neighbors(A, Side::Right), vec![B]);
    assert_eq!(l.neighbors(B, Side::Left), vec![A]);

    // Test proportional mapping at 0%, 25%, 50%, 75%, 100% along the height
    for pct in [0.0f64, 0.25, 0.5, 0.75, 1.0] {
        let ya = (1440.0 * pct).min(1439.0);
        let crossing = l.crossing_local(A, 4479.0, ya, Side::Right).expect("A->B crossing");
        assert_eq!(crossing.0, B);
        let (xb, yb) = (crossing.1, crossing.2);

        // At yb where Mon 1 on B exists (y in 540..1620), entry is at x=0 (Mon 1).
        // Where Mon 1 does not exist (y < 540 or y >= 1620), entry snaps to Mon 2 at x=1920.
        if (540.0..1620.0).contains(&yb) {
            assert_eq!(xb, 0.0);
        } else {
            assert_eq!(xb, 1920.0);
        }

        // Crossing back from B to A must return to the exact same Y position on A
        let back = l.crossing_local(B, xb, yb, Side::Left).expect("B->A crossing");
        assert_eq!(back.0, A);
        assert_eq!(back.1, 4479.0); // right edge of A's second monitor
        assert!((back.2 - ya).abs() < 1.0, "roundtrip at {pct}: got {back:?}, expected ya={ya}");
    }
}

#[test]
fn l_shaped_dual_monitors_boundary_and_sliding() {
    // Node A: Monitor 1 (0, 0, 1920, 1080) and Monitor 2 (1920, 300, 1080, 1920) in portrait
    // Top of Node A has no neighbor; right of Monitor 2 has Node B
    let l = Layout {
        nodes: vec![
            node(1, 0, 0, vec![mon(0, 0, 1920, 1080), mon(1920, 300, 1080, 1920)]),
            node(2, 3000, 300, vec![mon(0, 0, 1920, 1080)]),
        ],
    };

    // Moving between internal monitors of Node A (at y=500 where they touch):
    // From Mon 1 rightwards into Mon 2 is inside Node A
    let mv = l.move_cursor(A, 1918.0, 500.0, 10.0, 0.0);
    assert_eq!(mv, Move::Inside { x: 1928.0, y: 500.0 });

    // From Mon 2 leftwards into Mon 1 (at y=500 where they touch)
    let mv = l.move_cursor(A, 1922.0, 500.0, -10.0, 0.0);
    assert_eq!(mv, Move::Inside { x: 1912.0, y: 500.0 });

    // Moving upwards from Mon 2 into the concave empty space (x=2000, y=305 moving by dy=-20)
    // There is no monitor and no neighbor above Mon 2 -> slides along the top wall of Mon 2
    let mv = l.move_cursor(A, 2000.0, 305.0, 0.0, -20.0);
    assert_eq!(mv, Move::Inside { x: 2000.0, y: 300.0 });

    // Moving leftwards from Mon 1 (where there is no neighbor on the left) slides along wall
    let mv = l.move_cursor(A, 5.0, 200.0, -20.0, 0.0);
    assert_eq!(mv, Move::Inside { x: 0.0, y: 200.0 });

    // Crossing from right edge of Mon 2 into Node B
    let mv = l.move_cursor(A, 2998.0, 1000.0, 10.0, 0.0);
    match mv {
        Move::Cross { to, side, .. } => {
            assert_eq!(to, B);
            assert_eq!(side, Side::Right);
        }
        other => panic!("expected crossing to B, got {other:?}"),
    }

    // Edges of A leading to B:
    // Mon 1 free right edge is pos=1920 (y: 0..300).
    // Shared border (y: 300..1080) between Mon 1 and Mon 2 is skipped.
    // Mon 2 right edge is pos=3000 (y: 300..2220).
    let edges = l.edges_of(A);
    assert_eq!(
        edges,
        vec![
            Edge { side: Side::Right, pos: 1920, start: 0, end: 300, to: B },
            Edge { side: Side::Right, pos: 3000, start: 300, end: 2220, to: B },
        ]
    );
}

#[test]
fn vertical_multi_monitor_different_resolutions() {
    // Node A (top): Two 1920x1080 monitors side by side (total width 3840, height 1080)
    // Node B (bottom): Ultrawide 3440x1440 at y: 1080, x: 200
    let l = Layout {
        nodes: vec![
            node(1, 0, 0, vec![mon(0, 0, 1920, 1080), mon(1920, 0, 1920, 1080)]),
            node(2, 200, 1080, vec![mon(0, 0, 3440, 1440)]),
        ],
    };

    assert_eq!(l.neighbors(A, Side::Bottom), vec![B]);
    assert_eq!(l.neighbors(B, Side::Top), vec![A]);

    // Crossing down from left monitor of A (e.g. x = 960)
    let (to, bx, by) = l.crossing_local(A, 960.0, 1079.0, Side::Bottom).expect("A->B bottom");
    assert_eq!(to, B);
    assert_eq!(by, 0.0); // enters at top edge of B

    // Symmetrical return: crossing up from B at bx should return close to 960
    let (back_to, ax, ay) = l.crossing_local(B, bx, 0.0, Side::Top).expect("B->A top");
    assert_eq!(back_to, A);
    assert_eq!(ay, 1079.0);
    assert!((ax - 960.0).abs() < 1.0, "ax={ax}, expected 960");

    // Crossing down from right monitor of A (e.g. x = 2880)
    let (to2, bx2, by2) = l.crossing_local(A, 2880.0, 1079.0, Side::Bottom).expect("A->B bottom right");
    assert_eq!(to2, B);
    assert_eq!(by2, 0.0);
    let (back_to2, ax2, _) = l.crossing_local(B, bx2, 0.0, Side::Top).expect("B->A top right");
    assert_eq!(back_to2, A);
    assert!((ax2 - 2880.0).abs() < 1.0, "ax2={ax2}, expected 2880");
}

#[test]
fn degenerate_and_empty_monitor_geometries() {
    // 0x0 monitor or negative dimensions should not cause a panic
    let l = Layout {
        nodes: vec![
            node(1, 0, 0, vec![mon(0, 0, 0, 0)]),
            node(2, 0, 0, vec![mon(0, 0, -100, -100)]),
            node(3, 100, 0, vec![mon(0, 0, 1920, 1080)]),
        ],
    };
    assert!(l.crossing_local(A, 0.0, 0.0, Side::Right).is_none());
    assert!(l.crossing_local(B, 0.0, 0.0, Side::Right).is_none());
    let mv = l.move_cursor(A, 0.0, 0.0, 10.0, 10.0);
    assert!(matches!(mv, Move::Inside { .. }));
    let mv2 = l.move_cursor(B, -10.0, -10.0, 5.0, 5.0);
    assert!(matches!(mv2, Move::Inside { .. }));
}
