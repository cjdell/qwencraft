//! Per-agent dense local block window — a small 3D volume of the *entire*
//! world (solids and air) around the agent's feet.
//!
//! The physics probes only touch the agent AABB (~1x2x1 blocks around the
//! feet), so a (2*RADIUS+1)^3 volume (7^3 = 343 blocks, 343 bytes) always
//! covers them. Storing every block — not just surface blocks — means a
//! steady-state physics lookup is always served from this tiny window and
//! never touches the world's chunk buffers. That is the property the NPC
//! load test verifies: with many agents, physics cost must stay bounded by
//! the per-agent window, not by materialised chunks.
//!
//! The window is rebuilt whenever the agent's centre cell changes (or it is
//! marked dirty by a world edit inside it); lookups outside the window fall
//! back to the world (materialising a chunk on demand) so correctness never
//! depends on the window being complete.

use rustcraft_world::{Block, BlockPos};

use crate::world::World;

/// Half-size (in blocks) of the cubic window volume around the agent.
///
/// Keep this small: every cell change rebuilds the volume, sampling one
/// `world.block_at` per block (which can generate chunks on demand).
const RADIUS: i32 = 3;
/// Side length of the window volume in blocks (2*RADIUS+1).
pub const SIDE: i32 = 2 * RADIUS + 1;
/// Number of blocks in the window volume.
pub const CELLS: usize = (SIDE * SIDE * SIDE) as usize;

/// Lookup/rebuild accounting for one agent's window (see `LocalBlockCache`).
///
/// In steady state (agent moving, no edits) `hits == lookups` and
/// `solid_misses == 0`: every physics probe is answered from the window.
/// A non-zero `solid_misses` means physics read a *solid* from the world's
/// chunk buffers instead of the cache — the load test watches this.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Total `lookup` calls.
    pub lookups: u64,
    /// Lookups answered from the window.
    pub hits: u64,
    /// Lookups that fell back to the world and found a *solid* block there.
    /// (Air fallbacks are cheap-ish but solid ones indicate the window
    /// failed to cover a physics probe.)
    pub solid_misses: u64,
    /// Full window rebuilds performed.
    pub rebuilds: u64,
    /// `world.block_at` calls spent inside rebuilds (chunk materialisation
    /// pressure — the cost the window exists to amortise).
    pub rebuild_probes: u64,
}

impl CacheStats {
    pub fn add(&mut self, o: CacheStats) {
        self.lookups += o.lookups;
        self.hits += o.hits;
        self.solid_misses += o.solid_misses;
        self.rebuilds += o.rebuilds;
        self.rebuild_probes += o.rebuild_probes;
    }
}

/// Dense (2*RADIUS+1)^3 window of all blocks around one agent's feet cell.
pub struct LocalBlockCache {
    /// Block cell containing the agent's feet (centre of the window).
    center: BlockPos,
    /// Window contents, x fastest: `blocks[((y - R) * SIDE + (z - R)) * SIDE + (x - R)]`.
    blocks: [u8; CELLS],
    initialized: bool,
    /// Set by a world edit inside the window: force a rebuild on the next
    /// `update` even if the centre cell has not changed.
    dirty: bool,
    stats: CacheStats,
}

impl Default for LocalBlockCache {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalBlockCache {
    pub fn new() -> Self {
        Self {
            center: BlockPos::new(0, 0, 0),
            blocks: [0u8; CELLS],
            initialized: false,
            dirty: false,
            stats: CacheStats::default(),
        }
    }

    pub fn center(&self) -> BlockPos {
        self.center
    }

    /// True when `p` is inside the window volume.
    pub fn contains(&self, p: BlockPos) -> bool {
        (p.x - self.center.x).abs() <= RADIUS
            && (p.y - self.center.y).abs() <= RADIUS
            && (p.z - self.center.z).abs() <= RADIUS
    }

    /// Force a rebuild on the next `update` (called when a world edit lands
    /// inside the window; without it the window would serve the stale block
    /// until the agent moved to a new centre cell).
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn stats(&self) -> CacheStats {
        self.stats
    }

    pub fn reset_stats(&mut self) {
        self.stats = CacheStats::default();
    }

    /// Rebuild the window if the agent moved to a new centre cell (or the
    /// window is dirty). Samples every cell of the volume with one
    /// `world.block_at` (chunks materialise on demand — this is the only
    /// world access agents need for physics).
    pub fn update(&mut self, world: &mut World, feet: crate::Vec3) {
        let center = BlockPos::new(
            feet.x.floor() as i32,
            feet.y.floor() as i32,
            feet.z.floor() as i32,
        );
        if self.initialized && !self.dirty && center == self.center {
            return;
        }
        self.center = center;
        self.dirty = false;

        let mut probes = 0usize;
        for dy in -RADIUS..=RADIUS {
            for dz in -RADIUS..=RADIUS {
                for dx in -RADIUS..=RADIUS {
                    let p = BlockPos::new(center.x + dx, center.y + dy, center.z + dz);
                    let b = world.block_at(p);
                    probes += 1;
                    self.blocks[Self::index(center, p)] = b.as_u8();
                }
            }
        }
        self.initialized = true;
        self.stats.rebuilds += 1;
        self.stats.rebuild_probes += probes as u64;
    }

    const fn index(center: BlockPos, p: BlockPos) -> usize {
        let (dx, dy, dz) = (p.x - center.x, p.y - center.y, p.z - center.z);
        (((dy + RADIUS) * SIDE + (dz + RADIUS)) * SIDE + (dx + RADIUS)) as usize
    }

    /// Look up a block: window first, world as fallback (only for cells
    /// outside the volume, or before the first build).
    pub fn lookup(&mut self, p: BlockPos, world: &mut World) -> Block {
        self.stats.lookups += 1;
        if self.initialized && self.contains(p) {
            self.stats.hits += 1;
            return Block::from_u8(self.blocks[Self::index(self.center, p)]);
        }
        let b = world.block_at(p);
        if b.is_solid() {
            self.stats.solid_misses += 1;
        }
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Vec3;

    #[test]
    fn window_covers_agent_surroundings() {
        let mut world = World::new(1337);
        let h = world.height_at(8, 8);
        let feet = Vec3::new(8.5, h as f32 + 1.0, 8.5);

        let mut cache = LocalBlockCache::new();
        cache.update(&mut world, feet);

        assert_eq!(cache.stats().rebuilds, 1, "rebuild must have happened");
        // The rebuild samples exactly one block per window cell.
        assert_eq!(cache.stats().rebuild_probes, CELLS as u64);

        // Ground under the agent is stored (and it is solid).
        let ground = BlockPos::new(8, h, 8);
        assert!(cache.contains(ground));
        assert_eq!(cache.lookup(ground, &mut world), world.block_at(ground));

        // Every cell of the volume matches the world (air included — that
        // is what makes steady-state lookups fully cache-served).
        for dy in -RADIUS..=RADIUS {
            for dz in -RADIUS..=RADIUS {
                for dx in -RADIUS..=RADIUS {
                    let p = BlockPos::new(8 + dx, h + 1 + dy, 8 + dz);
                    assert_eq!(cache.lookup(p, &mut world), world.block_at(p), "mismatch at {p:?}");
                }
            }
        }
        // All lookups above were window hits.
        let st = cache.stats();
        assert_eq!(st.hits, st.lookups, "no world fallback expected in steady state");
        assert_eq!(st.solid_misses, 0);
    }

    #[test]
    fn window_survives_movement() {
        let mut world = World::new(1337);
        let h = world.height_at(8, 8);
        let mut cache = LocalBlockCache::new();
        let feet = Vec3::new(8.5, h as f32 + 1.0, 8.5);
        cache.update(&mut world, feet);
        let rebuilds_after_first = cache.stats().rebuilds;

        // Same cell: no rebuild.
        cache.update(&mut world, feet);
        assert_eq!(cache.stats().rebuilds, rebuilds_after_first);

        // Walk 30 blocks: window rebuilds, still valid.
        let feet2 = Vec3::new(38.5, world.height_at(38, 8) as f32 + 1.0, 8.5);
        cache.update(&mut world, feet2);
        assert_eq!(cache.stats().rebuilds, rebuilds_after_first + 1);

        // Ground under the new position is stored.
        let ground = BlockPos::new(38, world.height_at(38, 8), 8);
        assert_eq!(cache.lookup(ground, &mut world), world.block_at(ground));
    }

    #[test]
    fn dirty_window_rebuilds_on_same_cell() {
        let mut world = World::new(1337);
        let h = world.height_at(8, 8);
        let feet = Vec3::new(8.5, h as f32 + 1.0, 8.5);
        let mut cache = LocalBlockCache::new();
        cache.update(&mut world, feet);
        let ground = BlockPos::new(8, h, 8);
        assert!(cache.lookup(ground, &mut world).is_solid());

        // A world edit inside the window is invisible to the window until
        // it is marked dirty + rebuilt (Server does this on every edit).
        let dirty = world.set_block(ground, Block::Air);
        let _ = dirty;
        assert!(
            cache.lookup(ground, &mut world).is_solid(),
            "stale window must keep serving until marked dirty"
        );
        cache.mark_dirty();
        cache.update(&mut world, feet); // same centre cell — dirty forces a rebuild
        assert_eq!(cache.lookup(ground, &mut world), Block::Air);
        assert_eq!(cache.stats().rebuilds, 2);
    }
}
