// Instanced fill shader for hierarchical layout geometry.
//
// One cell's mesh (cell-local nm coords) is drawn N times, once per placement.
// The per-instance affine transform (child-local -> rebased world) arrives as
// two vec4 instance attributes; the translation is already origin-rebased on the
// CPU (subtracted in f64, cast to f32) so million-nm chips stay jitter-free.
// view_proj is shared; color is per-layer.

struct Globals {
    view_proj: mat4x4<f32>,
};

struct LayerUniform {
    color: vec4<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(1) @binding(0) var<uniform> layer: LayerUniform;

struct VsIn {
    @location(0) pos: vec2<f32>,   // cell-local position (nm)
    @location(1) i0: vec4<f32>,    // affine row 0: (a, c, tx, _)
    @location(2) i1: vec4<f32>,    // affine row 1: (b, d, ty, _)
};

@vertex
fn vs_main(v: VsIn) -> @builtin(position) vec4<f32> {
    // world_rebased = M * local + t   (t already rebased near the viewport)
    let wx = v.i0.x * v.pos.x + v.i0.y * v.pos.y + v.i0.z;
    let wy = v.i1.x * v.pos.x + v.i1.y * v.pos.y + v.i1.z;
    return globals.view_proj * vec4<f32>(wx, wy, 0.0, 1.0);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return layer.color;
}
