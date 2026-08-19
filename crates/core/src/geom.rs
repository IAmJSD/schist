//! Integer geometry in document pixel space.

use serde::{Deserialize, Serialize};

/// Axis-aligned rectangle, half-open: contains x in [left, right) and
/// y in [top, bottom). Coordinates may be negative (layers can extend
/// beyond the canvas).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IntRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl IntRect {
    pub const EMPTY: IntRect = IntRect { left: 0, top: 0, right: 0, bottom: 0 };

    pub fn new(left: i32, top: i32, right: i32, bottom: i32) -> Self {
        IntRect { left, top, right, bottom }
    }

    pub fn from_size(width: u32, height: u32) -> Self {
        IntRect { left: 0, top: 0, right: width as i32, bottom: height as i32 }
    }

    pub fn from_xywh(x: i32, y: i32, w: u32, h: u32) -> Self {
        IntRect { left: x, top: y, right: x + w as i32, bottom: y + h as i32 }
    }

    pub fn width(&self) -> i32 {
        (self.right - self.left).max(0)
    }

    pub fn height(&self) -> i32 {
        (self.bottom - self.top).max(0)
    }

    pub fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    pub fn intersect(&self, other: &IntRect) -> IntRect {
        let r = IntRect {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        };
        if r.is_empty() {
            IntRect::EMPTY
        } else {
            r
        }
    }

    pub fn union(&self, other: &IntRect) -> IntRect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        IntRect {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }

    pub fn translated(&self, dx: i32, dy: i32) -> IntRect {
        IntRect {
            left: self.left + dx,
            top: self.top + dy,
            right: self.right + dx,
            bottom: self.bottom + dy,
        }
    }

    pub fn inflated(&self, d: i32) -> IntRect {
        IntRect {
            left: self.left - d,
            top: self.top - d,
            right: self.right + d,
            bottom: self.bottom + d,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersect_disjoint_is_empty() {
        let a = IntRect::from_xywh(0, 0, 10, 10);
        let b = IntRect::from_xywh(20, 20, 5, 5);
        assert!(a.intersect(&b).is_empty());
    }

    #[test]
    fn union_of_empty_is_other() {
        let a = IntRect::EMPTY;
        let b = IntRect::from_xywh(-5, -5, 10, 10);
        assert_eq!(a.union(&b), b);
    }
}
