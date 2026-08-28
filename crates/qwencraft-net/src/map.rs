//! 2D top-down world map for the dashboard (minimap).
//!
//! The map is **pure terrain + edit overlay**: terrain is a pure function of
//! (seed, coordinates), so this type owns its own [`WorldGen`] and a
//! persistent column-height cache and never touches the authoritative
//! world's chunk buffers (or its lock). Edits are synced in from the world's
//! edit history (last-wins per position), so the map reflects what players
//! have actually done — holes, towers, water removal.
//!
//! Per column the map reports the topmost non-air block as (y, block id).
//! The per-column logic (and its deliberate approximations, all invisible
//! at minimap scale) lives in `qwencraft_world::column_top`.
//!
//! The map is computed under its own lock in a separate task from the 60 Hz
//! tick, so even a cold ~100 ms compute never stalls the game.

use std::collections::HashMap;

use qwencraft_server::Edit;
use qwencraft_world::{Block, BlockPos, WorldGen, column_top_base, column_top_edited};

/// Max side of a map region (256x256 = 65k columns ≈ a few ms warm,
/// ~100 ms cold in the map task — never on the tick path).
pub const MAP_MAX: i32 = 256;
pub const MAP_MIN: i32 = 16;

/// A computed map region: the topmost block of every column in a
/// rectangular area, 2 bytes per column in row-major (z, then x) order:
/// `[y: u8, block id: u8]` (y == 255 means "no block" — unreachable in
/// practice: y=0 is unbreakable bedrock).
pub struct MapRegion {
    pub x0: i32,
    pub z0: i32,
    pub w: i32,
    pub h: i32,
    pub cols: Vec<u8>,
}

/// Dashboard map state: pure terrain source + persistent height cache +
/// edit overlay. Guarded by its own mutex (see module docs).
pub struct MapState {
    gen: WorldGen,
    /// Column heights (immutable terrain) — cached for the process lifetime.
    heights: HashMap<(i32, i32), i32>,
    /// Current block for every edited position (last-wins over history).
    edits: HashMap<BlockPos, Block>,
}

impl MapState {
    pub fn new(seed: u64) -> Self {
        Self {
            gen: WorldGen::new(seed),
            heights: HashMap::new(),
            edits: HashMap::new(),
        }
    }

    /// Fold a tail of the world's edit history into the overlay. The tick
    /// loop passes only new entries (the history is append-only), so each
    /// edit is applied exactly once.
    pub fn sync_edits(&mut self, edits: &[Edit]) {
        for e in edits {
            self.edits.insert(e.pos, e.block);
        }
    }

    /// Compute the top block of every column in a `w x h` region centred on
    /// (cx, cz) (each side clamped to `MAP_MIN..=MAP_MAX`; the origin is the
    /// chunk-aligned centre the dashboard computes from the same formula).
    pub fn top_map(&mut self, cx: i32, cz: i32, w: i32, h: i32) -> MapRegion {
        let w = w.clamp(MAP_MIN, MAP_MAX);
        let h = h.clamp(MAP_MIN, MAP_MAX);
        let x0 = cx - w / 2;
        let z0 = cz - h / 2;

        // Index the region's edits by column (the overlay is sparse: only
        // columns near players' edits have entries).
        let mut edit_cols: HashMap<(i32, i32), Vec<(i32, Block)>> = HashMap::new();
        for (pos, &block) in &self.edits {
            if (x0..x0 + w).contains(&pos.x) && (z0..z0 + h).contains(&pos.z) {
                edit_cols.entry((pos.x, pos.z)).or_default().push((pos.y, block));
            }
        }

        let mut cols = vec![0u8; (w * h) as usize * 2];
        let mut col = 0usize;
        for z in z0..z0 + h {
            for x in x0..x0 + w {
                let hgt = *self
                    .heights
                    .entry((x, z))
                    .or_insert_with(|| self.gen.height(x, z));
                let top = match edit_cols.get(&(x, z)) {
                    // No edits in this column: O(1) terrain arithmetic.
                    None => {
                        let (y, b) = column_top_base(x, z, hgt, &self.gen, &mut self.heights);
                        (y as u8, b.as_u8())
                    }
                    // Edited column: exact top-down scan (edits override
                    // terrain cell by cell).
                    Some(edits) => column_top_edited(x, z, hgt, edits, &self.gen, &mut self.heights)
                        // Unreachable in practice (y=0 bedrock is unbreakable)
                        // — the dashboard renders 255 as "unknown".
                        .map_or((255, 0), |(y, b)| (y as u8, b.as_u8())),
                };
                cols[col] = top.0;
                cols[col + 1] = top.1;
                col += 2;
            }
        }
        MapRegion { x0, z0, w, h, cols }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use qwencraft_server::Server;
    use qwencraft_world::{Block, SEA_LEVEL, SNOW_LEVEL, WORLD_HEIGHT};

    /// Reference: the authoritative world's own topmost non-air block for a
    /// column (materialises the column's chunks; includes caves, flowers and
    /// canopy overhang — the ground truth the map approximates).
    fn world_column_top(server: &mut Server, x: i32, z: i32) -> Option<(i32, Block)> {
        for y in (0..WORLD_HEIGHT).rev() {
            let b = server.world_mut().block_at(BlockPos::new(x, y, z));
            if b != Block::Air {
                return Some((y, b));
            }
        }
        None
    }

    /// The map must match the authoritative world column-for-column over an
    /// unedited region, modulo the documented approximations (both invisible
    /// at 1px/block):
    ///
    /// - flowers: the world's top is the flower at surface+1, the map
    ///   reports the surface one below;
    /// - canopy: when the world's topmost block is tree foliage, the map's
    ///   per-column `tree_at` approximation may differ — a neighbour tree's
    ///   overhanging leaves sit above this column's own surface (arbitrary
    ///   gap when over water), and on the trunk column the map reports the
    ///   leaves one above the log tip (the 3x3 cap's centre is air).
    ///
    /// Terrain, water, snow and the surface height are exact: any column
    /// whose world top is NOT tree foliage must match exactly (modulo
    /// flowers).
    #[test]
    fn map_matches_world_around_spawn() {
        let mut server = Server::new_world(1337);
        let mut map = MapState::new(1337);

        // A 64x64 region around the spawn area (8, 8), before any edits.
        let r = map.top_map(8, 8, 64, 64);
        let mut mismatches = Vec::new();
        for z in 0..r.h {
            for x in 0..r.w {
                let (wx, wz) = (r.x0 + x, r.z0 + z);
                let i = (z * r.w + x) as usize * 2;
                let (y, b) = (r.cols[i] as i32, Block::from_u8(r.cols[i + 1]));
                let (wy, wb) = match world_column_top(&mut server, wx, wz) {
                    Some((y, b)) => (y, b),
                    None => panic!("column ({wx},{wz}) is entirely air"),
                };
                if y == wy && b == wb {
                    continue; // exact match
                }
                // Documented approximations (see above): flowers, and the
                // per-column tree approximation when the world's topmost
                // block is tree foliage.
                let flower = matches!(wb, Block::FlowerRed | Block::FlowerYellow)
                    && y == wy - 1;
                let canopy = matches!(wb, Block::Leaves | Block::Log) && y <= wy + 1;
                if !(flower || canopy) {
                    mismatches.push(format!("({wx},{wz}): map=({y},{b:?}) world=({wy},{wb:?})"));
                }
            }
        }
        assert!(
            mismatches.is_empty(),
            "{} unexpected mismatches:\n{}",
            mismatches.len(),
            mismatches.join("\n")
        );
    }

    #[test]
    fn map_reflects_edits() {
        let mut server = Server::new_world(1337);
        let mut map = MapState::new(1337);

        // Find a dry grass column near spawn and break its surface block.
        let mut spot = None;
        for dz in 0..8 {
            for dx in 0..8 {
                let (x, z) = (8 + dx, 8 + dz);
                let h = server.world().height_at(x, z);
                if h >= SEA_LEVEL && h < SNOW_LEVEL && server.world().tree_at(x, z).is_none() {
                    spot = Some((x, z, h));
                    break;
                }
            }
            if spot.is_some() {
                break;
            }
        }
        let (x, z, h) = spot.expect("a grass column near spawn");

        let before = map.top_map(x, z, 32, 32);
        let i = ((z - before.z0) * before.w + (x - before.x0)) as usize * 2;
        assert_eq!(before.cols[i + 1], Block::Grass.as_u8(), "baseline is grass");

        // Break the surface: the map (after syncing the edit) must show the
        // block below (dirt) at h-1.
        let dirty = server.world_mut().set_block(BlockPos::new(x, h, z), Block::Air);
        let _ = dirty;
        map.sync_edits(server.world().edits());
        let after = map.top_map(x, z, 32, 32);
        let i = ((z - after.z0) * after.w + (x - after.x0)) as usize * 2;
        assert_eq!(after.cols[i], (h - 1) as u8, "top should fall to the sub-surface");
        assert_eq!(after.cols[i + 1], Block::Dirt.as_u8(), "sub-surface is dirt");

        // Place a 3-block tower (h+1..=h+3, h is now air): the map's top
        // must climb to h+3 (stone).
        for dy in 0..3 {
            server.world_mut().set_block(BlockPos::new(x, h + 1 + dy, z), Block::Stone);
        }
        map.sync_edits(server.world().edits());
        let after2 = map.top_map(x, z, 32, 32);
        let i = ((z - after2.z0) * after2.w + (x - after2.x0)) as usize * 2;
        assert_eq!(after2.cols[i], (h + 3) as u8, "tower top at h+3");
        assert_eq!(after2.cols[i + 1], Block::Stone.as_u8(), "tower is stone");

        // Break the tower's top block again: back to h+2.
        server.world_mut().set_block(BlockPos::new(x, h + 3, z), Block::Air);
        map.sync_edits(server.world().edits());
        let after3 = map.top_map(x, z, 32, 32);
        let i = ((z - after3.z0) * after3.w + (x - after3.x0)) as usize * 2;
        assert_eq!(after3.cols[i], (h + 2) as u8, "top back to h+2");
    }

    #[test]
    fn map_clamps_and_layout() {
        let mut map = MapState::new(1337);
        // Oversized requests clamp to the max (and the layout stays
        // row-major: len = 2 * w * h).
        let r = map.top_map(8, 8, 4096, 4096);
        assert_eq!(r.w, MAP_MAX);
        assert_eq!(r.h, MAP_MAX);
        assert_eq!(r.cols.len(), 2 * (MAP_MAX as usize) * (MAP_MAX as usize));
        assert_eq!(r.x0, 8 - MAP_MAX / 2);
        // Tiny requests clamp up (a 1x1 map is useless).
        let r = map.top_map(8, 8, 1, 1);
        assert_eq!(r.w, MAP_MIN);
        assert_eq!(r.h, MAP_MIN);
        // Every column has a block (y=0 bedrock is unbreakable).
        let mut i = 0;
        while i < r.cols.len() {
            assert!(r.cols[i] < 255, "no unknown columns");
            i += 2;
        }
        // Deterministic: two states with the same seed agree.
        let mut map2 = MapState::new(1337);
        assert_eq!(map.top_map(100, -50, 64, 64).cols, map2.top_map(100, -50, 64, 64).cols);
    }

    #[test]
    fn map_underwater_columns_show_waterline() {
        let server = Server::new_world(1337);
        let mut map = MapState::new(1337);
        // Scan a wide area for a below-sea-level column.
        let mut wet = None;
        'outer: for dz in -128..128 {
            for dx in -128..128 {
                let (x, z) = (dx, dz);
                if server.world().height_at(x, z) < SEA_LEVEL {
                    wet = Some((x, z));
                    break 'outer;
                }
            }
        }
        let (x, z) = wet.expect("seed 1337 has lakes within 256 blocks of origin");
        let r = map.top_map(x, z, 32, 32);
        let i = ((z - r.z0) * r.w + (x - r.x0)) as usize * 2;
        assert_eq!(r.cols[i], SEA_LEVEL as u8, "top of a lake column is the waterline");
        assert_eq!(r.cols[i + 1], Block::Water.as_u8());
    }
}
