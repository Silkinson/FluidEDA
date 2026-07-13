//! Core layout data types.
//!
//! Coordinates are stored as `i32` in **database units (nanometres)** — never
//! floats. IC layouts span millions of nm; keeping integers avoids the rounding
//! that would corrupt a design. Conversion to `f32` happens only at the very
//! last moment, in the renderer's camera (with origin rebasing, Phase 1).

use bytemuck::{Pod, Zeroable};

/// A layer/datatype pair — the GDSII addressing of a drawing layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Layer {
    pub layer: u16,
    pub datatype: u16,
}

impl Layer {
    pub fn new(layer: u16, datatype: u16) -> Self {
        Self { layer, datatype }
    }
}

/// A point in database units (nm).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// A simple polygon: a closed ring of points, no holes (Phase 0).
/// Holes / paths / references come in Phase 1.
#[derive(Debug, Clone)]
pub struct Polygon {
    pub layer: Layer,
    pub points: Vec<Point>,
}

/// Axis-aligned bounding box in nm. Used for framing the initial view and,
/// later, for culling.
#[derive(Debug, Clone, Copy)]
pub struct Bbox {
    pub min: Point,
    pub max: Point,
}

impl Bbox {
    pub fn empty() -> Self {
        Self {
            min: Point::new(i32::MAX, i32::MAX),
            max: Point::new(i32::MIN, i32::MIN),
        }
    }

    pub fn expand(&mut self, p: Point) {
        self.min.x = self.min.x.min(p.x);
        self.min.y = self.min.y.min(p.y);
        self.max.x = self.max.x.max(p.x);
        self.max.y = self.max.y.max(p.y);
    }

    pub fn center(&self) -> (f64, f64) {
        (
            (self.min.x as f64 + self.max.x as f64) * 0.5,
            (self.min.y as f64 + self.max.y as f64) * 0.5,
        )
    }

    pub fn size(&self) -> (f64, f64) {
        (
            (self.max.x as f64 - self.min.x as f64).max(1.0),
            (self.max.y as f64 - self.min.y as f64).max(1.0),
        )
    }
}

/// GPU-ready vertex: position in nm (as f32 for upload) — origin rebasing is
/// applied in the camera, so f32 here is fine for the Phase 0 sample sizes.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, Pod, Zeroable)]
pub struct Vertex {
    pub pos: [f32; 2],
}

/// The flat Phase 0 layout: a bag of polygons plus derived bbox.
/// Phase 1 replaces this with a hierarchical cell database.
#[derive(Debug, Default)]
pub struct FlatLayout {
    pub polygons: Vec<Polygon>,
}

impl FlatLayout {
    pub fn bbox(&self) -> Bbox {
        let mut b = Bbox::empty();
        for poly in &self.polygons {
            for &p in &poly.points {
                b.expand(p);
            }
        }
        b
    }

    /// Distinct layers present, sorted — drives the layer panel + draw order.
    pub fn layers(&self) -> Vec<Layer> {
        let mut ls: Vec<Layer> = self.polygons.iter().map(|p| p.layer).collect();
        ls.sort();
        ls.dedup();
        ls
    }
}
