//! Overlay layer: remote cursors + shared markers, drawn on top of geometry.
//!
//! Phase 0 keeps this simple — markers are axis-aligned rectangles and cursors
//! are small triangles. Both come from the shared CRDT doc / awareness state and
//! are positioned in **world (nm) coordinates**, so they line up across users at
//! any zoom. Selection highlighting slots in here in Phase 3.

use glam::Vec2;

/// A collaborator's cursor, in world coordinates.
#[derive(Debug, Clone)]
pub struct RemoteCursor {
    pub pos: Vec2,
    pub color: [f32; 4],
    pub name: String,
}

/// A shared rectangle marker (world coordinates, nm).
#[derive(Debug, Clone, Copy)]
pub struct Marker {
    pub min: Vec2,
    pub max: Vec2,
    pub color: [f32; 4],
}

/// Placeholder for the overlay pipeline. In Phase 0 the app can draw cursors and
/// marker outlines using egui's painter in screen space (converting world->screen
/// via `Camera`), which avoids a second wgpu pipeline for the first demo. This
/// type reserves the seam for a dedicated wgpu overlay pass later.
pub struct OverlayRenderer {
    pub cursors: Vec<RemoteCursor>,
    pub markers: Vec<Marker>,
}

impl Default for OverlayRenderer {
    fn default() -> Self {
        Self {
            cursors: Vec::new(),
            markers: Vec::new(),
        }
    }
}

impl OverlayRenderer {
    pub fn new() -> Self {
        Self::default()
    }
}
