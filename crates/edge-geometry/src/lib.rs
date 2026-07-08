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

fn normalized_axis(pos: f64, extent: u32) -> f32 {
    if extent <= 1 {
        return 0.0;
    }

    let max = f64::from(extent - 1);
    (clamp(pos, 0.0, max) / max) as f32
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
}
