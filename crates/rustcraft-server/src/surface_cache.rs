//! Per-agent 3D surface cache — a "heightmap" that handles caves, bridges,
//! overhangs and tunnels.
//!
//! Agents only ever need to know the blocks *they interact with*: solid
//! blocks adjacent to air (surface blocks) within a small radius. Storing
//! just those keeps agent state tiny even though the world is infinite, and
//! avoids materialising chunk buffers far away from the agent.
//!
//! The cache is rebuilt whenever the agent's centre cell changes; lookups
//! fall back to the world (materialising a chunk on demand) so correctness
//! never depends on the cache being complete.

use std::collections::HashMap;

use rustcraft_world::BlockPos;

use crate::world::World;

/// Half-size (in blocks) of the cubic cache volume around the agent.
///
/// The physics probes only touch the agent AABB (~2x3x2 blocks), so a small
/// volume is plenty; the world fallback in `lookup` covers anything farther.
/// Keep this small: every cell change rebuilds the volume, sampling one
/// `world.block_at` per block (which can generate chunks on demand).
const RADIUS: i32 = 3;

/// 3D cache of nearby surface blocks for one agent.
pub struct SurfaceCache {
    /// Block cell containing the agent's feet (centre of the cache volume).
    center: BlockPos,
    /// Surface blocks: solid blocks with at least one air neighbour.
    blocks: HashMap<BlockPos, u8>,
    initialized: bool,
}

impl Default for SurfaceCache {
    fn default() -> Self {
        Self::new()
    }
}

impl SurfaceCache {
    pub fn new() -> Self {
        Self {
            center: BlockPos::new(0, 0, 0),
            blocks: HashMap::new(),
            initialized: false,
        }
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    pub fn center(&self) -> BlockPos {
        self.center
    }

    /// Rebuild the cache if the agent moved to a new centre cell.
    ///
    /// A 17x17x17 volume is sampled; each solid block with an air neighbour
    /// is recorded. Chunks are materialised on demand (this is the only
    /// world access agents need for physics).
    pub fn update(&mut self, world: &mut World, feet: crate::Vec3) {
        let center = BlockPos::new(
            feet.x.floor() as i32,
            feet.y.floor() as i32,
            feet.z.floor() as i32,
        );
        if self.initialized && center == self.center {
            return;
        }
        self.center = center;
        self.blocks.clear();

        let mut count = 0usize;
        for dy in -RADIUS..=RADIUS {
            for dz in -RADIUS..=RADIUS {
                for dx in -RADIUS..=RADIUS {
                    let p = BlockPos::new(center.x + dx, center.y + dy, center.z + dz);
                    let b = world.block_at(p);
                    if !b.is_solid() {
                        continue;
                    }
                    // Surface test: at least one non-solid neighbour
                    // (below world bottom counts as solid bedrock).
                    let surface = [
                        BlockPos::new(p.x + 1, p.y, p.z),
                        BlockPos::new(p.x - 1, p.y, p.z),
                        BlockPos::new(p.x, p.y + 1, p.z),
                        BlockPos::new(p.x, p.y - 1, p.z),
                        BlockPos::new(p.x, p.y, p.z + 1),
                        BlockPos::new(p.x, p.y, p.z - 1),
                    ]
                    .iter()
                    .any(|n| {
                        if n.y < 0 {
                            false
                        } else {
                            world.block_at(*n).is_solid() == false
                        }
                    });
                    if surface {
                        self.blocks.insert(p, b.as_u8());
                        count += 1;
                    }
                }
            }
        }
        self.initialized = true;
        // Sanity: if the agent is standing on / inside solid terrain, the
        // volume must have cached at least one surface block. Being airborne
        // (e.g. after fly mode, or mid-fall) can legitimately yield an empty
        // volume, so only assert when there is solid block at or under the
        // feet. (Debug-only; the extra lookups are gated to debug builds.)
        #[cfg(debug_assertions)]
        {
            let below = world.block_at(BlockPos::new(center.x, center.y - 1, center.z));
            let here = world.block_at(center);
            if below.is_solid() || here.is_solid() {
                debug_assert!(
                    count > 0,
                    "cache empty while agent is on/in solid terrain at {center:?}"
                );
            }
        }
    }

    /// Look up a block: cache first, world as fallback.
    pub fn lookup(&self, p: BlockPos, world: &mut World) -> rustcraft_world::Block {
        if let Some(&b) = self.blocks.get(&p) {
            return rustcraft_world::Block::from_u8(b);
        }
        world.block_at(p)
    }

    /// True when the position is inside the cached volume (cache is
    /// authoritative there, modulo staleness until the next rebuild).
    pub fn contains(&self, p: BlockPos) -> bool {
        (p.x - self.center.x).abs() <= RADIUS
            && (p.y - self.center.y).abs() <= RADIUS
            && (p.z - self.center.z).abs() <= RADIUS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Vec3;

    #[test]
    fn cache_covers_agent_surroundings() {
        let mut world = World::new(1337);
        let h = world.height_at(8, 8);
        let feet = Vec3::new(8.5, h as f32 + 1.0, 8.5);

        let mut cache = SurfaceCache::new();
        cache.update(&mut world, feet);

        assert!(!cache.is_empty());
        // A full 17^3 volume is 4913 blocks; only surface blocks are stored.
        assert!(
            cache.len() < 4913 / 2,
            "cache should only hold surface blocks, got {}",
            cache.len()
        );
        // Ground under the agent is cached.
        let ground = BlockPos::new(8, h, 8);
        assert!(cache.blocks.contains_key(&ground), "ground block missing");
        // Lookup matches the world.
        for dx in -3..=3 {
            for dz in -3..=3 {
                let p = BlockPos::new(8 + dx, h, 8 + dz);
                assert_eq!(cache.lookup(p, &mut world), world.block_at(p));
            }
        }
    }

    #[test]
    fn cache_survives_movement() {
        let mut world = World::new(1337);
        let h = world.height_at(8, 8);
        let mut cache = SurfaceCache::new();
        let feet = Vec3::new(8.5, h as f32 + 1.0, 8.5);
        cache.update(&mut world, feet);
        let c1 = cache.len();

        // Walk 30 blocks: cache rebuilds, still valid.
        let feet2 = Vec3::new(38.5, world.height_at(38, 8) as f32 + 1.0, 8.5);
        cache.update(&mut world, feet2);
        assert!(cache.len() > 0);
        let _ = c1;

        // Ground under the new position is cached.
        let ground = BlockPos::new(38, world.height_at(38, 8), 8);
        assert!(cache.blocks.contains_key(&ground));
    }
}
