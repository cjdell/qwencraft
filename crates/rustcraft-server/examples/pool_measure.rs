// Measure total vertex/index demand for a radius-7 sphere of chunk regions
// (what the client terrain pool must be able to hold while walking).
use rustcraft_server::World;
use rustcraft_world::ChunkPos;

fn measure(world: &mut World, center: ChunkPos, label: &str) {
    let mut total_v: usize = 0;
    let mut total_i: usize = 0;
    let mut n: u32 = 0;
    let mut worst_v: usize = 0;
    for dx in -7i32..=7 {
        for dy in -7..=7 {
            for dz in -7..=7 {
                if dx * dx + dy * dy + dz * dz > 49 {
                    continue;
                }
                let pos = ChunkPos {
                    x: center.x + dx,
                    y: center.y + dy,
                    z: center.z + dz,
                };
                world.generate(pos);
                let data = world.region(pos);
                let mesh = rustcraft_world::mesh::build_chunk_mesh(
                    (pos.x * 16, pos.y * 16, pos.z * 16),
                    &data,
                );
                if mesh.vertices.is_empty() {
                    continue;
                }
                n += 1;
                let v = mesh.vertices.len() / 6;
                worst_v = worst_v.max(v);
                total_v += v;
                total_i += mesh.indices.len();
            }
        }
    }
    println!(
        "{label}: {n} chunks with faces | verts={total_v} ({:.1} MB) idx={total_i} ({:.1} MB) | worst chunk {} verts | avg {}",
        total_v as f64 * 24.0 / 1e6,
        total_i as f64 * 4.0 / 1e6,
        worst_v,
        total_v / n.max(1) as usize
    );
}

fn main() {
    let spawn_h = World::new(1337).height_at(0, 0);
    let center = ChunkPos {
        x: 0,
        y: spawn_h / 16,
        z: 0,
    };
    let mut world = World::new(1337);
    // Fresh spawn view.
    measure(&mut world, center, "spawn");
    // Simulate 30s of walking (135 blocks) east, then measure the new view
    // WITHOUT clearing the world (trail chunks still exist server-side).
    let mut x = 0;
    for _ in 0..9 {
        x += 15;
        let c = ChunkPos {
            x: x / 16,
            y: spawn_h / 16,
            z: 0,
        };
        measure(&mut world, c, &format!("walk x={x}"));
    }
}
