//! Terrain generation: heightmap + caves, turned into per-chunk block data.

use crate::block::Block;
use crate::noise::Noise;
use crate::{CHUNK, CHUNK_BLOCKS, TERRAIN_MAX, TERRAIN_MIN, WORLD_HEIGHT, chunk_index};

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

    /// Single block query (independent of chunk boundaries).
    pub fn block_at(&self, x: i32, y: i32, z: i32) -> Block {
        if y < 0 || y >= WORLD_HEIGHT {
            return Block::Air;
        }
        let surface = self.height(x, z);
        if y > surface {
            return Block::Air;
        }
        if self.is_cave(x, y, z, surface) {
            return Block::Air;
        }
        if y == 0 {
            return Block::Stone;
        }
        if y == surface {
            // Sandy lowlands, grassland elsewhere.
            return if surface <= 18 { Block::Sand } else { Block::Grass };
        }
        if y >= surface - 3 {
            return if surface <= 18 { Block::Sand } else { Block::Dirt };
        }
        Block::Stone
    }

    /// Generate the block data for one chunk. Layout: `chunk_index` order.
    pub fn generate_chunk(&self, cx: i32, cy: i32, cz: i32) -> [u8; CHUNK_BLOCKS] {
        let mut out = [0u8; CHUNK_BLOCKS];
        let ox = cx * CHUNK;
        let oy = cy * CHUNK;
        let oz = cz * CHUNK;

        // Cache per-column heights for the 16x16 footprint.
        let mut heights = [[0i32; CHUNK as usize]; CHUNK as usize];
        for z in 0..CHUNK {
            for x in 0..CHUNK {
                heights[z as usize][x as usize] = self.height(ox + x, oz + z);
            }
        }

        for y in 0..CHUNK {
            let wy = oy + y;
            if wy < 0 || wy >= WORLD_HEIGHT {
                continue; // outside world bounds stays air
            }
            for z in 0..CHUNK {
                for x in 0..CHUNK {
                    let surface = heights[z as usize][x as usize];
                    let b = if wy > surface {
                        Block::Air
                    } else if self.is_cave(ox + x, wy, oz + z, surface) {
                        Block::Air
                    } else if wy == 0 {
                        Block::Stone
                    } else if wy == surface {
                        if surface <= 18 {
                            Block::Sand
                        } else {
                            Block::Grass
                        }
                    } else if wy >= surface - 3 {
                        if surface <= 18 {
                            Block::Sand
                        } else {
                            Block::Dirt
                        }
                    } else {
                        Block::Stone
                    };
                    out[chunk_index(crate::BlockPos::new(x, y, z))] = b.as_u8();
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
}
