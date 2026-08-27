// Terrain-pool capacity check.
//
// Measures the TOTAL vertex/index demand of the EXACT streamed view
// (the radius-7 view the server sends, all chunk layers, opaque + water —
// i.e. everything the client terrain pool must hold) across several seeds
// and positions, and asserts the worst case stays under ~80% of
// `TERRAIN_POOL_VERTS`/`TERRAIN_POOL_IDX` (rustcraft_world).
//
// The caps must hold the whole view — not one chunk: if the view is
// bigger than the pool, compaction drops still-visible chunks and they
// thrash on the evict/re-send loop (holes in the landscape that only
// fill when a block edit re-sends them). Re-run after any change that
// adds vertices per chunk (new mesh features, terrain features, fog or
// view-radius changes).
//
//   cargo run --release -p rustcraft-server --example pool_measure
use rustcraft_server::{Server, World, VIEW_RADIUS};
use rustcraft_world::{ChunkPos, TERRAIN_POOL_IDX, TERRAIN_POOL_VERTS, WORLD_HEIGHT};

/// The exact set of chunk columns the Streamer sends (15x15 square
/// rounded by dx^2+dz^2 <= (VIEW_RADIUS+1)^2).
fn stream_columns(pc: ChunkPos) -> Vec<ChunkPos> {
    let mut cols = Vec::new();
    for dx in -VIEW_RADIUS..=VIEW_RADIUS {
        for dz in -VIEW_RADIUS..=VIEW_RADIUS {
            if dx * dx + dz * dz > (VIEW_RADIUS + 1) * (VIEW_RADIUS + 1) {
                continue;
            }
            cols.push(ChunkPos::new(pc.x + dx, 0, pc.z + dz));
        }
    }
    cols
}

/// Demand of the streamed view around `center` (all layers, opaque +
/// water), after pre-generating the view + a ±1 chunk context halo so
/// `region()` sees real neighbours (missing context samples as air and
/// inflates boundary meshes).
fn view_demand(world: &mut World, center: ChunkPos) -> (usize, usize, u32) {
    let halo = VIEW_RADIUS + 2;
    for dx in -halo..=halo {
        for dz in -halo..=halo {
            for cy in 0..=(WORLD_HEIGHT / rustcraft_world::CHUNK) {
                world.generate(ChunkPos::new(center.x + dx, cy, center.z + dz));
            }
        }
    }
    let mut v = 0usize;
    let mut i = 0usize;
    let mut n = 0u32;
    for col in stream_columns(center) {
        for cy in 0..(WORLD_HEIGHT / rustcraft_world::CHUNK) {
            let pos = ChunkPos::new(col.x, cy, col.z);
            let data = world.region(pos);
            let mesh = rustcraft_world::mesh::build_chunk_mesh(
                (pos.x * 16, pos.y * 16, pos.z * 16),
                &data,
            );
            let ov = mesh.vertices.len() / 6;
            let wv = mesh.water_vertices.len() / 6;
            if ov + wv == 0 {
                continue;
            }
            n += 1;
            v += ov + wv;
            i += mesh.indices.len() + mesh.water_indices.len();
        }
    }
    (v, i, n)
}

fn main() {
    let seeds = [1337u64, 42, 7, 999, 555, 12345, 31337, 2024, 77777, 888];
    // Spawn + a spread of positions (far from spawn is where the pool has
    // a trail and the bug used to show up).
    let offsets: [(i32, i32); 12] = [
        (120, 0),
        (-120, 0),
        (0, 120),
        (0, -120),
        (200, 150),
        (-200, -150),
        (300, -200),
        (-300, 250),
        (500, 500),
        (-500, -500),
        (400, -100),
        (-150, 350),
    ];
    let mut worst_v = 0usize;
    let mut worst_i = 0usize;
    let mut worst_label = String::new();
    for seed in seeds {
        let server = Server::new(seed);
        let p = server.player_state().pos;
        let mut world = World::new(seed);
        let positions: Vec<(i32, i32)> =
            std::iter::once((p.x as i32, p.z as i32)).chain(offsets.iter().copied()).collect();
        for &(ox, oz) in &positions {
            let pc = ChunkPos::of(rustcraft_world::BlockPos::new(ox, 0, oz));
            let (v, i, n) = view_demand(&mut world, pc);
            if v > worst_v {
                worst_v = v;
                worst_label = format!("seed {seed} @ ({ox},{oz})");
            }
            worst_i = worst_i.max(i);
            eprintln!("  seed {seed} @ ({ox},{oz}): {n} chunks | verts={v} idx={i}");
        }
    }
    let v_pct = worst_v as f64 / TERRAIN_POOL_VERTS as f64 * 100.0;
    let i_pct = worst_i as f64 / TERRAIN_POOL_IDX as f64 * 100.0;
    println!(
        "pool_measure: worst view {worst_label} | verts={worst_v} ({v_pct:.0}% of VERT_CAP={TERRAIN_POOL_VERTS}) idx={worst_i} ({i_pct:.0}% of IDX_CAP={TERRAIN_POOL_IDX})"
    );
    // The pool must hold the whole worst-case view with headroom (the
    // fog-bound trail coexists with the view, and fast movement briefly
    // overlaps old and new views).
    assert!(
        worst_v as f64 <= TERRAIN_POOL_VERTS as f64 * 0.8,
        "worst view uses {v_pct:.0}% of VERT_CAP (> 80%): raise TERRAIN_POOL_VERTS/IDX_CAP in rustcraft-world and re-verify the walk test"
    );
    assert!(
        worst_i as f64 <= TERRAIN_POOL_IDX as f64 * 0.8,
        "worst view uses {i_pct:.0}% of IDX_CAP (> 80%): raise TERRAIN_POOL_VERTS/IDX_CAP in rustcraft-world and re-verify the walk test"
    );
    println!("pool_measure: OK (worst view <= 80% of pool capacity)");
}
