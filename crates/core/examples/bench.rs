//! Pipeline bench: measure load + tessellation cost and data sizes.
//! Run: cargo run --release -p fluideda-core --example bench

use std::time::Instant;

const SAMPLE: &[u8] = include_bytes!("../../../assets/sample.gds");

fn main() {
    println!("=== INPUT ===");
    println!("sample.gds: {} bytes ({:.1} KB)", SAMPLE.len(), SAMPLE.len() as f64 / 1024.0);

    // --- parse ---
    let t0 = Instant::now();
    let layout = fluideda_core::gds::load_gds_bytes(SAMPLE).expect("parse");
    let t_parse = t0.elapsed();

    let npoly = layout.polygons.len();
    let total_pts: usize = layout.polygons.iter().map(|p| p.points.len()).sum();
    let bbox = layout.bbox();

    println!("\n=== PARSE (GDS -> flat polygons) ===");
    println!("time:      {:?}", t_parse);
    println!("polygons:  {npoly}");
    println!("points:    {total_pts} (avg {:.1}/poly)", total_pts as f64 / npoly.max(1) as f64);
    println!("bbox:      ({:.0},{:.0}) .. ({:.0},{:.0}) nm", bbox.min.x, bbox.min.y, bbox.max.x, bbox.max.y);

    // --- tessellate ---
    let t1 = Instant::now();
    let meshes = fluideda_core::tessellate(&layout);
    let t_tess = t1.elapsed();

    let mut total_v = 0usize;
    let mut total_i = 0usize;
    println!("\n=== TESSELLATE (polygons -> triangles, per layer) ===");
    println!("time:      {:?}", t_tess);
    println!("layers:    {}", meshes.len());
    println!("{:<20} {:>10} {:>10} {:>12} {:>12}", "layer", "verts", "indices", "tris", "gpu_bytes");
    for m in &meshes {
        let vb = m.vertices.len() * std::mem::size_of::<fluideda_core::Vertex>();
        let ib = m.indices.len() * 4;
        total_v += m.vertices.len();
        total_i += m.indices.len();
        println!(
            "{:<20} {:>10} {:>10} {:>12} {:>12}",
            format!("{:?}", m.layer),
            m.vertices.len(),
            m.indices.len(),
            m.indices.len() / 3,
            vb + ib
        );
    }

    let vbytes = total_v * std::mem::size_of::<fluideda_core::Vertex>();
    let ibytes = total_i * 4;
    println!("\n=== TOTALS ===");
    println!("vertices:  {total_v} ({} bytes = {:.1} KB)", vbytes, vbytes as f64 / 1024.0);
    println!("indices:   {total_i} ({} bytes = {:.1} KB)", ibytes, ibytes as f64 / 1024.0);
    println!("triangles: {}", total_i / 3);
    println!("GPU vertex+index buffers: {:.1} KB", (vbytes + ibytes) as f64 / 1024.0);
    println!("Vertex size: {} bytes", std::mem::size_of::<fluideda_core::Vertex>());
    println!("\ntotal load+tess: {:?}", t_parse + t_tess);
}
