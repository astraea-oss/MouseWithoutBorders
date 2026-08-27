use edge_protocol::Edge;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn right(self) -> f64 {
        self.x + f64::from(self.width.saturating_sub(1))
    }

    pub fn bottom(self) -> f64 {
        self.y + f64::from(self.height.saturating_sub(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EnterRemote {
    pub edge: Edge,
    pub normalized_y: f32,
    pub remote_start: Point,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LeaveRemote {
    pub edge: Edge,
    pub normalized_y: f32,
    pub local_restore: Point,
}

pub fn clamp(value: f64, min: f64, max: f64) -> f64 {
    value.max(min).min(max)
}

pub fn enter_left_edge(local_cursor_y: f64, local: Size, remote: Size) -> EnterRemote {
    let normalized_y = normalized_axis(local_cursor_y, local.height);
    let remote_y = f64::from(remote.height.saturating_sub(1)) * f64::from(normalized_y);

    EnterRemote {
        edge: Edge::Left,
        normalized_y,
        remote_start: Point {
            x: f64::from(remote.width.saturating_sub(2)),
            y: clamp(remote_y, 0.0, f64::from(remote.height.saturating_sub(1))),
        },
    }
}

pub fn leave_right_edge(remote_cursor_y: f64, local: Size, remote: Size) -> LeaveRemote {
    let normalized_y = normalized_axis(remote_cursor_y, remote.height);
    let local_y = f64::from(local.height.saturating_sub(1)) * f64::from(normalized_y);

    LeaveRemote {
        edge: Edge::Right,
        normalized_y,
        local_restore: Point {
            x: 1.0,
            y: clamp(local_y, 0.0, f64::from(local.height.saturating_sub(1))),
        },
    }
}

pub fn apply_remote_motion(cursor: Point, dx: f64, dy: f64, remote: Size) -> Point {
    Point {
        x: clamp(
            cursor.x + dx,
            0.0,
            f64::from(remote.width.saturating_sub(1)),
        ),
        y: clamp(
            cursor.y + dy,
            0.0,
            f64::from(remote.height.saturating_sub(1)),
        ),
    }
}

pub fn exits_right(cursor: Point, remote: Size) -> bool {
    cursor.x >= f64::from(remote.width.saturating_sub(1))
}

pub fn point_is_at_edge(edge: Edge, point: Point, bounds: Rect) -> bool {
    match edge {
        Edge::Left => point.x <= bounds.x,
        Edge::Right => point.x >= bounds.right(),
        Edge::Top => point.y <= bounds.y,
        Edge::Bottom => point.y >= bounds.bottom(),
    }
}

pub fn edge_anchor(edge: Edge, point: Point, bounds: Rect, inset: f64) -> Point {
    match edge {
        Edge::Left => Point {
            x: bounds.x + inset,
            y: clamp(point.y, bounds.y, bounds.bottom()),
        },
        Edge::Right => Point {
            x: bounds.right() - inset,
            y: clamp(point.y, bounds.y, bounds.bottom()),
        },
        Edge::Top => Point {
            x: clamp(point.x, bounds.x, bounds.right()),
            y: bounds.y + inset,
        },
        Edge::Bottom => Point {
            x: clamp(point.x, bounds.x, bounds.right()),
            y: bounds.bottom() - inset,
        },
    }
}

pub fn normalized_perpendicular(edge: Edge, point: Point, bounds: Rect) -> f32 {
    match edge {
        Edge::Left | Edge::Right => normalized_axis(point.y - bounds.y, bounds.height),
        Edge::Top | Edge::Bottom => normalized_axis(point.x - bounds.x, bounds.width),
    }
}

pub fn remote_entry_point(edge: Edge, normalized: f32, remote: Size, inset: f64) -> Point {
    let x_inset = bounded_inset(inset, remote.width);
    let y_inset = bounded_inset(inset, remote.height);
    let x = f64::from(remote.width.saturating_sub(1)) * f64::from(normalized);
    let y = f64::from(remote.height.saturating_sub(1)) * f64::from(normalized);
    match edge {
        Edge::Left => Point {
            x: f64::from(remote.width.saturating_sub(1)) - x_inset,
            y,
        },
        Edge::Right => Point { x: x_inset, y },
        Edge::Top => Point {
            x,
            y: f64::from(remote.height.saturating_sub(1)) - y_inset,
        },
        Edge::Bottom => Point { x, y: y_inset },
    }
}

pub fn local_restore_point(edge: Edge, normalized: f32, bounds: Rect, inset: f64) -> Point {
    let x = clamp(
        bounds.x + f64::from(bounds.width.saturating_sub(1)) * f64::from(normalized),
        bounds.x,
        bounds.right(),
    )
    .round();
    let y = clamp(
        bounds.y + f64::from(bounds.height.saturating_sub(1)) * f64::from(normalized),
        bounds.y,
        bounds.bottom(),
    )
    .round();
    match edge {
        Edge::Left => Point {
            x: bounds.x + inset,
            y,
        },
        Edge::Right => Point {
            x: bounds.right() - inset,
            y,
        },
        Edge::Top => Point {
            x,
            y: bounds.y + inset,
        },
        Edge::Bottom => Point {
            x,
            y: bounds.bottom() - inset,
        },
    }
}

pub fn remote_return_edge_reached(
    controller_edge: Edge,
    cursor: Point,
    remote: Size,
    margin: f64,
) -> bool {
    match controller_edge {
        Edge::Left => cursor.x >= f64::from(remote.width.saturating_sub(1)) - margin,
        Edge::Right => cursor.x <= margin,
        Edge::Top => cursor.y >= f64::from(remote.height.saturating_sub(1)) - margin,
        Edge::Bottom => cursor.y <= margin,
    }
}

pub fn normalized_axis(pos: f64, extent: u32) -> f32 {
    if extent <= 1 {
        return 0.0;
    }

    let max = f64::from(extent - 1);
    (clamp(pos, 0.0, max) / max) as f32
}

fn bounded_inset(inset: f64, extent: u32) -> f64 {
    let max = f64::from(extent.saturating_sub(1));
    clamp(inset, 1.0, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_windows_left_edge_to_remote_right_edge() {
        let local = Size {
            width: 2560,
            height: 1440,
        };
        let remote = Size {
            width: 1920,
            height: 1080,
        };

        let entry = enter_left_edge(720.0, local, remote);

        assert_eq!(entry.edge, Edge::Left);
        assert_eq!(entry.remote_start.x, 1918.0);
        assert!((entry.remote_start.y - 539.875).abs() < 0.01);
    }

    #[test]
    fn clamps_remote_motion_to_bounds() {
        let remote = Size {
            width: 1920,
            height: 1080,
        };

        let cursor = apply_remote_motion(Point { x: 10.0, y: 10.0 }, -50.0, 5000.0, remote);

        assert_eq!(cursor.x, 0.0);
        assert_eq!(cursor.y, 1079.0);
    }

    #[test]
    fn four_edge_entry_and_return_math_is_directionally_symmetric() {
        let local = Rect {
            x: -2560.0,
            y: 0.0,
            width: 2560,
            height: 1440,
        };
        let remote = Size {
            width: 1920,
            height: 1080,
        };

        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            let source = match edge {
                Edge::Left | Edge::Right => Point {
                    x: local.x,
                    y: 360.0,
                },
                Edge::Top | Edge::Bottom => Point {
                    x: -1920.0,
                    y: local.y,
                },
            };
            let normalized = normalized_perpendicular(edge, source, local);
            let entry = remote_entry_point(edge, normalized, remote, 32.0);
            let restore = local_restore_point(edge, normalized, local, 3.0);

            assert!((normalized - 0.25).abs() < 0.001);
            match edge {
                Edge::Left => {
                    assert_eq!(entry.x, 1887.0);
                    assert_eq!(restore.x, -2557.0);
                    assert!(remote_return_edge_reached(
                        edge,
                        Point {
                            x: 1910.0,
                            y: entry.y
                        },
                        remote,
                        12.0
                    ));
                }
                Edge::Right => {
                    assert_eq!(entry.x, 32.0);
                    assert_eq!(restore.x, -4.0);
                    assert!(remote_return_edge_reached(
                        edge,
                        Point { x: 8.0, y: entry.y },
                        remote,
                        12.0
                    ));
                }
                Edge::Top => {
                    assert_eq!(entry.y, 1047.0);
                    assert_eq!(restore.y, 3.0);
                    assert!(remote_return_edge_reached(
                        edge,
                        Point {
                            x: entry.x,
                            y: 1070.0
                        },
                        remote,
                        12.0
                    ));
                }
                Edge::Bottom => {
                    assert_eq!(entry.y, 32.0);
                    assert_eq!(restore.y, 1436.0);
                    assert!(remote_return_edge_reached(
                        edge,
                        Point { x: entry.x, y: 8.0 },
                        remote,
                        12.0
                    ));
                }
            }
        }
    }
}
