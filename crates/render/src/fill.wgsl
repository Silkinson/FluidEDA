// Fill shader for layer geometry.
// One draw call per layer: view_proj is shared, color is per-layer.

struct Globals {
    view_proj: mat4x4<f32>,
};

struct LayerUniform {
    color: vec4<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(1) @binding(0) var<uniform> layer: LayerUniform;

@vertex
fn vs_main(@location(0) pos: vec2<f32>) -> @builtin(position) vec4<f32> {
    return globals.view_proj * vec4<f32>(pos, 0.0, 1.0);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return layer.color;
}
