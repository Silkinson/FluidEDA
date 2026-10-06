//! fluideda-render — wgpu pipelines for drawing **hierarchical** layout geometry.
//!
//! Each **chunk** of a cell definition is tessellated once (cell-local nm) and
//! uploaded as a static per-layer mesh. Every frame the culler
//! ([`fluideda_core::cull`]) hands us, per visible (cell, chunk), the world
//! transforms at which to draw it; we pack those into one instance buffer and
//! issue **one instanced draw per cell-chunk/layer**
//! — an AREF of a million leaves is one leaf buffer drawn N times, never N copies
//! of the geometry. Cells too small to resolve arrive as LOD proxy boxes, drawn
//! as a unit quad instanced to each box.
//!
//! Origin rebasing (the f32-precision fix) happens here: instance translations
//! are computed as `world - world_origin` in f64, then cast to f32, so million-nm
//! coordinates never lose precision at deep zoom. Kept free of windowing/app
//! logic so an eframe callback or a Phase 2 worker OffscreenCanvas can drive it.

mod camera;
pub mod overlay;
mod palette;

pub use camera::Camera;
pub use overlay::{Marker, OverlayRenderer, RemoteCursor};

/// The deterministic display color for a layer (matches the renderer's fill).
/// Exposed so the app's layer panel can show a matching swatch.
pub fn layer_color(layer: fluideda_core::Layer) -> [f32; 4] {
    palette::color_for(layer)
}

/// The colour a chunk's layers make from far away (Phase 7.3 SLICE C), packed
/// straight RGBA8 — what a streamed chunk too dense to fetch is drawn as. Each
/// visible layer goes over the previous in draw order (ascending layer) with
/// its palette alpha scaled by its area coverage; a stack of src-over draws is
/// itself one src-over, so the proxy drawn with this colour shows what the
/// full geometry averages to. Channels are quantized to 5 bits so a view's
/// proxies fall into few colour groups (draw calls). 0 = nothing visible.
pub fn composite_rgba(look: &[(Layer, u8)], hidden: impl Fn(Layer) -> bool) -> u32 {
    let mut layers: Vec<(Layer, u8)> = look.iter().copied().filter(|&(l, _)| !hidden(l)).collect();
    layers.sort_by_key(|&(l, _)| l);
    // Premultiplied accumulation over transparent black.
    let (mut r, mut g, mut b, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for (l, c) in layers {
        let [lr, lg, lb, la] = palette::color_for(l);
        let e = (la * c as f32 / 255.0).clamp(0.0, 1.0);
        r = lr * e + r * (1.0 - e);
        g = lg * e + g * (1.0 - e);
        b = lb * e + b * (1.0 - e);
        a = e + a * (1.0 - e);
    }
    if a <= 1.0 / 255.0 {
        return 0;
    }
    let q = |v: f32| ((v.clamp(0.0, 1.0) * 255.0).round() as u32) & 0xF8;
    let alpha = q(a).max(8);
    (q(r / a) << 24) | (q(g / a) << 16) | (q(b / a) << 8) | alpha
}

fn unpack_rgba(v: u32) -> [f32; 4] {
    [(v >> 24) as f32 / 255.0, ((v >> 16) & 0xff) as f32 / 255.0, ((v >> 8) & 0xff) as f32 / 255.0, (v & 0xff) as f32 / 255.0]
}

use bytemuck::{Pod, Zeroable};
use fluideda_core::{Bboxf, CellId, ChunkId, LayerMesh, Layer, VisibleSet};
use std::collections::{HashMap, HashSet};
use wgpu::util::DeviceExt;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    view_proj: [[f32; 4]; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct LayerUniform {
    color: [f32; 4],
}

/// Per-instance affine transform (child-local → rebased world), two rows.
#[repr(C)]
#[derive(Clone, Copy, Default, Pod, Zeroable)]
struct InstanceRaw {
    row0: [f32; 4], // a, c, tx, _
    row1: [f32; 4], // b, d, ty, _
}

/// One per-layer GPU mesh belonging to a cell def (or the proxy quad).
struct GpuMesh {
    layer: Layer,
    vbuf: wgpu::Buffer,
    ibuf: wgpu::Buffer,
    index_count: u32,
    color_bind: wgpu::BindGroup,
}

struct CellGpu {
    meshes: Vec<GpuMesh>,
}

/// Key for one uploaded geometry buffer set: a chunk of a cell definition.
/// Chunking (Phase 2b) made this a pair — a die-spanning cell now uploads only
/// the chunks a viewport touched, not all of its geometry.
type ChunkKey = (CellId, ChunkId);

/// A recorded instanced draw for this frame: a cell chunk and its slice of the
/// shared instance buffer.
struct CellPlan {
    key: ChunkKey,
    first: u32,
    count: u32,
}

/// One mesh to issue this frame, carrying its `(cell, layer, chunk)` sort key.
///
/// Draw order is not cosmetic — the pipeline alpha-blends, so which mesh goes
/// first decides the pixel wherever two layers overlap. Before chunking, a cell
/// was one mesh set and drew **layer-major**: all of layer 0, then all of layer
/// 1. Splitting a cell into chunks would naturally draw chunk-major (chunk 0's
/// every layer, then chunk 1's), which re-composites every cross-layer overlap
/// and visibly changes the image — 4.7% of caravel's pixels at fit when this was
/// measured.
///
/// Sorting by `(cell, layer, chunk)` restores the original layer sweep exactly.
/// Reordering chunks *within* one layer is safe: a layer has a single colour and
/// alpha, and `src-over` of identical colours is order-independent.
struct DrawItem {
    cell: CellId,
    layer: Layer,
    chunk: ChunkId,
    plan: u32,
    mesh: u32,
}

/// Draws a hierarchical layout by instancing per-cell meshes.
pub struct HierRenderer {
    pipeline: wgpu::RenderPipeline,
    globals_buf: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,
    layer_bgl: wgpu::BindGroupLayout, // kept for lazy per-cell mesh uploads

    cells: HashMap<ChunkKey, CellGpu>,
    /// Running total of uploaded vertex+index bytes (lazy-tessellation growth).
    uploaded_bytes: usize,
    proxy: GpuMesh, // unit-square [0,1]^2 mesh, dim color

    /// Phase 3 edit overlay: moved/added shapes in **world nm**, drawn on top of
    /// the base with a single identity (origin-rebased) instance. Rebuilt whenever
    /// the overlay changes; tiny, so a full re-upload per edit is fine.
    overlay: Vec<GpuMesh>,

    /// Shared instance buffer, grown as needed and rewritten each frame.
    instbuf: wgpu::Buffer,
    instcap: usize,

    hidden: HashSet<Layer>,

    // Per-frame plan (set in `prepare`, consumed in `paint`).
    cell_plan: Vec<CellPlan>,
    /// Meshes to draw, in `(cell, layer, chunk)` order. See [`DrawItem`].
    draw_seq: Vec<DrawItem>,
    /// This frame's proxy draws: `(packed colour, first instance, count)` —
    /// colour 0 is the neutral proxy (Phase 7.3 SLICE C).
    proxy_groups: Vec<(u32, u32, u32)>,
    /// Bind groups for coloured proxies, made on first use.
    proxy_binds: HashMap<u32, wgpu::BindGroup>,
    /// Instance-buffer slot of the overlay's identity transform (set in `prepare`).
    overlay_first: u32,
}

const INST_SIZE: u64 = std::mem::size_of::<InstanceRaw>() as u64;

impl HierRenderer {
    /// Build the pipeline and proxy mesh only. Cell geometry is tessellated and
    /// uploaded lazily via [`upload_cell`] the first time a cell is drawn.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fill"),
            source: wgpu::ShaderSource::Wgsl(include_str!("fill.wgsl").into()),
        });

        let globals_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals-bgl"),
            entries: &[uniform_entry(0, wgpu::ShaderStages::VERTEX)],
        });
        let layer_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("layer-bgl"),
            entries: &[uniform_entry(0, wgpu::ShaderStages::FRAGMENT)],
        });

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals-bind"),
            layout: &globals_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buf.as_entire_binding(),
            }],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fill-layout"),
            bind_group_layouts: &[Some(&globals_bgl), Some(&layer_bgl)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fill-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[
                    // slot 0: per-vertex cell-local position
                    Some(wgpu::VertexBufferLayout {
                        array_stride: 8, // vec2<f32>
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x2],
                    }),
                    // slot 1: per-instance affine transform (two vec4)
                    Some(wgpu::VertexBufferLayout {
                        array_stride: INST_SIZE,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &wgpu::vertex_attr_array![1 => Float32x4, 2 => Float32x4],
                    }),
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // Cells start empty and fill in on demand (lazy tessellation).
        let cells = HashMap::new();

        // Unit-square proxy mesh (two triangles), dim neutral color.
        let proxy = build_proxy(device, &layer_bgl);

        // Start with a modest instance buffer; grows on demand.
        let instcap = 4096;
        let instbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("instances"),
            size: instcap as u64 * INST_SIZE,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            globals_buf,
            globals_bind,
            layer_bgl,
            cells,
            uploaded_bytes: 0,
            proxy,
            instbuf,
            instcap,
            overlay: Vec::new(),
            hidden: HashSet::new(),
            cell_plan: Vec::new(),
            draw_seq: Vec::new(),
            proxy_groups: Vec::new(),
            proxy_binds: HashMap::new(),
            overlay_first: 0,
        }
    }

    /// Drop all uploaded geometry (cells + overlay) but keep the compiled
    /// pipeline, bind groups, and buffers. Used when a new design is installed so
    /// the swap doesn't pay a shader/pipeline recompile — the ~50-100 ms cost of
    /// building a fresh `HierRenderer` in WebGL2. Everything reuploads lazily.
    pub fn reset(&mut self) {
        self.cells.clear();
        self.overlay.clear();
        self.uploaded_bytes = 0;
        self.hidden.clear();
        self.cell_plan.clear();
        self.draw_seq.clear();
    }

    /// Is this cell chunk's geometry already tessellated + uploaded?
    pub fn has_chunk(&self, key: ChunkKey) -> bool {
        self.cells.contains_key(&key)
    }

    /// Draw calls the last `prepare` planned (one per visible (chunk, layer)
    /// mesh, plus the proxy and overlay draws) — the per-frame command count
    /// that dominates WebGPU's CPU cost at fit view (Phase 5c measurement).
    pub fn draw_calls(&self) -> usize {
        self.draw_seq.len() + self.proxy_groups.len() + (!self.overlay.is_empty()) as usize
    }

    /// Total vertex+index bytes uploaded so far (grows with lazy tessellation).
    pub fn uploaded_bytes(&self) -> usize {
        self.uploaded_bytes
    }

    /// Tessellate-then-upload happens caller-side; this takes the finished
    /// per-layer meshes and builds this chunk's GPU buffers. Idempotent-ish: an
    /// empty mesh set marks the chunk as "seen" so we don't retessellate it.
    pub fn upload_chunk(&mut self, device: &wgpu::Device, key: ChunkKey, meshes: &[LayerMesh]) {
        if self.cells.contains_key(&key) {
            return;
        }
        let gpu_meshes: Vec<GpuMesh> = meshes
            .iter()
            .filter(|m| !m.indices.is_empty())
            .map(|m| {
                self.uploaded_bytes += m.vertices.len() * std::mem::size_of::<fluideda_core::Vertex>()
                    + m.indices.len() * 4;
                build_mesh(device, &self.layer_bgl, m.layer, &m.vertices, &m.indices)
            })
            .collect();
        self.cells.insert(key, CellGpu { meshes: gpu_meshes });
    }

    /// Forget a chunk's uploaded geometry so the next [`upload_chunk`] rebuilds
    /// it (Phase 3 incremental invalidation). Called when an edit dirties a
    /// chunk; the app then re-tessellates just that chunk with suppressions.
    pub fn drop_chunk(&mut self, key: ChunkKey) {
        if let Some(gpu) = self.cells.remove(&key) {
            for m in &gpu.meshes {
                let bytes = (m.vbuf.size() + m.ibuf.size()) as usize;
                self.uploaded_bytes = self.uploaded_bytes.saturating_sub(bytes);
            }
        }
    }

    /// Forget every uploaded chunk of `cell`. Used when a cell's chunk table is
    /// re-partitioned (SLICE 2 finer chunking on edit) and its old chunk ids no
    /// longer match cull's output.
    pub fn drop_cell(&mut self, cell: CellId) {
        let keys: Vec<ChunkKey> = self.cells.keys().copied().filter(|&(c, _)| c == cell).collect();
        for key in keys {
            self.drop_chunk(key);
        }
    }

    /// Replace the edit-overlay geometry (Phase 3). `meshes` are per-layer meshes
    /// in **world nm**; the renderer draws them on top of the base with one
    /// identity, origin-rebased instance. Tiny by construction (a handful of
    /// moved/added shapes), so a full rebuild per edit is cheap.
    pub fn upload_overlay(&mut self, device: &wgpu::Device, meshes: &[LayerMesh]) {
        self.overlay = meshes
            .iter()
            .filter(|m| !m.indices.is_empty())
            .map(|m| build_mesh(device, &self.layer_bgl, m.layer, &m.vertices, &m.indices))
            .collect();
    }

    /// Drop all overlay geometry (e.g. on load of a new design).
    pub fn clear_overlay(&mut self) {
        self.overlay.clear();
    }

    pub fn set_layer_visible(&mut self, layer: Layer, visible: bool) {
        if visible {
            self.hidden.remove(&layer);
        } else {
            self.hidden.insert(layer);
        }
    }

    /// Pack this frame's visible set into the instance buffer and record the draw
    /// plan. Instance translations are rebased against `camera.world_origin` in
    /// f64 before the f32 cast (the deep-zoom precision fix).
    pub fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, camera: &Camera, visible: &VisibleSet) {
        // Camera uniform.
        let g = Globals {
            view_proj: camera.view_proj().to_cols_array_2d(),
        };
        queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&g));

        let ox = camera.world_origin.x as f64;
        let oy = camera.world_origin.y as f64;

        // Flatten all instances into one buffer: cells first, then proxies.
        let total: usize =
            visible.draws.iter().map(|d| d.transforms.len()).sum::<usize>() + visible.proxies.len();
        let mut raw: Vec<InstanceRaw> = Vec::with_capacity(total);

        self.cell_plan.clear();
        // Phase 4 STEP 2: visible chunks whose geometry isn't uploaded yet (a
        // tessellation worker job is in flight) are drawn as a coarse bbox proxy
        // at each placement until the real mesh arrives — progressive refinement.
        // On native / no-worker this is always empty: the app tessellates every
        // visible chunk inline before paint, so the loop below always finds it.
        // Phase 7.3 SLICE C: a chunk that knows its look (a streamed design's)
        // proxies in the colour its layers make.
        let mut pending_proxies: Vec<(u32, Bboxf)> = Vec::new();
        for d in &visible.draws {
            let key = (d.cell, d.chunk);
            // Phase 7.6: a dense chunk is its proxy even when its mesh is
            // loaded (it was drawn at a closer zoom).
            if d.dense || !self.cells.contains_key(&key) {
                // Not tessellated yet: proxy the chunk's world footprint at each
                // placement (a loose AABB of the local bbox under the transform).
                for t in &d.transforms {
                    pending_proxies.push((d.rgba, t.apply_bbox(&d.bbox)));
                }
                continue;
            }
            let first = raw.len() as u32;
            for t in &d.transforms {
                raw.push(InstanceRaw {
                    row0: [t.a as f32, t.c as f32, (t.tx - ox) as f32, 0.0],
                    row1: [t.b as f32, t.d as f32, (t.ty - oy) as f32, 0.0],
                });
            }
            self.cell_plan.push(CellPlan {
                key,
                first,
                count: d.transforms.len() as u32,
            });
        }

        // Proxies: the culler's LOD boxes (sub-pixel cells) plus this frame's
        // not-yet-tessellated chunk boxes, grouped by colour: neutral first,
        // then each layer colour in layer order (a fixed order, so blending is
        // reproducible).
        pending_proxies.sort_by_key(|(l, _)| *l);
        self.proxy_groups.clear();
        let push_box = |raw: &mut Vec<InstanceRaw>, b: &Bboxf| {
            let w = (b.max_x - b.min_x).max(0.0) as f32;
            let h = (b.max_y - b.min_y).max(0.0) as f32;
            raw.push(InstanceRaw {
                row0: [w, 0.0, (b.min_x - ox) as f32, 0.0],
                row1: [0.0, h, (b.min_y - oy) as f32, 0.0],
            });
        };
        let first = raw.len() as u32;
        for b in &visible.proxies {
            push_box(&mut raw, b);
        }
        let mut i = 0;
        while i < pending_proxies.len() && pending_proxies[i].0 == 0 {
            push_box(&mut raw, &pending_proxies[i].1);
            i += 1;
        }
        if raw.len() as u32 > first {
            self.proxy_groups.push((0, first, raw.len() as u32 - first));
        }
        while i < pending_proxies.len() {
            let rgba = pending_proxies[i].0;
            let first = raw.len() as u32;
            while i < pending_proxies.len() && pending_proxies[i].0 == rgba {
                push_box(&mut raw, &pending_proxies[i].1);
                i += 1;
            }
            if !self.proxy_binds.contains_key(&rgba) {
                let bind = color_bind(device, &self.layer_bgl, unpack_rgba(rgba), "proxy-look");
                self.proxy_binds.insert(rgba, bind);
            }
            self.proxy_groups.push((rgba, first, raw.len() as u32 - first));
        }

        // Phase 3: one identity, origin-rebased instance for the overlay. Its
        // meshes are already in world nm, so the only transform is the rebase.
        self.overlay_first = raw.len() as u32;
        if !self.overlay.is_empty() {
            raw.push(InstanceRaw {
                row0: [1.0, 0.0, -ox as f32, 0.0],
                row1: [0.0, 1.0, -oy as f32, 0.0],
            });
        }

        // Expand the per-chunk plans into per-mesh draws and put them back into
        // layer-major order within each cell (see `DrawItem`).
        let mut seq = std::mem::take(&mut self.draw_seq);
        seq.clear();
        for (pi, plan) in self.cell_plan.iter().enumerate() {
            let Some(gpu) = self.cells.get(&plan.key) else { continue };
            for (mi, mesh) in gpu.meshes.iter().enumerate() {
                seq.push(DrawItem {
                    cell: plan.key.0,
                    layer: mesh.layer,
                    chunk: plan.key.1,
                    plan: pi as u32,
                    mesh: mi as u32,
                });
            }
        }
        seq.sort_unstable_by_key(|d| (d.cell, d.layer, d.chunk));
        self.draw_seq = seq;

        if raw.is_empty() {
            return;
        }

        // Grow the instance buffer if needed.
        if raw.len() > self.instcap {
            self.instcap = raw.len().next_power_of_two();
            self.instbuf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("instances"),
                size: self.instcap as u64 * INST_SIZE,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        queue.write_buffer(&self.instbuf, 0, bytemuck::cast_slice(&raw));
    }

    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.globals_bind, &[]);

        // LOD proxies first (behind real geometry), one draw per colour.
        for &(rgba, first, count) in &self.proxy_groups {
            let bind = self.proxy_binds.get(&rgba).unwrap_or(&self.proxy.color_bind);
            pass.set_vertex_buffer(1, self.instbuf.slice(first as u64 * INST_SIZE..));
            pass.set_bind_group(1, bind, &[]);
            pass.set_vertex_buffer(0, self.proxy.vbuf.slice(..));
            pass.set_index_buffer(self.proxy.ibuf.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..self.proxy.index_count, 0, 0..count);
        }

        // Instanced cell geometry, in (cell, layer, chunk) order. Slicing the
        // instance buffer per chunk keeps first_instance = 0, which the WebGL2
        // fallback requires.
        for item in &self.draw_seq {
            if self.hidden.contains(&item.layer) {
                continue;
            }
            let plan = &self.cell_plan[item.plan as usize];
            let Some(cell) = self.cells.get(&plan.key) else {
                continue;
            };
            let mesh = &cell.meshes[item.mesh as usize];
            pass.set_vertex_buffer(1, self.instbuf.slice(plan.first as u64 * INST_SIZE..));
            pass.set_bind_group(1, &mesh.color_bind, &[]);
            pass.set_vertex_buffer(0, mesh.vbuf.slice(..));
            pass.set_index_buffer(mesh.ibuf.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..mesh.index_count, 0, 0..plan.count);
        }

        // Phase 3 overlay geometry, on top of the base, one identity instance.
        // `upload_overlay` already built the meshes in layer order (per-layer
        // meshes come out of the tessellator sorted), so the draw is deterministic.
        if !self.overlay.is_empty() {
            let off = self.overlay_first as u64 * INST_SIZE;
            pass.set_vertex_buffer(1, self.instbuf.slice(off..));
            for mesh in &self.overlay {
                if self.hidden.contains(&mesh.layer) {
                    continue;
                }
                pass.set_bind_group(1, &mesh.color_bind, &[]);
                pass.set_vertex_buffer(0, mesh.vbuf.slice(..));
                pass.set_index_buffer(mesh.ibuf.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.index_count, 0, 0..1);
            }
        }
    }
}

/// A colour uniform + its bind group (per mesh, or per proxy layer).
fn color_bind(device: &wgpu::Device, layer_bgl: &wgpu::BindGroupLayout, color: [f32; 4], label: &str) -> wgpu::BindGroup {
    let ubuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents: bytemuck::bytes_of(&LayerUniform { color }),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout: layer_bgl,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: ubuf.as_entire_binding(),
        }],
    })
}

fn build_mesh(
    device: &wgpu::Device,
    layer_bgl: &wgpu::BindGroupLayout,
    layer: Layer,
    vertices: &[fluideda_core::Vertex],
    indices: &[u32],
) -> GpuMesh {
    let color_bind = color_bind(device, layer_bgl, palette::color_for(layer), "layer-bind");
    let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("verts"),
        contents: bytemuck::cast_slice(vertices),
        usage: wgpu::BufferUsages::VERTEX,
    });
    let ibuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("indices"),
        contents: bytemuck::cast_slice(indices),
        usage: wgpu::BufferUsages::INDEX,
    });
    GpuMesh {
        layer,
        vbuf,
        ibuf,
        index_count: indices.len() as u32,
        color_bind,
    }
}

fn build_proxy(device: &wgpu::Device, layer_bgl: &wgpu::BindGroupLayout) -> GpuMesh {
    // Unit square in [0,1]^2; per-instance transform scales/translates it to a box.
    let verts: [fluideda_core::Vertex; 4] = [
        fluideda_core::Vertex { pos: [0.0, 0.0] },
        fluideda_core::Vertex { pos: [1.0, 0.0] },
        fluideda_core::Vertex { pos: [1.0, 1.0] },
        fluideda_core::Vertex { pos: [0.0, 1.0] },
    ];
    let idx: [u32; 6] = [0, 1, 2, 0, 2, 3];
    let color = [0.55, 0.6, 0.7, 0.35]; // dim slate — reads as "cell block" when zoomed out
    let ubuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("proxy-color"),
        contents: bytemuck::bytes_of(&LayerUniform { color }),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let color_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("proxy-bind"),
        layout: layer_bgl,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: ubuf.as_entire_binding(),
        }],
    });
    let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("proxy-verts"),
        contents: bytemuck::cast_slice(&verts),
        usage: wgpu::BufferUsages::VERTEX,
    });
    let ibuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("proxy-indices"),
        contents: bytemuck::cast_slice(&idx),
        usage: wgpu::BufferUsages::INDEX,
    });
    GpuMesh {
        layer: Layer::new(u16::MAX, u16::MAX),
        vbuf,
        ibuf,
        index_count: 6,
        color_bind,
    }
}

fn uniform_entry(binding: u32, vis: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: vis,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}


#[cfg(test)]
mod composite_tests {
    use super::*;

    #[test]
    fn a_full_single_layer_is_that_layer_and_nothing_is_zero() {
        let l = Layer::new(68, 20);
        let [r, g, b, a] = palette::color_for(l);
        let v = unpack_rgba(composite_rgba(&[(l, 255)], |_| false));
        for (got, want) in v.iter().zip([r, g, b, a]) {
            assert!((got - want).abs() < 10.0 / 255.0, "{v:?} vs {:?}", [r, g, b, a]);
        }
        assert_eq!(composite_rgba(&[], |_| false), 0);
        assert_eq!(composite_rgba(&[(l, 255)], |x| x == l), 0, "a hidden layer adds nothing");
        // Sparse coverage is translucent: alpha well below the layer's own.
        let sparse = unpack_rgba(composite_rgba(&[(l, 13)], |_| false));
        assert!(sparse[3] < a / 4.0);
    }
}
