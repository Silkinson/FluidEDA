//! fluideda-render — wgpu pipelines for drawing layout geometry + overlays.
//!
//! Consumes `LayerMesh`es from `fluideda-core`, uploads them once, and redraws
//! each frame with only the camera uniform changing. Kept free of any windowing
//! or app logic so it can be driven by an eframe paint callback or a worker
//! OffscreenCanvas (Phase 2).

mod camera;
mod palette;
pub mod overlay;

pub use camera::Camera;
pub use overlay::{Marker, OverlayRenderer, RemoteCursor};

use bytemuck::{Pod, Zeroable};
use fluideda_core::LayerMesh;
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

struct GpuLayer {
    vbuf: wgpu::Buffer,
    ibuf: wgpu::Buffer,
    index_count: u32,
    bind: wgpu::BindGroup,
    visible: bool,
}

/// Draws all layer geometry for one loaded layout.
pub struct LayoutRenderer {
    pipeline: wgpu::RenderPipeline,
    globals_buf: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,
    layer_bgl: wgpu::BindGroupLayout,
    layers: Vec<GpuLayer>,
}

impl LayoutRenderer {
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        meshes: &[LayerMesh],
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
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 8, // vec2<f32>
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2],
                }],
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

        let mut renderer = Self {
            pipeline,
            globals_buf,
            globals_bind,
            layer_bgl,
            layers: Vec::new(),
        };
        renderer.upload(device, meshes);
        renderer
    }

    fn upload(&mut self, device: &wgpu::Device, meshes: &[LayerMesh]) {
        for mesh in meshes {
            if mesh.indices.is_empty() {
                continue;
            }
            let color = palette::color_for(mesh.layer);
            let ubuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("layer-uniform"),
                contents: bytemuck::bytes_of(&LayerUniform { color }),
                usage: wgpu::BufferUsages::UNIFORM,
            });
            let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("layer-bind"),
                layout: &self.layer_bgl,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: ubuf.as_entire_binding(),
                }],
            });
            let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("verts"),
                contents: bytemuck::cast_slice(&mesh.vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
            let ibuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("indices"),
                contents: bytemuck::cast_slice(&mesh.indices),
                usage: wgpu::BufferUsages::INDEX,
            });
            self.layers.push(GpuLayer {
                vbuf,
                ibuf,
                index_count: mesh.indices.len() as u32,
                bind,
                visible: true,
            });
        }
    }

    pub fn set_layer_visible(&mut self, idx: usize, visible: bool) {
        if let Some(l) = self.layers.get_mut(idx) {
            l.visible = visible;
        }
    }

    pub fn layer_count(&self) -> usize {
        self.layers.len()
    }

    /// Update the camera uniform. Call once per frame before `draw`.
    pub fn update_camera(&self, queue: &wgpu::Queue, camera: &Camera) {
        let g = Globals {
            view_proj: camera.view_proj().to_cols_array_2d(),
        };
        queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&g));
    }

    /// Record draw calls into an existing render pass.
    pub fn draw(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.globals_bind, &[]);
        for layer in &self.layers {
            if !layer.visible {
                continue;
            }
            pass.set_bind_group(1, &layer.bind, &[]);
            pass.set_vertex_buffer(0, layer.vbuf.slice(..));
            pass.set_index_buffer(layer.ibuf.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..layer.index_count, 0, 0..1);
        }
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
