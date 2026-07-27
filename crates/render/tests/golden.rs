//! Golden-image invariant: the render pipeline must produce the same picture
//! across code changes. This is the guard the streaming parser (Phase 2b step 3)
//! is built behind — a dropped GDS record is missing geometry that renders fine
//! and passes every native unit test, so the only thing that catches it is
//! pixels.
//!
//! ## What it does
//!
//! Renders `hier.gds` (embedded) and, when present, `sky130` (gitignored real
//! data) at the same fit/mid/leaf viewports `shot.rs` uses, through the exact
//! `HierRenderer` + culler the app uses, to an offscreen texture on the native
//! wgpu adapter. It diffs each frame against a committed reference PNG.
//!
//! ## Tolerance, and why it isn't zero
//!
//! On one machine the pipeline is deterministic — Phase 2b step 0 made the draw
//! order stable, so two runs are byte-identical. The reference PNGs were captured
//! on this repo's dev machine (Apple M5 / Metal). GPU rasterisation is not
//! bit-identical across drivers/GPUs, so a strict byte compare would be a false
//! alarm the first time this runs on other hardware. The tolerance absorbs
//! driver-level AA/rounding differences while staying far tighter than any real
//! dropped-geometry regression:
//!
//!   * a pixel is "changed" if any RGB channel differs by more than
//!     [`CHANNEL_TOL`] (8/255);
//!   * the test fails if more than [`FRAC_TOL`] (0.5%) of pixels are changed.
//!
//! A single dropped shape instanced across a leaf array, or any dropped cell,
//! moves far more than 0.5% of pixels — the regressions this exists to catch are
//! orders of magnitude above the noise floor it tolerates. If you are on the
//! capture machine the diff is exactly zero; the budget is headroom, not slack we
//! expect to use. Regenerate the references with `UPDATE_GOLDEN=1 cargo test -p
//! fluideda-render --test golden` after an *intended* visual change, and eyeball
//! the diff before committing.

use fluideda_core::{cull, cull_with_overlay, Bboxf, EditOverlay, Polygon};
use fluideda_render::{Camera, HierRenderer};
use glam::Vec2;

const HIER: &[u8] = include_bytes!("../../../assets/hier.gds");
const SKY130: &str = "../../testdata/sky130_fd_sc_hd_merged.gds";
/// Tiny fixture whose TOP cell has direct editable shapes (see gen_edit_fixture.py).
const EDIT_FIXTURE: &[u8] = include_bytes!("../../../assets/edit_fixture.gds");

/// Reference resolution. Smaller than shot.rs's 1000² so the committed PNGs stay
/// a few KB each; still ample to catch a dropped record (a missing cell is
/// hundreds of pixels here).
const W: u32 = 512;
const H: u32 = 512;
const MIN_PIXEL: f64 = 4.0;
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Per-channel delta below which a pixel counts as unchanged (driver AA noise).
const CHANNEL_TOL: u8 = 8;
/// Max fraction of pixels allowed to exceed `CHANNEL_TOL` before the test fails.
const FRAC_TOL: f64 = 0.005;

/// The three viewport names, in the order [`render_file_async`] computes their
/// spans. Sized to the design (as in shot.rs) so this works for any file.
const VIEW_NAMES: [&str; 3] = ["fit", "mid", "leaf"];

#[test]
fn hier_golden() {
    let cases = render_file("hier", HIER);
    check_all("hier", &cases);
}

#[test]
fn sky130_golden() {
    match std::fs::read(SKY130) {
        Ok(bytes) => {
            let cases = render_file("sky130", &bytes);
            check_all("sky130", &cases);
        }
        Err(_) => {
            // Real data is gitignored (4.2 MB). hier (embedded) still guards the
            // pipeline in a bare checkout; skip sky130 rather than fail.
            eprintln!("SKIP sky130_golden: {SKY130} not present (fetch testdata to enable)");
        }
    }
}

/// Render all three viewports of one file; returns (view name, RGBA pixels).
fn render_file(label: &str, bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    pollster::block_on(render_file_async(label, bytes))
}

async fn render_file_async(_label: &str, bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let db = fluideda_core::gds::load_gds_db(bytes).expect("parse");
    let wb = db.world_bbox();
    let (cx, cy) = ((wb.min_x + wb.max_x) * 0.5, (wb.min_y + wb.max_y) * 0.5);

    let mid_span = (wb.width() * 0.03).clamp(20_000.0, 300_000.0);
    let spans = [
        wb.width().max(wb.height()) * 1.05, // fit
        mid_span,                           // mid
        5_000.0,                            // leaf
    ];

    let instance = wgpu::Instance::default();
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions::default())
        .await
        .expect("no wgpu adapter");
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor::default(), None)
        .await
        .expect("request_device");

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("golden-target"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let tview = target.create_view(&Default::default());
    let bytes_per_row = (W * 4).div_ceil(256) * 256;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("golden-readback"),
        size: (bytes_per_row * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut out = Vec::new();
    for (name, span) in VIEW_NAMES.iter().zip(spans) {
        // Fresh renderer per view so lazy uploads don't leak across viewports
        // (matches shot.rs; keeps each frame a clean render of just its content).
        let mut renderer = HierRenderer::new(&device, FORMAT);
        let mut cam = Camera::new(Vec2::new(W as f32, H as f32));
        cam.center = Vec2::new(cx as f32, cy as f32);
        cam.scale = W as f32 / span as f32;
        cam.world_origin = cam.center;

        let half_w = span * 0.5;
        let half_h = half_w * (H as f64 / W as f64);
        let vp = Bboxf::new(cx - half_w, cy - half_h, cx + half_w, cy + half_h);

        let vs = cull(&db, &vp, cam.scale as f64, MIN_PIXEL);
        for d in &vs.draws {
            let key = (d.cell, d.chunk);
            if !renderer.has_chunk(key) {
                let cell = &db.cells[d.cell];
                let meshes =
                    fluideda_core::tessellate_chunk(&cell.shapes, &cell.chunks[d.chunk as usize]);
                renderer.upload_chunk(&device, key, &meshes);
            }
        }
        renderer.prepare(&device, &queue, &cam, &vs);

        let mut enc = device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("golden"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &tview,
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

        out.push((name.to_string(), pixels));
    }
    out
}

/// Phase 3 edit-overlay goldens. Renders the fixture (whose TOP cell has direct
/// shapes) at fit under four overlays and checks:
///   * the empty overlay renders **byte-identical** to the no-overlay path
///     (the composite must be a true no-op when nothing is edited);
///   * a known move and a known delete match committed reference PNGs;
///   * the move and delete actually changed the picture (guards against a
///     silent no-op edit passing by rendering the base).
#[test]
fn edit_overlay_goldens() {
    let frames = pollster::block_on(render_edit_frames());
    let get = |name: &str| -> &Vec<u8> { &frames.iter().find(|(n, _)| *n == name).unwrap().1 };

    // Empty overlay must be a true no-op: exact byte match to the base (cull),
    // not merely within tolerance.
    assert_eq!(
        get("base"),
        get("empty"),
        "empty overlay must render byte-identical to the no-overlay path"
    );

    check_one("edit_base", get("base"));
    check_one("edit_moved", get("moved"));
    check_one("edit_deleted", get("deleted"));

    // Sanity: the edits must actually move pixels, or the goldens prove nothing.
    let (moved_changed, _) = diff(get("moved"), get("base"));
    let (deleted_changed, _) = diff(get("deleted"), get("base"));
    assert!(
        moved_changed as f64 / (W * H) as f64 > 0.01,
        "move changed only {moved_changed} px — suspiciously close to a no-op"
    );
    assert!(
        deleted_changed as f64 / (W * H) as f64 > 0.005,
        "delete changed only {deleted_changed} px — suspiciously close to a no-op"
    );
}

/// Render the edit fixture at fit under: no overlay ("base"), an empty overlay
/// ("empty"), a moved shape 0 ("moved"), a deleted shape 2 ("deleted").
async fn render_edit_frames() -> Vec<(&'static str, Vec<u8>)> {
    let db = fluideda_core::gds::load_gds_db(EDIT_FIXTURE).expect("parse fixture");
    let top = db.top;
    let wb = db.world_bbox();
    let (cx, cy) = ((wb.min_x + wb.max_x) * 0.5, (wb.min_y + wb.max_y) * 0.5);
    let span = wb.width().max(wb.height()) * 1.05;

    // Move shape 0 by a large, obvious delta; delete shape 2.
    let s0 = base_shape(&db, top, 0);
    let mut moved = EditOverlay::new();
    moved.move_shape((top, 0), fluideda_core::edit::translate(s0.layer, &s0.points, 1200, 700));
    let mut deleted = EditOverlay::new();
    deleted.delete((top, 2));
    let empty = EditOverlay::new();

    let instance = wgpu::Instance::default();
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions::default())
        .await
        .expect("no wgpu adapter");
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor::default(), None)
        .await
        .expect("request_device");

    let overlays: [(&'static str, Option<&EditOverlay>); 4] = [
        ("base", None),
        ("empty", Some(&empty)),
        ("moved", Some(&moved)),
        ("deleted", Some(&deleted)),
    ];
    let mut out = Vec::new();
    for (name, overlay) in overlays {
        let px = render_frame(&device, &queue, &db, cx, cy, span, overlay);
        out.push((name, px));
    }
    out
}

/// An owned copy of base shape `idx` in `cell` (for building a moved copy).
fn base_shape(db: &fluideda_core::CellDb, cell: usize, idx: usize) -> Polygon {
    let s = &db.cell(cell).shapes;
    Polygon { layer: s.layer(idx), points: s.points(idx).to_vec() }
}

/// Render one fit frame of `db` with an optional overlay, mirroring the app's
/// compositing exactly: cull (with overlay), tessellate dirty base chunks with
/// suppressions, tessellate the overlay geometry, draw. Returns RGBA pixels.
fn render_frame(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    db: &fluideda_core::CellDb,
    cx: f64,
    cy: f64,
    span: f64,
    overlay: Option<&EditOverlay>,
) -> Vec<u8> {
    let mut renderer = HierRenderer::new(device, FORMAT);
    let mut cam = Camera::new(Vec2::new(W as f32, H as f32));
    cam.center = Vec2::new(cx as f32, cy as f32);
    cam.scale = W as f32 / span as f32;
    cam.world_origin = cam.center;

    let half_w = span * 0.5;
    let half_h = half_w * (H as f64 / W as f64);
    let vp = Bboxf::new(cx - half_w, cy - half_h, cx + half_w, cy + half_h);

    let vs = match overlay {
        Some(o) => cull_with_overlay(db, &vp, cam.scale as f64, MIN_PIXEL, o),
        None => cull(db, &vp, cam.scale as f64, MIN_PIXEL),
    };
    for d in &vs.draws {
        let key = (d.cell, d.chunk);
        if renderer.has_chunk(key) {
            continue;
        }
        let cell = &db.cells[d.cell];
        let chunk = &cell.chunks[d.chunk as usize];
        let meshes = if d.dirty {
            // Dirty chunk: re-tessellate without the overlay's suppressed shapes.
            let empty = std::collections::HashSet::new();
            let suppressed = overlay.and_then(|o| o.suppressed_in(d.cell)).unwrap_or(&empty);
            fluideda_core::tessellate_chunk_filtered(&cell.shapes, chunk, suppressed)
        } else {
            fluideda_core::tessellate_chunk(&cell.shapes, chunk)
        };
        renderer.upload_chunk(device, key, &meshes);
    }
    // Overlay geometry (moved copies + adds), tessellated and uploaded on top.
    if !vs.overlay.is_empty() {
        let meshes = fluideda_core::tessellate_shapes(&vs.overlay);
        renderer.upload_overlay(device, &meshes);
    }
    renderer.prepare(device, queue, &cam, &vs);

    read_frame(device, queue, &renderer)
}

/// Draw a prepared renderer to an offscreen target and read back RGBA pixels.
fn read_frame(device: &wgpu::Device, queue: &wgpu::Queue, renderer: &HierRenderer) -> Vec<u8> {
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("edit-target"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let tview = target.create_view(&Default::default());
    let bytes_per_row = (W * 4).div_ceil(256) * 256;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("edit-readback"),
        size: (bytes_per_row * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = device.create_command_encoder(&Default::default());
    {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("edit"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &tview,
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
    pixels
}

/// Golden-check a single named image (same tolerance as [`check_all`]).
fn check_one(name: &str, pixels: &[u8]) {
    let path = golden_path(name, "fit");
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        write_png(&path, pixels);
        eprintln!("wrote golden {}", path.display());
        return;
    }
    let reference = read_png(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden {} ({e}). Capture with UPDATE_GOLDEN=1 cargo test \
             -p fluideda-render --test golden",
            path.display()
        )
    });
    let (changed, worst) = diff(pixels, &reference);
    let frac = changed as f64 / (W * H) as f64;
    eprintln!("{name}: {changed} px changed ({:.4}%), worst channel Δ {worst}", frac * 100.0);
    assert!(
        frac <= FRAC_TOL,
        "{name}: {:.4}% pixels changed (> {:.4}% tol), worst Δ {worst}",
        frac * 100.0,
        FRAC_TOL * 100.0
    );
}

fn golden_path(file: &str, view: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{file}_{view}.png"))
}

fn check_all(file: &str, cases: &[(String, Vec<u8>)]) {
    let updating = std::env::var("UPDATE_GOLDEN").is_ok();
    let mut failures = Vec::new();
    for (view, pixels) in cases {
        let path = golden_path(file, view);
        if updating {
            write_png(&path, pixels);
            eprintln!("wrote golden {}", path.display());
            continue;
        }
        let reference = read_png(&path).unwrap_or_else(|e| {
            panic!(
                "missing golden {} ({e}). Capture it with \
                 UPDATE_GOLDEN=1 cargo test -p fluideda-render --test golden",
                path.display()
            )
        });
        let (changed, worst) = diff(pixels, &reference);
        let frac = changed as f64 / (W * H) as f64;
        eprintln!(
            "{file}/{view}: {changed} px changed ({:.4}%), worst channel Δ {worst}",
            frac * 100.0
        );
        if frac > FRAC_TOL {
            failures.push(format!(
                "{file}/{view}: {:.4}% pixels changed (> {:.4}% tol), worst Δ {worst}",
                frac * 100.0,
                FRAC_TOL * 100.0
            ));
        }
    }
    assert!(failures.is_empty(), "GOLDEN MISMATCH:\n  {}", failures.join("\n  "));
}

/// Count pixels whose RGB differs by more than `CHANNEL_TOL`, plus the worst
/// per-channel delta seen. Alpha is ignored (always opaque here).
fn diff(a: &[u8], b: &[u8]) -> (usize, u8) {
    assert_eq!(a.len(), b.len(), "size mismatch (resolution changed?)");
    let mut changed = 0;
    let mut worst = 0u8;
    for (pa, pb) in a.chunks_exact(4).zip(b.chunks_exact(4)) {
        let mut over = false;
        for c in 0..3 {
            let d = pa[c].abs_diff(pb[c]);
            worst = worst.max(d);
            if d > CHANNEL_TOL {
                over = true;
            }
        }
        if over {
            changed += 1;
        }
    }
    (changed, worst)
}

fn write_png(path: &std::path::Path, rgba: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let file = std::fs::File::create(path).unwrap();
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(rgba).unwrap();
}

fn read_png(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let dec = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    buf.truncate(info.buffer_size());
    Ok(buf)
}
