use std::time::Instant;
use qwencraft_world::mesh::build_chunk_mesh;
use qwencraft_world::WorldGen;

fn region_for(gen: &WorldGen, cx: i32, cy: i32, cz: i32) -> Vec<u8> {
    let mut out = vec![0u8; 26 * 26 * 26];
    let idx = |x: i32, y: i32, z: i32| ((y * 26 + z) * 26 + x) as usize;
    let ox = cx * 16 - 5;
    let oy = cy * 16 - 5;
    let oz = cz * 16 - 5;
    for y in 0..26 {
        for z in 0..26 {
            for x in 0..26 {
                out[idx(x, y, z)] = gen.block_at(ox + x, oy + y, oz + z).as_u8();
            }
        }
    }
    out
}

fn main() {
    let gen = WorldGen::new(1337);
    // Warmup
    for i in 0..8 {
        let _ = gen.generate_chunk(i, 0, 0);
    }
    let n = 200;
    let t = Instant::now();
    for i in 0..n {
        let _ = gen.generate_chunk(10 + i, 1, 40 + i);
    }
    let gen_us = t.elapsed().as_micros() as f64 / n as f64;

    let regions: Vec<(i32, i32, i32, Vec<u8>)> = (0..n)
        .map(|i| {
            let (x, z) = (100 + i as i32, 100 + i as i32);
            let h = gen.height(x, z);
            let cy = ((h - 1) / 16).clamp(0, 3);
            (x / 16, cy, z / 16, region_for(&gen, x / 16, cy, z / 16))
        })
        .collect();
    // Warmup mesh
    for r in regions.iter().take(10) {
        let _ = build_chunk_mesh((r.0 * 16, r.1 * 16, r.2 * 16), &r.3);
    }
    let t = Instant::now();
    for r in &regions {
        let _ = build_chunk_mesh((r.0 * 16, r.1 * 16, r.2 * 16), &r.3);
    }
    let mesh_us = t.elapsed().as_micros() as f64 / n as f64;

    println!("BENCH native release: generate_chunk={gen_us:.0}us  build_chunk_mesh={mesh_us:.0}us");
    println!("BENCH per-frame estimate (6 gen + 4 mesh): {:.0}us", 6.0 * gen_us + 4.0 * mesh_us);
}
