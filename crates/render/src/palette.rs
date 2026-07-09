//! Deterministic per-layer colors, roughly matching a KLayout-style palette so
//! the side-by-side comparison reads cleanly. Semi-transparent for overlap.

use fluideda_core::Layer;

const PALETTE: [[f32; 4]; 8] = [
    [0.30, 0.80, 0.90, 0.55], // cyan
    [0.85, 0.30, 0.75, 0.55], // magenta
    [0.35, 0.85, 0.40, 0.55], // green
    [0.95, 0.80, 0.25, 0.55], // yellow
    [0.55, 0.45, 0.95, 0.55], // violet
    [0.95, 0.50, 0.30, 0.55], // orange
    [0.40, 0.60, 0.95, 0.55], // blue
    [0.90, 0.35, 0.40, 0.55], // red
];

pub fn color_for(layer: Layer) -> [f32; 4] {
    let idx = (layer.layer as usize + layer.datatype as usize) % PALETTE.len();
    PALETTE[idx]
}
