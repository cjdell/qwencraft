//! Per-column "topmost block" queries for the dashboard minimap.
//!
//! Pure functions of (seed, coordinates, edits): the map keeps its own
//! [`WorldGen`] + column-height cache + edit overlay, so it can run off the
//! authoritative world's lock (see `qwencraft-net/src/map.rs`). Living in
//! the world crate — next to the terrain rules they encode — is what keeps
//! them from drifting: `top_block`/`sub_top_block` are the *same* functions
//! the generator stamps chunks with.
//!
//! Deliberate approximations (all invisible at minimap scale):
//! - caves below a broken surface read as stone (a cave can't be a column's
//!   topmost block unless the surface is broken, in which case we'd need
//!   per-cell noise — not worth it for a 1px/block map);
//! - unedited columns only consider the tree rooted in that column, not
//!   canopy overhang from neighbours (a few % of tree pixels);
//! - flowers (passable ground decals) are not modelled.

use std::collections::HashMap;

use crate::terrain::{sub_top_block, top_block, SEA_LEVEL, SNOW_LEVEL};
use crate::{Block, WORLD_HEIGHT, WorldGen};

/// Top of an unedited column: water (low-lying), own-column tree
/// (trunk/canopy top), or the terrain surface. O(1) given the column height.
pub fn column_top_base(
    x: i32,
    z: i32,
    h: i32,
    gen: &WorldGen,
    heights: &mut HashMap<(i32, i32), i32>,
) -> (i32, Block) {
    if h < SEA_LEVEL {
        return (SEA_LEVEL, Block::Water);
    }
    // Trees only grow on grass between the waterline and the snowline.
    if h < SNOW_LEVEL {
        if let Some(t) = gen.tree_at_cached(x, z, heights) {
            // The column's own highest tree block is the 5x5 canopy
            // layer at base + trunk (the 3x3 cap one above skips the
            // centre).
            return (h + t.trunk, Block::Leaves);
        }
    }
    (h, top_block(h))
}

/// Top of an edited column: scan from the top of the world, edits
/// overriding terrain cell by cell. `edits` holds the column's current
/// blocks (last-wins) as (y, block) pairs. `None` in practice (y=0
/// bedrock is unbreakable) — callers render their own "unknown".
pub fn column_top_edited(
    x: i32,
    z: i32,
    h: i32,
    edits: &[(i32, Block)],
    gen: &WorldGen,
    heights: &mut HashMap<(i32, i32), i32>,
) -> Option<(i32, Block)> {
    // Tree roots in the 5x5 neighbourhood: a canopy overhangs up to 2
    // blocks, so a neighbour's leaves can be this column's topmost
    // block (only relevant here — edited columns are the few that scan).
    let mut trees: Vec<(i32, i32, i32, i32)> = Vec::new(); // (rx, rz, base, trunk)
    for dz in -2..=2 {
        for dx in -2..=2 {
            let (nx, nz) = (x + dx, z + dz);
            let nh = *heights
                .entry((nx, nz))
                .or_insert_with(|| gen.height(nx, nz));
            if (SEA_LEVEL..SNOW_LEVEL).contains(&nh) {
                if let Some(t) = gen.tree_at_cached(nx, nz, heights) {
                    trees.push((nx, nz, nh, t.trunk));
                }
            }
        }
    }
    for y in (0..WORLD_HEIGHT).rev() {
        let b = cell_block(x, y, z, h, &trees, edits);
        if b != Block::Air {
            return Some((y, b));
        }
    }
    None
}

/// Current block at (x, y, z): an edit for that exact cell wins, else
/// pure terrain (caves below the surface are not modelled — see the
/// module docs).
fn cell_block(
    x: i32,
    y: i32,
    z: i32,
    h: i32,
    trees: &[(i32, i32, i32, i32)],
    edits: &[(i32, Block)],
) -> Block {
    if let Some(&(_, b)) = edits.iter().find(|&&(ey, _)| ey == y) {
        return b;
    }
    if y < 0 || y >= WORLD_HEIGHT {
        return Block::Air;
    }
    if y > h {
        // Water fills low-lying columns up to the sea level.
        if y <= SEA_LEVEL {
            return Block::Water;
        }
        // Tree blocks (trunk / canopy) from the neighbourhood roots.
        for &(rx, rz, base, trunk) in trees {
            let ddx = x - rx;
            let ddz = z - rz;
            let dy = y - base;
            if ddx == 0 && ddz == 0 && (1..=trunk).contains(&dy) {
                return Block::Log;
            }
            if dy == trunk && ddx.abs() <= 2 && ddz.abs() <= 2 {
                return Block::Leaves;
            }
            if dy == trunk + 1 && ddx.abs() <= 1 && ddz.abs() <= 1 && !(ddx == 0 && ddz == 0) {
                return Block::Leaves;
            }
        }
        return Block::Air;
    }
    if y == 0 {
        return Block::Stone;
    }
    if y == h {
        return top_block(h);
    }
    if y >= h - 3 {
        // Sub-surface: dirt (or sand under water).
        return sub_top_block(h);
    }
    Block::Stone
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dry grass column without a tree near the origin (seed 1337).
    fn grass_column(gen: &WorldGen) -> (i32, i32, i32) {
        for dz in 0..16 {
            for dx in 0..16 {
                let (x, z) = (8 + dx, 8 + dz);
                let h = gen.height(x, z);
                if h >= SEA_LEVEL && h < SNOW_LEVEL && gen.tree_at(x, z).is_none() {
                    return (x, z, h);
                }
            }
        }
        panic!("no grass column near the origin")
    }

    #[test]
    fn unedited_grass_column_reports_its_surface() {
        let gen = WorldGen::new(1337);
        let mut heights = HashMap::new();
        let (x, z, h) = grass_column(&gen);
        assert_eq!(
            column_top_base(x, z, h, &gen, &mut heights),
            (h, Block::Grass)
        );
    }

    #[test]
    fn unedited_tree_column_reports_its_canopy() {
        let gen = WorldGen::new(1337);
        let mut heights = HashMap::new();
        let mut found = None;
        'outer: for dz in -32..32 {
            for dx in -32..32 {
                if let Some(t) = gen.tree_at(dx, dz) {
                    found = Some((dx, dz, t));
                    break 'outer;
                }
            }
        }
        let (x, z, t) = found.expect("seed 1337 has trees near the origin");
        assert_eq!(
            column_top_base(x, z, t.base, &gen, &mut heights),
            (t.base + t.trunk, Block::Leaves)
        );
    }

    #[test]
    fn unedited_lake_column_reports_the_waterline() {
        let gen = WorldGen::new(1337);
        let mut heights = HashMap::new();
        let mut wet = None;
        'outer: for dz in -128..128 {
            for dx in -128..128 {
                if gen.height(dx, dz) < SEA_LEVEL {
                    wet = Some((dx, dz));
                    break 'outer;
                }
            }
        }
        let (x, z) = wet.expect("seed 1337 has lakes near the origin");
        assert_eq!(
            column_top_base(x, z, gen.height(x, z), &gen, &mut heights),
            (SEA_LEVEL, Block::Water)
        );
    }

    #[test]
    fn edited_column_falls_and_climbs() {
        let gen = WorldGen::new(1337);
        let mut heights = HashMap::new();
        let (x, z, h) = grass_column(&gen);
        // Break the surface: the top falls to the sub-surface (dirt at h-1).
        let edits = vec![(h, Block::Air)];
        assert_eq!(
            column_top_edited(x, z, h, &edits, &gen, &mut heights),
            Some((h - 1, Block::Dirt))
        );
        // A 3-block stone tower: the top climbs to h+3.
        let edits = vec![
            (h, Block::Air),
            (h + 1, Block::Stone),
            (h + 2, Block::Stone),
            (h + 3, Block::Stone),
        ];
        assert_eq!(
            column_top_edited(x, z, h, &edits, &gen, &mut heights),
            Some((h + 3, Block::Stone))
        );
    }

    #[test]
    fn top_block_matches_the_surface_rules() {
        assert_eq!(top_block(5), Block::Sand); // under the waterline
        assert_eq!(top_block(SEA_LEVEL), Block::Grass);
        assert_eq!(top_block(SNOW_LEVEL - 1), Block::Grass);
        assert_eq!(top_block(SNOW_LEVEL), Block::SnowGrass);
        assert_eq!(sub_top_block(5), Block::Sand);
        assert_eq!(sub_top_block(SEA_LEVEL), Block::Dirt);
    }
}
