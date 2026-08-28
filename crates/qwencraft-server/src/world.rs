//! The world: lazily generated chunk buffers + a delta layer for edits.
//!
//! Memory strategy: only chunks that are actually needed (streamed to a
//! client or probed by an agent) are materialised. Edits to ungenerated
//! chunks are stored as deltas and applied when the chunk is generated.

use std::collections::HashMap;

use qwencraft_world::{
    Block, BlockPos, ChunkPos, Tree, CHUNK, CHUNK_BLOCKS, REGION, REGION_BLOCKS, REGION_MARGIN,
    WORLD_HEIGHT, chunk_index, region_index,
};

use qwencraft_world::WorldGen;

use crate::Vec3;

/// World update sent to a client.
#[derive(Clone, Debug)]
pub enum WorldUpdate {
    /// A chunk region (16^3 core + 5-block border) that (re)appeared.
    Chunk { pos: ChunkPos, data: Vec<u8> },
}

/// A single block edit.
#[derive(Clone, Copy, Debug)]
pub struct Edit {
    pub pos: BlockPos,
    pub block: Block,
}

pub struct World {
    gen: WorldGen,
    /// Materialised chunk buffers.
    chunks: HashMap<ChunkPos, Vec<u8>>,
    /// Pending edits for chunks that are not materialised yet, and the full
    /// edit history (used to keep resends correct after edits).
    deltas: HashMap<ChunkPos, HashMap<BlockPos, Block>>,
    edits: Vec<Edit>,
    /// Column-height cache: tree placement scans a 1-chunk halo around each
    /// chunk, and neighbouring chunks share most of those heights. Caching
    /// them makes steady-state generation nearly as cheap as the plain
    /// heightmap fill (heights are immutable — terrain never moves).
    heights: HashMap<(i32, i32), i32>,
}

impl World {
    pub fn new(seed: u64) -> Self {
        Self {
            gen: WorldGen::new(seed),
            chunks: HashMap::new(),
            deltas: HashMap::new(),
            edits: Vec::new(),
            heights: HashMap::new(),
        }
    }

    /// The tree rooted at column (x, z), if any (for spawn placement).
    pub fn tree_at(&self, x: i32, z: i32) -> Option<Tree> {
        self.gen.tree_at(x, z)
    }

    /// Terrain surface height (topmost solid Y) for a column.
    pub fn height_at(&self, x: i32, z: i32) -> i32 {
        self.gen.height(x, z)
    }

    pub fn contains(&self, c: &ChunkPos) -> bool {
        self.chunks.contains_key(c)
    }

    pub fn chunks_generated(&self) -> usize {
        self.chunks.len()
    }

    pub fn delta_count(&self) -> usize {
        self.deltas.values().map(|d| d.len()).sum()
    }

    pub fn edits(&self) -> &[Edit] {
        &self.edits
    }

    /// Materialise a chunk (generating terrain and applying pending deltas).
    pub fn generate(&mut self, c: ChunkPos) {
        if self.chunks.contains_key(&c) || c.guaranteed_air() {
            return;
        }
        if c.y * CHUNK < 0 || c.y * CHUNK + (CHUNK - 1) >= WORLD_HEIGHT {
            // Outside the world's Y range: an all-air chunk.
            self.chunks.insert(c, vec![0u8; CHUNK_BLOCKS]);
            return;
        }
        let mut data = self.gen.generate_chunk_cached(c.x, c.y, c.z, &mut self.heights).to_vec();
        // Apply pending deltas for this chunk.
        if let Some(d) = self.deltas.get(&c) {
            for (local, block) in d {
                if local.x >= 0
                    && local.x < CHUNK
                    && local.y >= 0
                    && local.y < CHUNK
                    && local.z >= 0
                    && local.z < CHUNK
                {
                    data[chunk_index(*local)] = block.as_u8();
                }
            }
        }
        self.chunks.insert(c, data);
    }

    /// Read a block, materialising its chunk on demand.
    pub fn block_at(&mut self, pos: BlockPos) -> Block {
        if !pos.in_world_y() {
            return Block::Air;
        }
        let c = ChunkPos::of(pos);
        if c.guaranteed_air() {
            return Block::Air;
        }
        self.generate(c);
        let local = pos.local();
        let data = self.chunks.get(&c).expect("chunk just generated");
        Block::from_u8(data[chunk_index(local)])
    }

    /// Read a block without generating anything (None when unknown).
    pub fn block_at_opt(&self, pos: BlockPos) -> Option<Block> {
        if !pos.in_world_y() {
            return Some(Block::Air);
        }
        let c = ChunkPos::of(pos);
        let local = pos.local();
        // Deltas win over generated data.
        if let Some(d) = self.deltas.get(&c) {
            if let Some(b) = d.get(&local) {
                return Some(*b);
            }
        }
        let data = self.chunks.get(&c)?;
        Some(Block::from_u8(data[chunk_index(local)]))
    }

    /// Write a block. Returns the chunks whose streamed regions changed
    /// (the edited chunk plus face-adjacent chunks when the edit is within
    /// one block of a border).
    pub fn set_block(&mut self, pos: BlockPos, block: Block) -> Vec<ChunkPos> {
        if !pos.in_world_y() {
            return Vec::new();
        }
        self.edits.push(Edit { pos, block });
        let c = ChunkPos::of(pos);
        let local = pos.local();
        // Edits on a chunk face also change the streamed region of the
        // neighbouring chunk (its 5-block border covers this one).
        let near_border = local.x == 0
            || local.x == CHUNK - 1
            || local.z == 0
            || local.z == CHUNK - 1
            || local.y == 0
            || local.y == CHUNK - 1;

        if let Some(data) = self.chunks.get_mut(&c) {
            data[chunk_index(local)] = block.as_u8();
            if let Some(d) = self.deltas.get_mut(&c) {
                d.remove(&local);
            }
        } else {
            self.deltas
                .entry(c)
                .or_default()
                .insert(local, block);
        }

        let mut dirty = vec![c];
        if near_border {
            for d in [
                BlockPos::new(1, 0, 0),
                BlockPos::new(-1, 0, 0),
                BlockPos::new(0, 0, 1),
                BlockPos::new(0, 0, -1),
                BlockPos::new(0, 1, 0),
                BlockPos::new(0, -1, 0),
            ] {
                let n = ChunkPos::new(c.x + d.x, c.y + d.y, c.z + d.z);
                if n.y >= 0 {
                    dirty.push(n);
                }
            }
        }
        dirty
    }

    /// Sample the 26^3 region payload for a chunk. Neighbouring chunks are
    /// expected to be generated (see `Server::region_ready`); unknown blocks
    /// sample as air so this never panics.
    ///
    /// Implemented as bulk row copies from the 3x3x3 chunk buffers (with
    /// per-chunk delta overrides) instead of 17k individual lookups — this
    /// runs in a few microseconds instead of several milliseconds.
    pub fn region(&self, c: ChunkPos) -> Vec<u8> {
        let mut out = vec![0u8; REGION_BLOCKS];
        let origin = c.origin();
        let rx0 = origin.x - REGION_MARGIN;
        let ry0 = origin.y - REGION_MARGIN;
        let rz0 = origin.z - REGION_MARGIN;
        for dy in -1i32..=1 {
            for dz in -1i32..=1 {
                for dx in -1i32..=1 {
                    let nc = ChunkPos::new(c.x + dx, c.y + dy, c.z + dz);
                    let data = match self.chunks.get(&nc) {
                        Some(d) => d,
                        None => continue, // unknown chunk samples as air
                    };
                    let nx0 = nc.x * CHUNK;
                    let ny0 = nc.y * CHUNK;
                    let nz0 = nc.z * CHUNK;
                    // Region-local box of this chunk's overlap with the
                    // region volume [r0, r0 + 26) on each axis.
                    let x0 = nx0.max(rx0) - rx0;
                    let x1 = (nx0 + CHUNK).min(rx0 + REGION) - rx0;
                    let y0 = ny0.max(ry0) - ry0;
                    let y1 = (ny0 + CHUNK).min(ry0 + REGION) - ry0;
                    let z0 = nz0.max(rz0) - rz0;
                    let z1 = (nz0 + CHUNK).min(rz0 + REGION) - rz0;
                    if x0 >= x1 || y0 >= y1 || z0 >= z1 {
                        continue;
                    }
                    // Chunk-local box of the same overlap.
                    let cx0 = nx0.max(rx0) - nx0;
                    let cy0 = ny0.max(ry0) - ny0;
                    let cz0 = nz0.max(rz0) - nz0;
                    // Both layouts store x fastest, so each (cy, cz) row is
                    // one contiguous copy.
                    for ly in 0..(y1 - y0) {
                        let cy = cy0 + ly;
                        for lz in 0..(z1 - z0) {
                            let cz = cz0 + lz;
                            let src = (cy as usize * CHUNK as usize + cz as usize) * CHUNK as usize
                                + cx0 as usize;
                            let dst = ((y0 + ly) as usize * REGION as usize + (z0 + lz) as usize)
                                * REGION as usize
                                + x0 as usize;
                            let len = (x1 - x0) as usize;
                            out[dst..dst + len].copy_from_slice(&data[src..src + len]);
                        }
                    }
                    // Pending edits for this chunk override the copy.
                    if let Some(deltas) = self.deltas.get(&nc) {
                        for (local, block) in deltas {
                            let lx = local.x + (nx0 - rx0);
                            let ly = local.y + (ny0 - ry0);
                            let lz = local.z + (nz0 - rz0);
                            if (0..REGION).contains(&lx)
                                && (0..REGION).contains(&ly)
                                && (0..REGION).contains(&lz)
                            {
                                out[region_index(BlockPos::new(lx, ly, lz))] = block.as_u8();
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// DDA voxel raycast. Returns (hit block, previous air block) or None.
    pub fn raycast(&mut self, origin: &Vec3, dir: &Vec3, max_dist: f32) -> Option<(BlockPos, BlockPos)> {
        let d = dir.normalize();
        if d.length() < 1e-6 {
            return None;
        }

        // Floor the eye into its containing cell. Note: NOT `as i32`, which
        // truncates toward zero — for negative coordinates that lands the start
        // cell one too far toward zero, so a ray looking along +/-Z (d.x ~ 0,
        // where X never steps) would report a hit one block +X off. That was the
        // intermittent crosshair offset.
        let mut pos = BlockPos::new(
            origin.x.floor() as i32,
            origin.y.floor() as i32,
            origin.z.floor() as i32,
        );
        // If we start inside a solid block, that is the hit (no prev).
        if self.block_at(pos).is_solid() {
            return Some((pos, pos));
        }

        let step_x = if d.x > 0.0 { 1 } else { -1 };
        let step_y = if d.y > 0.0 { 1 } else { -1 };
        let step_z = if d.z > 0.0 { 1 } else { -1 };

        let t_delta_x = if d.x != 0.0 { 1.0 / d.x.abs() } else { f32::INFINITY };
        let t_delta_y = if d.y != 0.0 { 1.0 / d.y.abs() } else { f32::INFINITY };
        let t_delta_z = if d.z != 0.0 { 1.0 / d.z.abs() } else { f32::INFINITY };

        let frac_x = (origin.x - pos.x as f32).abs();
        let frac_y = (origin.y - pos.y as f32).abs();
        let frac_z = (origin.z - pos.z as f32).abs();
        // Distance to the next boundary along each axis.
        let mut t_max_x = if d.x > 0.0 {
            (1.0 - frac_x) / d.x.max(1e-9)
        } else if d.x < 0.0 {
            frac_x / d.x.abs().max(1e-9)
        } else {
            f32::INFINITY
        };
        let mut t_max_y = if d.y > 0.0 {
            (1.0 - frac_y) / d.y.max(1e-9)
        } else if d.y < 0.0 {
            frac_y / d.y.abs().max(1e-9)
        } else {
            f32::INFINITY
        };
        let mut t_max_z = if d.z > 0.0 {
            (1.0 - frac_z) / d.z.max(1e-9)
        } else if d.z < 0.0 {
            frac_z / d.z.abs().max(1e-9)
        } else {
            f32::INFINITY
        };

        let mut t = 0.0f32;
        while t <= max_dist {
            // Advance to the nearest next boundary. 0=x,1=y,2=z.
            let axis = if t_max_x <= t_max_y && t_max_x <= t_max_z {
                t = t_max_x;
                t_max_x += t_delta_x;
                0
            } else if t_max_y <= t_max_z {
                t = t_max_y;
                t_max_y += t_delta_y;
                1
            } else {
                t = t_max_z;
                t_max_z += t_delta_z;
                2
            };
            if t > max_dist {
                return None;
            }
            let prev = pos;
            match axis {
                0 => pos.x += step_x,
                1 => pos.y += step_y,
                _ => pos.z += step_z,
            }
            if self.block_at(pos).is_solid() {
                return Some((pos, prev));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qwencraft_world::{Block, BlockPos, ChunkPos};

    /// The crosshair raycast must floor the eye into its containing cell,
    /// not truncate toward zero. For a negative eye coordinate this used to
    /// start the DDA one cell too far toward zero, so a ray looking along
    /// +/-Z (d.x ~ 0, so X never steps) reported the hit one block +X off.
    /// Regression test: eye at x=-2.5 looking -Z at a wall in the XZ plane
    /// must hit x=-3 (floor), never x=-2 (truncate).
    #[test]
    fn raycast_floors_eye_cell_not_truncates() {
        let mut w = World::new(1337);
        // Build the scenario entirely with edits; block_at generates each
        // chunk on demand and applies pending deltas, so no explicit generate
        // is needed.
        // A wall in the XZ plane at z=-6 (solid), in front of the eye.
        for x in -8..=2 {
            for y in 0..=15 {
                w.set_block(BlockPos::new(x, y, -6), Block::Stone);
            }
        }
        // Air along the eye row (y=10, z=-5..-1) so the ray only stops at the
        // wall, leaving the z=-6 wall intact.
        for x in -8..=2 {
            for z in -5..=-1 {
                w.set_block(BlockPos::new(x, 10, z), Block::Air);
            }
        }
        // Eye in air at negative x, looking -Z (yaw=0 => dir (0,0,-1)).
        let eye = Vec3::new(-2.5, 10.0, -1.0);
        let dir = Vec3::new(0.0, 0.0, -1.0);
        let (hit, _prev) = w.raycast(&eye, &dir, 6.0).expect("should hit the wall");
        assert_eq!((hit.x, hit.y, hit.z), (-3, 10, -6), "eye cell must be floored, not truncated");
    }

    /// Reference implementation: per-cell sampling (what `region` used to do).
    fn region_reference(world: &World, c: ChunkPos) -> Vec<u8> {
        let mut out = vec![0u8; REGION_BLOCKS];
        let origin = c.origin();
        let m = REGION_MARGIN;
        for y in 0..REGION {
            for z in 0..REGION {
                for x in 0..REGION {
                    let wp = BlockPos::new(origin.x - m + x, origin.y - m + y, origin.z - m + z);
                    if let Some(b) = world.block_at_opt(wp) {
                        out[region_index(BlockPos::new(x, y, z))] = b.as_u8();
                    }
                }
            }
        }
        out
    }

    #[test]
    fn region_matches_per_cell_reference() {
        let mut world = World::new(1337);
        // Materialise a 5x5x3 chunk block around the origin so all 3x3x3
        // neighbourhoods are complete.
        for dx in -2..=2 {
            for dz in -2..=2 {
                for cy in 0..3 {
                    world.generate(ChunkPos::new(dx, cy, dz));
                }
            }
        }
        for dx in -1..=1 {
            for dz in -1..=1 {
                for cy in 0..2 {
                    let c = ChunkPos::new(dx, cy, dz);
                    assert_eq!(
                        world.region(c),
                        region_reference(&world, c),
                        "region mismatch at {c:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn region_includes_pending_edits() {
        let mut world = World::new(1337);
        let c = ChunkPos::new(0, 1, 0);
        world.generate(c);
        for dx in -1..=1 {
            for dz in -1..=1 {
                for cy in 0..3 {
                    world.generate(ChunkPos::new(dx, cy, dz));
                }
            }
        }
        // Edit a block in the chunk's core (visible in the chunk's own region).
        let target = BlockPos::new(8, 20, 8);
        world.set_block(target, Block::Sand);
        let r = world.region(c);
        let local = qwencraft_world::BlockPos::new(
            8 - (c.x * CHUNK - REGION_MARGIN),
            20 - (c.y * CHUNK - REGION_MARGIN),
            8 - (c.z * CHUNK - REGION_MARGIN),
        );
        assert_eq!(r[region_index(local)], Block::Sand.as_u8());
        // And it matches the reference (which also sees the delta).
        assert_eq!(r, region_reference(&world, c));
        // Edit a block in a neighbour chunk's border area: it must appear in
        // this chunk's region (the 5-block border overlaps it).
        let ntarget = BlockPos::new(16, 20, 8); // in chunk +X
        world.set_block(ntarget, Block::Stone);
        let r2 = world.region(c);
        let nlocal = qwencraft_world::BlockPos::new(
            16 - (c.x * CHUNK - REGION_MARGIN),
            20 - (c.y * CHUNK - REGION_MARGIN),
            8 - (c.z * CHUNK - REGION_MARGIN),
        );
        assert_eq!(r2[region_index(nlocal)], Block::Stone.as_u8());
        assert_eq!(r2, region_reference(&world, c));
    }
}
