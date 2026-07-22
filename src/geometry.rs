//! Small geometry helpers shared by the humanizer, the CDP backend (CSS-pixel
//! viewport coordinates) and the native backend (screen coordinates).

use rand::{Rng, RngExt};
use serde::{Deserialize, Serialize};

/// A 2D point in floating-point coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    pub fn new(x: f64, y: f64) -> Self {
        Point { x, y }
    }

    pub fn distance(&self, other: &Point) -> f64 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        (dx * dx + dy * dy).sqrt()
    }

    /// Linear interpolation between `a` and `b` at fraction `t`.
    pub fn lerp(a: Point, b: Point, t: f64) -> Point {
        Point {
            x: a.x + (b.x - a.x) * t,
            y: a.y + (b.y - a.y) * t,
        }
    }
}

/// An axis-aligned rectangle (CSS pixels in the viewport for CDP boxes).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    pub fn center(&self) -> Point {
        Point::new(self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    /// A point inside the rect biased toward the centre, the way a human tends
    /// to land clicks rather than hitting the exact middle or the edges. Uses
    /// the average of two uniform samples (a triangular distribution) so the
    /// result clusters around the centre but stays well inside the box.
    pub fn humanlike_point<R: Rng>(&self, rng: &mut R) -> Point {
        // Inset a few percent so we never land on the very edge / border.
        let inset_x = (self.width * 0.15).min(6.0);
        let inset_y = (self.height * 0.15).min(6.0);
        let left = self.x + inset_x;
        let right = (self.x + self.width - inset_x).max(left);
        let top = self.y + inset_y;
        let bottom = (self.y + self.height - inset_y).max(top);

        let tri = |rng: &mut R, lo: f64, hi: f64| -> f64 {
            if hi <= lo {
                return lo;
            }
            let a: f64 = rng.random_range(lo..hi);
            let b: f64 = rng.random_range(lo..hi);
            (a + b) / 2.0
        };
        Point::new(tri(rng, left, right), tri(rng, top, bottom))
    }
}
