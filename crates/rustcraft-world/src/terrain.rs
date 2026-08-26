//! Terrain generation: heightmap + caves + water + trees + flowers, turned
//! into per-chunk block data.
//!
//! Everything is a *pure function of world coordinates* (plus the seed), so
//! chunks agree perfectly across boundaries — a tree whose canopy overhangs
//! a chunk border is stamped identically by every chunk that contains part
//! of it (enforced by `chunk_matches_block_at` in the tests).

use std::collections::HashMap;

use crate::block::Block;
use crate::noise::Noise;
use crate::{CHUNK, CHUNK_BLOCKS, TERRAIN_MAX, TERRAIN_MIN, WORLD_HEIGHT, chunk_index};

/// Y of the topmost water block; the water surface is at `SEA_LEVEL + 1`.
pub const SEA_LEVEL: i32 = 21;
/// Columns at or above this height are capped with snow.
pub const SNOW_LEVEL: i32 = 33;
/// Fraction of eligible (grassy, flat) columns that carry a tree.
const TREE_DENSITY: f32 = 0.012;
/// Fraction of eligible columns with a red flower.
const FLOWER_RED: f32 = 0.015;
/// Cumulative fraction of eligible columns with a flower (red + yellow).
const FLOWER_BOTH: f32 = 0.03;

/// A deterministic tree rooted at column `(x, z)` on top of surface `base`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tree {
    pub x: i32,
    pub z: i32,
    /// Topmost solid block of the root column.
    pub base: i32,
    /// Trunk height (4..=6); the trunk occupies `base+1 ..= base+trunk`.
    pub trunk: i32,
}

/// Deterministic world generator.
#[derive(Clone, Copy, Debug)]
pub struct WorldGen {
    pub seed: u64,
}

impl WorldGen {
    pub fn new(seed: u64) -> Self {
        Self { seed }
    }

    fn noise(&self) -> Noise {
        Noise::new(self.seed)
    }

    /// Terrain surface height (topmost solid block Y) for a column.
    pub fn height(&self, x: i32, z: i32) -> i32 {
        let n = self.noise();
        // Broad rolling hills + medium detail + fine ripple.
        let hills = n.fbm2(x as f32 * 0.004, z as f32 * 0.004, 4);
        let detail = n.fbm2(x as f32 * 0.02 + 100.0, z as f32 * 0.02 - 50.0, 3);
        let h = 26.0 + hills * 15.0 + detail * 4.5;
        (h as i32).clamp(TERRAIN_MIN, TERRAIN_MAX)
    }

    /// True when a block at (x, y, z) is carved out by caves.
    fn is_cave(&self, x: i32, y: i32, z: i32, surface: i32) -> bool {
        if y <= 0 || y >= surface - 1 {
            return false; // keep bedrock floor and the surface intact
        }
        let n = self.noise();
        // Two overlapping 3D fields: one for long worm-like tunnels, one for
        // open cavern pockets.
        let tunnels = n.fbm3(x as f32 * 0.045, y as f32 * 0.06, z as f32 * 0.045, 2);
        if tunnels.abs() < 0.055 {
            return true;
        }
        let cavern = n.fbm3(x as f32 * 0.018 + 500.0, y as f32 * 0.028, z as f32 * 0.018 - 300.0, 3);
        cavern > 0.62
    }

    /// Cheap, well-distributed deterministic hash of a column -> [0, 1).
    pub fn column_hash(&self, x: i32, z: i32, salt: u64) -> f32 {
        let mut h = self.seed ^ salt.wrapping_mul(0x9E3779B97F4A7C15);
        h ^= (x as u64).rotate_left(13).wrapping_mul(0xC2B2AE3D27D4EB4F);
        h = (h ^ (h >> 31)).wrapping_mul(0x9E3779B97F4A7C15);
        h ^= (z as u64).rotate_left(29).wrapping_mul(0x165667B19E3779F9);
        h = (h ^ (h >> 27)).wrapping_mul(0xC2B2AE3D27D4EB4F);
        h ^= h >> 31;
        (h % 1_000_003) as f32 / 1_000_003.0
    }

    /// 3D cell hash (y folded into x/z with large primes) -> [0, 1).
    fn cell_hash(&self, x: i32, y: i32, z: i32, salt: u64) -> f32 {
        self.column_hash(
            x.wrapping_add(y.wrapping_mul(7919)),
            z.wrapping_add(y.wrapping_mul(104_729)),
            salt,
        )
    }

    /// Surface block of a column.
    fn top_block(&self, surface: i32) -> Block {
        if surface < SEA_LEVEL {
            Block::Sand // lakebed / underwater shore
        } else if surface >= SNOW_LEVEL {
            Block::SnowGrass
        } else {
            Block::Grass
        }
    }

    /// Block just below the surface.
    fn sub_top_block(&self, surface: i32) -> Block {
        if surface < SEA_LEVEL {
            Block::Sand
        } else {
            Block::Dirt
        }
    }

    /// The tree rooted at column (x, z), if any. Trees grow on flat grass
    /// (no beaches, no snow, no cliffs), one in ~80 eligible columns.
    pub fn tree_at(&self, x: i32, z: i32) -> Option<Tree> {
        let h = self.height(x, z);
        self.tree_at_h(x, z, h, |x, z| self.height(x, z))
    }

    /// `tree_at` with a height oracle (used to amortise noise calls when
    /// scanning a chunk's halo, where heights are already known/cached).
    fn tree_at_h(
        &self,
        x: i32,
        z: i32,
        h: i32,
        mut heights: impl FnMut(i32, i32) -> i32,
    ) -> Option<Tree> {
        if h < SEA_LEVEL || h >= SNOW_LEVEL {
            return None;
        }
        // Gentle slope only: all four neighbours within 1 block.
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            if heights(x + dx, z + dz).abs_diff(h) > 1 {
                return None;
            }
        }
        if self.column_hash(x, z, 1) >= TREE_DENSITY {
            return None;
        }
        let trunk = 4 + (self.column_hash(x, z, 2) * 3.0) as i32; // 4..=6
        Some(Tree { x, z, base: h, trunk })
    }

    /// The block `t` occupies at world cell (x, y, z), if any. Trees only
    /// claim air; callers must not overwrite terrain with this.
    fn tree_block(&self, t: &Tree, x: i32, y: i32, z: i32) -> Option<Block> {
        let dx = x - t.x;
        let dz = z - t.z;
        if dx.abs() > 2 || dz.abs() > 2 {
            return None;
        }
        let dy = y - t.base; // 1 = just above the ground
        if dx == 0 && dz == 0 && (1..=t.trunk).contains(&dy) {
            return Some(Block::Log);
        }
        match dy {
            // 5x5 layer at the trunk top, organic corner cutouts.
            d if d == t.trunk => {
                if dx.abs() == 2 && dz.abs() == 2 && self.cell_hash(x, y, z, 3) < 0.5 {
                    return None;
                }
                Some(Block::Leaves)
            }
            // 3x3 cap without the centre (the trunk ends one below).
            d if d == t.trunk + 1 => {
                if dx.abs() <= 1 && dz.abs() <= 1 && !(dx == 0 && dz == 0) {
                    Some(Block::Leaves)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Flower on top of column (x, z), if any (a passable ground decal).
    fn flower_at_h(&self, x: i32, z: i32, surface: i32, has_tree: bool) -> Option<Block> {
        if surface < SEA_LEVEL || surface >= SNOW_LEVEL || has_tree {
            return None;
        }
        let r = self.column_hash(x, z, 10);
        if r < FLOWER_RED {
            Some(Block::FlowerRed)
        } else if r < FLOWER_BOTH {
            Some(Block::FlowerYellow)
        } else {
            None
        }
    }

    fn flower_at(&self, x: i32, z: i32, surface: i32) -> Option<Block> {
        self.flower_at_h(x, z, surface, self.tree_at(x, z).is_some())
    }

    /// Single block query (independent of chunk boundaries).
    pub fn block_at(&self, x: i32, y: i32, z: i32) -> Block {
        if y < 0 || y >= WORLD_HEIGHT {
            return Block::Air;
        }
        let surface = self.height(x, z);
        if y > surface {
            // Water fills low-lying columns up to the sea level.
            if y <= SEA_LEVEL {
                return Block::Water;
            }
            // Above the waterline: tree blocks (trunk/canopy from roots in
            // the 5x5 neighbourhood), then flowers, then air.
            for dz in -2..=2 {
                for dx in -2..=2 {
                    if let Some(t) = self.tree_at(x - dx, z - dz) {
                        if let Some(b) = self.tree_block(&t, x, y, z) {
                            return b;
                        }
                    }
                }
            }
            if y == surface + 1 {
                if let Some(f) = self.flower_at(x, z, surface) {
                    return f;
                }
            }
            return Block::Air;
        }
        if self.is_cave(x, y, z, surface) {
            return Block::Air;
        }
        if y == 0 {
            return Block::Stone;
        }
        if y == surface {
            return self.top_block(surface);
        }
        if y >= surface - 3 {
            return self.sub_top_block(surface);
        }
        Block::Stone
    }

    /// Generate the block data for one chunk. Layout: `chunk_index` order.
    pub fn generate_chunk(&self, cx: i32, cy: i32, cz: i32) -> [u8; CHUNK_BLOCKS] {
        self.generate_chunk_cached(cx, cy, cz, &mut HashMap::new())
    }

    /// `generate_chunk` with a shared column-height cache: neighbouring
    /// chunks overlap in the 1-chunk halo that tree placement scans, so a
    /// persistent cache (kept by the server's `World`) makes steady-state
    /// generation nearly as cheap as the plain heightmap fill.
    pub fn generate_chunk_cached(
        &self,
        cx: i32,
        cy: i32,
        cz: i32,
        heights: &mut HashMap<(i32, i32), i32>,
    ) -> [u8; CHUNK_BLOCKS] {
        let mut out = [0u8; CHUNK_BLOCKS];
        let ox = cx * CHUNK;
        let oy = cy * CHUNK;
        let oz = cz * CHUNK;

        let mut h_of = |x: i32, z: i32| -> i32 {
            *heights.entry((x, z)).or_insert_with(|| self.height(x, z))
        };

        // Cache per-column heights for the 16x16 footprint.
        let mut heights_local = [[0i32; CHUNK as usize]; CHUNK as usize];
        for z in 0..CHUNK {
            for x in 0..CHUNK {
                heights_local[z as usize][x as usize] = h_of(ox + x, oz + z);
            }
        }

        for y in 0..CHUNK {
            let wy = oy + y;
            if wy < 0 || wy >= WORLD_HEIGHT {
                continue; // outside world bounds stays air
            }
            for z in 0..CHUNK {
                for x in 0..CHUNK {
                    let surface = heights_local[z as usize][x as usize];
                    let b = if wy > surface {
                        // Water fills low-lying columns to the sea level.
                        if wy <= SEA_LEVEL {
                            Block::Water
                        } else {
                            Block::Air
                        }
                    } else if self.is_cave(ox + x, wy, oz + z, surface) {
                        Block::Air
                    } else if wy == 0 {
                        Block::Stone
                    } else if wy == surface {
                        self.top_block(surface)
                    } else if wy >= surface - 3 {
                        self.sub_top_block(surface)
                    } else {
                        Block::Stone
                    };
                    out[chunk_index(crate::BlockPos::new(x, y, z))] = b.as_u8();
                }
            }
        }

        // Trees: a canopy overhangs up to 2 blocks past the root column, so
        // scan the 24x24 halo around the chunk. Stamping is idempotent
        // across chunks — each chunk writes the same overlapping cells.
        let mut trees: Vec<Tree> = Vec::new();
        for z in (oz - 2)..(oz + CHUNK + 2) {
            for x in (ox - 2)..(ox + CHUNK + 2) {
                let h = *heights.entry((x, z)).or_insert_with(|| self.height(x, z));
                if let Some(t) = self.tree_at_h(x, z, h, |nx, nz| {
                    *heights.entry((nx, nz)).or_insert_with(|| self.height(nx, nz))
                }) {
                    trees.push(t);
                }
            }
        }
        for t in &trees {
            for dy in 1..=t.trunk + 1 {
                for dz in -2..=2 {
                    for dx in -2..=2 {
                        let lx = t.x + dx - ox;
                        let lz = t.z + dz - oz;
                        if lx < 0 || lx >= CHUNK || lz < 0 || lz >= CHUNK {
                            continue;
                        }
                        let ly = t.base + dy - oy;
                        if ly < 0 || ly >= CHUNK {
                            continue;
                        }
                        if let Some(b) = self.tree_block(t, t.x + dx, t.base + dy, t.z + dz) {
                            let i = chunk_index(crate::BlockPos::new(lx, ly, lz));
                            if out[i] == 0 {
                                out[i] = b.as_u8();
                            }
                        }
                    }
                }
            }
        }

        // Flowers: a passable decal on top of the grass column.
        for z in 0..CHUNK {
            for x in 0..CHUNK {
                let surface = heights_local[z as usize][x as usize];
                let ly = surface + 1 - oy;
                if ly < 0 || ly >= CHUNK {
                    continue;
                }
                let has_tree = trees.iter().any(|t| t.x == ox + x && t.z == oz + z);
                if let Some(f) = self.flower_at_h(ox + x, oz + z, surface, has_tree) {
                    let i = chunk_index(crate::BlockPos::new(x, ly, z));
                    if out[i] == 0 {
                        out[i] = f.as_u8();
                    }
                }
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockPos, CHUNK, WORLD_HEIGHT};

    #[test]
    fn deterministic_chunks() {
        let a = WorldGen::new(1337);
        let b = WorldGen::new(1337);
        for (cx, cz) in [(-2, -2), (0, 0), (3, -1)] {
            for cy in 0..(WORLD_HEIGHT / CHUNK) {
                assert_eq!(a.generate_chunk(cx, cy, cz), b.generate_chunk(cx, cy, cz));
            }
        }
    }

    #[test]
    fn height_in_range() {
        let g = WorldGen::new(42);
        for z in -200..200 {
            for x in -200..200 {
                let h = g.height(x, z);
                assert!((TERRAIN_MIN..=TERRAIN_MAX).contains(&h), "height {h} out of range");
            }
        }
    }

    #[test]
    fn bottom_is_solid_surface_varies() {
        let g = WorldGen::new(99);
        // The very bottom of the world is always stone (can't fall out).
        for z in 0..16 {
            for x in 0..16 {
                assert_eq!(g.block_at(x, 0, z), Block::Stone);
            }
        }
        // Some variation across a wide area.
        let mut hs = Vec::new();
        for z in (0..256).step_by(4) {
            for x in (0..256).step_by(4) {
                hs.push(g.height(x, z));
            }
        }
        let min = *hs.iter().min().unwrap();
        let max = *hs.iter().max().unwrap();
        assert!(max - min >= 8, "terrain should vary, got {min}..{max}");
    }

    #[test]
    fn caves_exist() {
        let g = WorldGen::new(2024);
        let mut caves = 0;
        for z in 0..64 {
            for x in 0..64 {
                let h = g.height(x, z);
                for y in 2..h.saturating_sub(2) {
                    if g.block_at(x, y, z) == Block::Air {
                        caves += 1;
                    }
                }
            }
        }
        assert!(caves > 50, "expected caves, found {caves}");
    }

    #[test]
    fn chunk_matches_block_at() {
        let g = WorldGen::new(7);
        let (cx, cy, cz) = (1, 0, -2);
        let chunk = g.generate_chunk(cx, cy, cz);
        for y in 0..CHUNK {
            for z in 0..CHUNK {
                for x in 0..CHUNK {
                    let local = BlockPos::new(x, y, z);
                    let pos = BlockPos::from_chunk(crate::ChunkPos::new(cx, cy, cz), local);
                    assert_eq!(
                        Block::from_u8(chunk[crate::chunk_index(local)]),
                        g.block_at(pos.x, pos.y, pos.z)
                    );
                }
            }
        }
    }

    #[test]
    fn water_fills_low_columns_to_sea_level() {
        let g = WorldGen::new(1337);
        let mut lakes = 0;
        for z in -64..64 {
            for x in -64..64 {
                let h = g.height(x, z);
                if h < SEA_LEVEL {
                    lakes += 1;
                    // Sand lakebed, water above it up to the sea level...
                    assert_eq!(g.block_at(x, h, z), Block::Sand, "lakebed at ({x},{z})");
                    assert_eq!(g.block_at(x, h + 1, z), Block::Water);
                    assert_eq!(g.block_at(x, SEA_LEVEL, z), Block::Water);
                    // ...and air above the surface.
                    assert_eq!(g.block_at(x, SEA_LEVEL + 1, z), Block::Air);
                } else {
                    // Dry columns have no water at the surface.
                    assert_ne!(g.block_at(x, h + 1, z), Block::Water, "water on dry column ({x},{z})");
                }
            }
        }
        assert!(lakes > 100, "expected a decent lake area, found {lakes} low columns");
    }

    #[test]
    fn snow_caps_high_columns() {
        let g = WorldGen::new(1337);
        let mut snowy = 0;
        for z in (-256..256).step_by(2) {
            for x in (-256..256).step_by(2) {
                let h = g.height(x, z);
                if h >= SNOW_LEVEL {
                    snowy += 1;
                    assert_eq!(g.block_at(x, h, z), Block::SnowGrass);
                } else if h >= SEA_LEVEL {
                    assert!(matches!(g.block_at(x, h, z), Block::Grass | Block::Sand));
                }
            }
        }
        assert!(snowy > 10, "expected snowy peaks, found {snowy}");
    }

    #[test]
    fn trees_grow_on_grass_and_shape_is_right() {
        let g = WorldGen::new(1337);
        let mut found = None;
        for z in -128..128 {
            for x in -128..128 {
                if let Some(t) = g.tree_at(x, z) {
                    found = Some(t);
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        let t = found.expect("expected at least one tree near the origin");
        // Trunk from base+1 to base+trunk.
        assert_eq!(g.block_at(t.x, t.base + 1, t.z), Block::Log);
        assert_eq!(g.block_at(t.x, t.base + t.trunk, t.z), Block::Log);
        // 5x5 canopy layer at the trunk top (dx=1 is never a cut corner).
        assert_eq!(g.block_at(t.x + 1, t.base + t.trunk, t.z), Block::Leaves);
        // Cap layer above, with an open centre.
        assert_eq!(g.block_at(t.x + 1, t.base + t.trunk + 1, t.z), Block::Leaves);
        assert_eq!(g.block_at(t.x, t.base + t.trunk + 1, t.z), Block::Air);
        // Nothing above the cap.
        assert_eq!(g.block_at(t.x, t.base + t.trunk + 2, t.z), Block::Air);
        // Trees only on dry, snow-free grass.
        assert!((SEA_LEVEL..SNOW_LEVEL).contains(&t.base));
    }

    #[test]
    fn trees_are_sparse() {
        let g = WorldGen::new(1337);
        let mut n = 0;
        let mut eligible = 0;
        for z in -128..128 {
            for x in -128..128 {
                let h = g.height(x, z);
                if (SEA_LEVEL..SNOW_LEVEL).contains(&h) {
                    eligible += 1;
                    if g.tree_at(x, z).is_some() {
                        n += 1;
                    }
                }
            }
        }
        let density = n as f32 / eligible as f32;
        assert!(n > 20, "expected trees, found {n}");
        assert!(density < 0.05, "trees too dense: {density}");
    }

    #[test]
    fn flowers_appear_on_grass() {
        let g = WorldGen::new(1337);
        let mut n = 0;
        for z in -128..128 {
            for x in -128..128 {
                let h = g.height(x, z);
                if (SEA_LEVEL..SNOW_LEVEL).contains(&h) && g.tree_at(x, z).is_none() {
                    let b = g.block_at(x, h + 1, z);
                    if matches!(b, Block::FlowerRed | Block::FlowerYellow) {
                        n += 1;
                        // Flowers sit on the grass and nothing above them.
                        assert_eq!(g.block_at(x, h, z), Block::Grass);
                        assert_eq!(g.block_at(x, h + 2, z), Block::Air);
                    }
                }
            }
        }
        assert!(n > 30, "expected flowers, found {n}");
    }

    #[test]
    fn tree_chunks_agree_across_boundaries() {
        // A canopy overhanging a chunk border must be stamped by the chunk
        // it overhangs into (halo scanning) while the root chunk clips it.
        let g = WorldGen::new(1337);
        // Canopy reaches t.x+2; find a tree rooted 2 blocks before a +X
        // chunk border (x = 14 mod 16, so t.x+2 lands on local x = 0).
        let mut t = None;
        for z in -96..96 {
            for x in -96..96 {
                if x % 16 == 14 {
                    if let Some(tt) = g.tree_at(x, z) {
                        t = Some(tt);
                        break;
                    }
                }
            }
            if t.is_some() {
                break;
            }
        }
        let t = t.expect("expected a tree rooted 2 blocks before a +X chunk border");
        let w = crate::BlockPos::new(t.x + 2, t.base + t.trunk, t.z);
        let c = crate::ChunkPos::of(w);
        let expect = g.block_at(w.x, w.y, w.z);
        assert_eq!(expect, Block::Leaves);
        // The chunk the canopy overhangs into contains it...
        let b = g.generate_chunk(c.x, c.y, c.z);
        assert_eq!(
            Block::from_u8(b[crate::chunk_index(w.local())]),
            expect,
            "neighbour chunk must contain the overhanging canopy"
        );
        // ...and the root-side chunk still has its own canopy cells.
        let wa = crate::BlockPos::new(t.x + 1, t.base + t.trunk, t.z);
        assert_eq!(crate::ChunkPos::of(wa).x, c.x - 1);
        let expect_a = g.block_at(wa.x, wa.y, wa.z);
        assert_eq!(expect_a, Block::Leaves);
        let a = g.generate_chunk(c.x - 1, c.y, c.z);
        assert_eq!(
            Block::from_u8(a[crate::chunk_index(wa.local())]),
            expect_a
        );
    }
}
