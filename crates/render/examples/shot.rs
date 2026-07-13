//! Headless render-to-PNG + frame-time bench for the hierarchical viewer.
//!
//! Renders `assets/hier.gds` through the *exact same* `HierRenderer` + culler
//! the app uses, but to an offscreen texture on a native wgpu adapter (Metal /
//! Vulkan). Proves geometry, layer colors, culling, and deep-zoom precision
//! without a window, and produces images to diff against KLayout. Also times the
//! per-frame cull+prepare+draw cost to estimate fps.
//!
//! Run: cargo run --release -p fluideda-render --example shot
//! Output: target/shots/*.png

use fluideda_core::{cull, Bboxf};
use fluideda_render::{Camera, HierRenderer};
use glam::Vec2;
use std::collections::HashMap;
use std::time::Instant;

const HIER: &[u8] = include_bytes!("../../../assets/hier.gds");
const W: u32 = 1000;
const H: u32 = 1000;
const MIN_PIXEL: f64 = 4.0;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

struct View {
    name: String,
    center: (f64, f64),
    /// World width (nm) spanned by the viewport.
    span_nm: f64,
}

fn main() {
    pollster::block_on(run());
}

async fn run() {
    // Optional file arg; default to the embedded hier.gds.
    let arg = std::env::args().nth(1);
    let (label, bytes) = match &arg {
        Some(path) => (path.clone(), std::fs::read(path).expect("read file")),
        None => ("hier.gds".to_string(), HIER.to_vec()),
    };
    println!("file: {label} ({} bytes)", bytes.len());
    let db = fluideda_core::gds::load_gds_db(&bytes).expect("parse");
    let wb = db.world_bbox();
    let (cx, cy) = ((wb.min_x + wb.max_x) * 0.5, (wb.min_y + wb.max_y) * 0.5);

    // Viewports scale to the file: whole chip, ~3% window, and a 5um close-up,
    // all centered on the design so this works for any GDS, not just hier.
    let mid_span = (wb.width() * 0.03).clamp(20_000.0, 300_000.0);
    let views = [
        View { name: "fit".into(), center: (cx, cy), span_nm: wb.width().max(wb.height()) * 1.05 },
        View { name: "mid".into(), center: (cx, cy), span_nm: mid_span },
        View { name: "leaf_5um".into(), center: (cx, cy), span_nm: 5_000.0 },
    ];

    // --- headless wgpu device ---
    let instance = wgpu::Instance::default();
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions::default())
        .await
        .expect("no wgpu adapter");
    println!("adapter: {}", adapter.get_info().name);
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor::default(), None)
        .await
        .expect("request_device");

    // --- per-cell meshes + renderer ---
    let mut per_cell: HashMap<usize, Vec<fluideda_core::LayerMesh>> = HashMap::new();
    for (id, cell) in db.cells.iter().enumerate() {
        let m = fluideda_core::tessellate_shapes(&cell.shapes);
        if !m.is_empty() {
            per_cell.insert(id, m);
        }
    }
    let mut renderer = HierRenderer::new(&device, FORMAT, &per_cell);

    // --- offscreen target + readback buffer ---
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("target"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    let bytes_per_row = (W * 4).div_ceil(256) * 256; // wgpu 256-byte row alignment
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (bytes_per_row * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    std::fs::create_dir_all("target/shots").ok();
    let total = db.flat_shape_count();

    for v in &views {
        let mut cam = Camera::new(Vec2::new(W as f32, H as f32));
        cam.center = Vec2::new(v.center.0 as f32, v.center.1 as f32);
        cam.scale = W as f32 / v.span_nm as f32; // px per nm
        cam.world_origin = cam.center;

        let half_w = v.span_nm * 0.5;
        let half_h = half_w * (H as f64 / W as f64);
        let vp = Bboxf::new(
            v.center.0 - half_w,
            v.center.1 - half_h,
            v.center.0 + half_w,
            v.center.1 + half_h,
        );

        let vs = cull(&db, &vp, cam.scale as f64, MIN_PIXEL);
        renderer.prepare(&device, &queue, &cam, &vs);

        // Encode a clear + draw.
        let mut enc = device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("shot"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.05, g: 0.06, b: 0.08, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            renderer.draw(&mut pass);
        }
        enc.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &readback,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(H),
                },
            },
            wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        );
        queue.submit([enc.finish()]);

        // Map + write PNG.
        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
        device.poll(wgpu::Maintain::Wait);
        rx.recv().unwrap().unwrap();
        let data = slice.get_mapped_range();
        let mut pixels = Vec::with_capacity((W * H * 4) as usize);
        for y in 0..H {
            let row = (y * bytes_per_row) as usize;
            pixels.extend_from_slice(&data[row..row + (W * 4) as usize]);
        }
        drop(data);
        readback.unmap();

        let path = format!("target/shots/{}.png", v.name);
        write_png(&path, W, H, &pixels);

        // Frame-time: cull + prepare + submit, N times.
        let n = 120;
        let t = Instant::now();
        for _ in 0..n {
            let vs = cull(&db, &vp, cam.scale as f64, MIN_PIXEL);
            renderer.prepare(&device, &queue, &cam, &vs);
            let mut e = device.create_command_encoder(&Default::default());
            {
                let mut pass = e.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                renderer.draw(&mut pass);
            }
            queue.submit([e.finish()]);
        }
        device.poll(wgpu::Maintain::Wait);
        let per = t.elapsed() / n;
        let fps = 1.0 / per.as_secs_f64();

        println!(
            "{:<10} span={:>9.0}nm  visible={:>8}/{:<10} proxies={:>5} instances={:>6}  {:>8.2?}/frame  ~{:>5.0} fps  -> {}",
            v.name, v.span_nm, vs_stats(&db, &vp, cam.scale).0, total,
            vs_stats(&db, &vp, cam.scale).2, vs_stats(&db, &vp, cam.scale).1, per, fps, path
        );
    }
    println!("\nwrote PNGs to target/shots/ (compare against KLayout with the same viewports)");
}

fn vs_stats(db: &fluideda_core::CellDb, vp: &Bboxf, scale: f32) -> (u64, u64, usize) {
    let vs = cull(db, vp, scale as f64, MIN_PIXEL);
    (vs.stats.visible_shapes, vs.stats.instances_emitted, vs.stats.proxy_count)
}

fn write_png(path: &str, w: u32, h: u32, rgba: &[u8]) {
    let file = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(rgba).unwrap();
}
