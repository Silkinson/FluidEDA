//! fluideda-render — wgpu pipelines for drawing **hierarchical** layout geometry.
//!
//! Each cell definition is tessellated once (cell-local nm) and uploaded as a
//! static per-layer mesh. Every frame the culler ([`fluideda_core::cull`]) hands
//! us, per visible cell def, the world transforms at which to draw it; we pack
//! those into one instance buffer and issue **one instanced draw per cell/layer**
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

use bytemuck::{Pod, Zeroable};
use fluideda_core::{CellId, LayerMesh, Layer, VisibleSet};
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

/// A recorded instanced draw for this frame: a cell def and its slice of the
/// shared instance buffer.
struct CellPlan {
    cell: CellId,
    first: u32,
    count: u32,
}

/// Draws a hierarchical layout by instancing per-cell meshes.
pub struct HierRenderer {
    pipeline: wgpu::RenderPipeline,
    globals_buf: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,

    cells: HashMap<CellId, CellGpu>,
    proxy: GpuMesh, // unit-square [0,1]^2 mesh, dim color

    /// Shared instance buffer, grown as needed and rewritten each frame.
    instbuf: wgpu::Buffer,
    instcap: usize,

    hidden: HashSet<Layer>,

    // Per-frame plan (set in `prepare`, consumed in `paint`).
    cell_plan: Vec<CellPlan>,
    proxy_first: u32,
    proxy_count: u32,
}

const INST_SIZE: u64 = std::mem::size_of::<InstanceRaw>() as u64;

impl HierRenderer {
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        per_cell: &HashMap<CellId, Vec<LayerMesh>>,
    ) -> Self {
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
            bind_group_layouts: &[&globals_bgl, &layer_bgl],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fill-pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                compilation_options: Default::default(),
                buffers: &[
                    // slot 0: per-vertex cell-local position
                    wgpu::VertexBufferLayout {
                        array_stride: 8, // vec2<f32>
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &wgpu::vertex_attr_array![0 => Float32x2],
                    },
                    // slot 1: per-instance affine transform (two vec4)
                    wgpu::VertexBufferLayout {
                        array_stride: INST_SIZE,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &wgpu::vertex_attr_array![1 => Float32x4, 2 => Float32x4],
                    },
                ],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
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
            multiview: None,
            cache: None,
        });

        // Build per-cell GPU meshes.
        let mut cells = HashMap::new();
        for (&id, meshes) in per_cell {
            let gpu_meshes = meshes
                .iter()
                .filter(|m| !m.indices.is_empty())
                .map(|m| build_mesh(device, &layer_bgl, m.layer, &m.vertices, &m.indices))
                .collect::<Vec<_>>();
            if !gpu_meshes.is_empty() {
                cells.insert(id, CellGpu { meshes: gpu_meshes });
            }
        }

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
            cells,
            proxy,
            instbuf,
            instcap,
            hidden: HashSet::new(),
            cell_plan: Vec::new(),
            proxy_first: 0,
            proxy_count: 0,
        }
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
        for d in &visible.draws {
            if !self.cells.contains_key(&d.cell) {
                continue; // cell has no drawable geometry
            }
            let first = raw.len() as u32;
            for t in &d.transforms {
                raw.push(InstanceRaw {
                    row0: [t.a as f32, t.c as f32, (t.tx - ox) as f32, 0.0],
                    row1: [t.b as f32, t.d as f32, (t.ty - oy) as f32, 0.0],
                });
            }
            self.cell_plan.push(CellPlan {
                cell: d.cell,
                first,
                count: d.transforms.len() as u32,
            });
        }

        self.proxy_first = raw.len() as u32;
        self.proxy_count = visible.proxies.len() as u32;
        for b in &visible.proxies {
            let w = (b.max_x - b.min_x).max(0.0) as f32;
            let h = (b.max_y - b.min_y).max(0.0) as f32;
            raw.push(InstanceRaw {
                row0: [w, 0.0, (b.min_x - ox) as f32, 0.0],
                row1: [0.0, h, (b.min_y - oy) as f32, 0.0],
            });
        }

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

        // LOD proxies first (behind real geometry).
        if self.proxy_count > 0 {
            let off = self.proxy_first as u64 * INST_SIZE;
            pass.set_vertex_buffer(1, self.instbuf.slice(off..));
            pass.set_bind_group(1, &self.proxy.color_bind, &[]);
            pass.set_vertex_buffer(0, self.proxy.vbuf.slice(..));
            pass.set_index_buffer(self.proxy.ibuf.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..self.proxy.index_count, 0, 0..self.proxy_count);
        }

        // Instanced cell geometry. Slicing the instance buffer per cell keeps
        // first_instance = 0, which the WebGL2 fallback requires.
        for plan in &self.cell_plan {
            let Some(cell) = self.cells.get(&plan.cell) else {
                continue;
            };
            let off = plan.first as u64 * INST_SIZE;
            pass.set_vertex_buffer(1, self.instbuf.slice(off..));
            for mesh in &cell.meshes {
                if self.hidden.contains(&mesh.layer) {
                    continue;
                }
                pass.set_bind_group(1, &mesh.color_bind, &[]);
                pass.set_vertex_buffer(0, mesh.vbuf.slice(..));
                pass.set_index_buffer(mesh.ibuf.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.index_count, 0, 0..plan.count);
            }
        }
    }
}

fn build_mesh(
    device: &wgpu::Device,
    layer_bgl: &wgpu::BindGroupLayout,
    layer: Layer,
    vertices: &[fluideda_core::Vertex],
    indices: &[u32],
) -> GpuMesh {
    let color = palette::color_for(layer);
    let ubuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("layer-color"),
        contents: bytemuck::bytes_of(&LayerUniform { color }),
        usage: wgpu::BufferUsages::UNIFORM,
    });
    let color_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("layer-bind"),
        layout: layer_bgl,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: ubuf.as_entire_binding(),
        }],
    });
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
