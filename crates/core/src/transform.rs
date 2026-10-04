//! Affine 2D transforms for the cell hierarchy.
//!
//! A `Transform` maps **child-local** coordinates (nm) into a **parent** frame.
//! Composing parent→child down the instance tree accumulates the full
//! placement. The linear part is kept in `f64`: rotation and magnification
//! inherently produce non-integer intermediate values, but the *stored geometry*
//! stays `i32` nm (invariant) — transforms are only ever applied to derive
//! world-space bounding boxes for culling and the per-instance matrix uploaded
//! to the GPU. Integer coordinates are cast to `f32` only at that upload seam.

use crate::types::{Bbox, Point};

/// A 2D affine transform, row-major:
/// ```text
/// x' = a*x + c*y + tx
/// y' = b*x + d*y + ty
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub tx: f64,
    pub ty: f64,
}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Transform {
    pub const IDENTITY: Transform = Transform {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    /// Pure translation.
    pub fn translate(tx: f64, ty: f64) -> Self {
        Self {
            tx,
            ty,
            ..Self::IDENTITY
        }
    }

    /// Build the linear+translation transform described by a GDSII `STRANS`
    /// (reflection about x, then rotation, then magnification) plus a placement
    /// origin. Per the spec (and the `gds21` doc): reflection about the x-axis is
    /// applied **before** rotation; magnification is a uniform scale.
    ///
    /// Resulting map: `world = Translate(origin) · Rotate(angle) · Reflect · Scale(mag)`.
    pub fn from_strans(
        origin_x: f64,
        origin_y: f64,
        angle_deg: f64,
        reflected: bool,
        mag: f64,
    ) -> Self {
        let theta = angle_deg.to_radians();
        let (s, co) = theta.sin_cos();
        // Linear part = mag * Rotate(theta) * Reflect_x?
        //   Reflect_x negates y:      [[1,0],[0,-1]]
        //   Rotate CCW by theta:      [[co,-s],[s,co]]
        //   Rotate * Reflect:         [[co, s],[s,-co]]
        let refl = if reflected { -1.0 } else { 1.0 };
        // Without reflection: [[co,-s],[s,co]]; with: [[co,s],[s,-co]].
        Self {
            a: mag * co,
            b: mag * s,
            c: mag * (-s) * refl,
            d: mag * co * refl,
            tx: origin_x,
            ty: origin_y,
        }
    }

    /// `(angle°, reflected, mag)` of a [`from_strans`](Self::from_strans)
    /// transform, in the one form both writers emit (Phase 7.4). The angle
    /// is in [0°, 360°). It is exactly a multiple of 90° when it is one to
    /// within 1e-9°, and other angles are rounded to 1e-9°. Magnification is
    /// rounded to 12 decimals.
    ///
    /// The reader's sin/cos and this atan2 leave noise in the last bits, and
    /// the noise differs between platforms (wasm's math library vs the
    /// native one). Measured: caravel exported in the browser and on the
    /// server differed in 582 ANGLE records, all −90° ± 1 ulp. Canonical
    /// values make an export the same bytes wherever it is written.
    pub fn strans_parts(&self) -> (f64, bool, f64) {
        let mag = (self.a * self.a + self.b * self.b).sqrt();
        let reflected = self.a * self.d - self.b * self.c < 0.0;
        let mut angle = self.b.atan2(self.a).to_degrees().rem_euclid(360.0);
        let quarter = (angle / 90.0).round();
        angle = if (angle - quarter * 90.0).abs() < 1e-9 {
            (quarter as i64).rem_euclid(4) as f64 * 90.0
        } else {
            (angle * 1e9).round() / 1e9
        };
        let mag = (mag * 1e12).round() / 1e12;
        (angle, reflected, mag)
    }

    /// `self ∘ rhs`: apply `rhs` first (inner/child), then `self` (outer/parent).
    /// Used to push a child's placement onto the parent's accumulated transform.
    pub fn then(&self, rhs: &Transform) -> Transform {
        let p = self;
        let c = rhs;
        Transform {
            a: p.a * c.a + p.c * c.b,
            b: p.b * c.a + p.d * c.b,
            c: p.a * c.c + p.c * c.d,
            d: p.b * c.c + p.d * c.d,
            tx: p.a * c.tx + p.c * c.ty + p.tx,
            ty: p.b * c.tx + p.d * c.ty + p.ty,
        }
    }

    /// Map a point (nm) into the transform's target frame.
    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.tx,
            self.b * x + self.d * y + self.ty,
        )
    }

    pub fn apply_point(&self, p: Point) -> (f64, f64) {
        self.apply(p.x as f64, p.y as f64)
    }

    /// Determinant of the linear part. `|det|.sqrt()` is the uniform-ish scale
    /// factor a cell's geometry undergoes — used for the pixel-size LOD test.
    pub fn det(&self) -> f64 {
        self.a * self.d - self.b * self.c
    }

    /// Approximate uniform scale (geometric mean of the axis scales).
    pub fn scale_factor(&self) -> f64 {
        self.det().abs().sqrt()
    }

    /// Inverse transform. Panics only on a degenerate (zero-det) linear part,
    /// which a valid mag>0 STRANS never produces.
    pub fn inverse(&self) -> Transform {
        let det = self.det();
        let inv = 1.0 / det;
        let ia = self.d * inv;
        let ib = -self.b * inv;
        let ic = -self.c * inv;
        let id = self.a * inv;
        Transform {
            a: ia,
            b: ib,
            c: ic,
            d: id,
            tx: -(ia * self.tx + ic * self.ty),
            ty: -(ib * self.tx + id * self.ty),
        }
    }

    /// Transform an integer bbox into this frame, returning the **axis-aligned**
    /// bbox of the four mapped corners (correct under rotation/reflection).
    pub fn apply_bbox(&self, b: &Bbox) -> Bboxf {
        let corners = [
            self.apply(b.min.x as f64, b.min.y as f64),
            self.apply(b.max.x as f64, b.min.y as f64),
            self.apply(b.max.x as f64, b.max.y as f64),
            self.apply(b.min.x as f64, b.max.y as f64),
        ];
        let mut out = Bboxf::empty();
        for (x, y) in corners {
            out.expand(x, y);
        }
        out
    }

    /// Transform a float bbox into this frame (axis-aligned bound of the mapped
    /// corners). Used to push the world viewport into a cell's local frame.
    pub fn apply_bboxf(&self, b: &Bboxf) -> Bboxf {
        let corners = [
            self.apply(b.min_x, b.min_y),
            self.apply(b.max_x, b.min_y),
            self.apply(b.max_x, b.max_y),
            self.apply(b.min_x, b.max_y),
        ];
        let mut out = Bboxf::empty();
        for (x, y) in corners {
            out.expand(x, y);
        }
        out
    }
}

/// Floating-point axis-aligned bbox in world nm — the currency of culling,
/// where accumulated transforms make integer bounds meaningless.
#[derive(Debug, Clone, Copy)]
pub struct Bboxf {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Bboxf {
    pub fn empty() -> Self {
        Self {
            min_x: f64::INFINITY,
            min_y: f64::INFINITY,
            max_x: f64::NEG_INFINITY,
            max_y: f64::NEG_INFINITY,
        }
    }

    pub fn new(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Self {
        Self {
            min_x,
            min_y,
            max_x,
            max_y,
        }
    }

    pub fn expand(&mut self, x: f64, y: f64) {
        self.min_x = self.min_x.min(x);
        self.min_y = self.min_y.min(y);
        self.max_x = self.max_x.max(x);
        self.max_y = self.max_y.max(y);
    }

    pub fn is_empty(&self) -> bool {
        self.min_x > self.max_x || self.min_y > self.max_y
    }

    pub fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    pub fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    /// Do two boxes overlap (touching edges count as overlap)?
    pub fn intersects(&self, o: &Bboxf) -> bool {
        self.min_x <= o.max_x
            && self.max_x >= o.min_x
            && self.min_y <= o.max_y
            && self.max_y >= o.min_y
    }
}

#[cfg(test)]
mod tests {

    /// Phase 7.4: ulp noise of either sign around a 90° multiple (what the
    /// reader's sin/cos leaves, differently per platform) gives one exact
    /// angle; other angles and magnifications are rounded, not noisy.
    #[test]
    fn strans_parts_are_canonical() {
        let t = |a: f64, b: f64, c: f64, d: f64| Transform { a, b, c, d, tx: 0.0, ty: 0.0 };
        let e = 6.123233995736766e-17;
        for (m, want) in [
            (t(e, -1.0, 1.0, e), (270.0, false, 1.0)),
            (t(-e, -1.0, 1.0, -e), (270.0, false, 1.0)),
            (t(-1.0, 1.2e-16, -1.2e-16, -1.0), (180.0, false, 1.0)),
            (t(-1.0, -1.2e-16, 1.2e-16, -1.0), (180.0, false, 1.0)),
            (t(1.0, -1e-17, 1e-17, 1.0), (0.0, false, 1.0)),
            (t(e, 1.0, 1.0, -e), (90.0, true, 1.0)),
        ] {
            assert_eq!(m.strans_parts(), want);
        }
        for (deg, refl, mag) in [(45.0, false, 1.0), (30.0, true, 2.5), (271.5, false, 0.25)] {
            let (a, r, g) = Transform::from_strans(0.0, 0.0, deg, refl, mag).strans_parts();
            assert_eq!((a, r, g), (deg, refl, mag), "{deg}° {refl} ×{mag}");
        }
    }
    use super::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-6, "{a} != {b}");
    }

    #[test]
    fn identity_roundtrip() {
        let t = Transform::IDENTITY;
        let (x, y) = t.apply(3.0, 5.0);
        approx(x, 3.0);
        approx(y, 5.0);
    }

    #[test]
    fn rotate_90_ccw() {
        // (1,0) rotated 90° CCW -> (0,1)
        let t = Transform::from_strans(0.0, 0.0, 90.0, false, 1.0);
        let (x, y) = t.apply(1.0, 0.0);
        approx(x, 0.0);
        approx(y, 1.0);
    }

    #[test]
    fn reflect_then_rotate() {
        // Reflect about x: (0,1) -> (0,-1); no rotation.
        let t = Transform::from_strans(0.0, 0.0, 0.0, true, 1.0);
        let (x, y) = t.apply(0.0, 1.0);
        approx(x, 0.0);
        approx(y, -1.0);
    }

    #[test]
    fn compose_and_invert() {
        let parent = Transform::from_strans(100.0, 200.0, 90.0, false, 2.0);
        let child = Transform::from_strans(10.0, 0.0, 0.0, false, 1.0);
        let world = parent.then(&child);
        let inv = world.inverse();
        let (wx, wy) = world.apply(7.0, 9.0);
        let (bx, by) = inv.apply(wx, wy);
        approx(bx, 7.0);
        approx(by, 9.0);
    }

    #[test]
    fn magnification_scales() {
        let t = Transform::from_strans(0.0, 0.0, 0.0, false, 3.0);
        approx(t.scale_factor(), 3.0);
    }
}
