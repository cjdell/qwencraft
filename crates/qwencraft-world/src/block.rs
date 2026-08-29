//! The block registry — every block type in the game, defined in ONE place.
//!
//! Each [`BlockInfo`] fixes a block's identity (id, name), physics (solid
//! collision, water, translucent, passable decal), what players may do with
//! it (place it from the hotbar, break it), the texture ids of its three
//! face orientations, and CPU fallback colours (minimap, hotbar swatches,
//! anything that needs a colour without a GPU).
//!
//! **Appearance is procedural:** the texture id of each face selects a
//! WGSL texture function in `qwencraft-client/src/textures.wgsl` (one
//! small function per texture, sampled in the fragment shader from the
//! face UV + world position + time; `textures.rs` just embeds the file).
//! The WebGL2 shadow renderer (`qwencraft-web/src/verify_gl.rs`) mirrors
//! those functions in GLSL, and the concatenated module is type-checked
//! by naga in `cargo test`
//! (`qwencraft-client/tests/wgsl_valid.rs`) before any browser sees it.
//!
//! **Adding a block type:**
//! 1. add the variant to [`Block`] (next free id),
//! 2. add its [`BlockInfo`] to [`BLOCKS`] (physics + face textures +
//!    colours) and, if placeable, to [`PLACEABLE`],
//! 3. add its texture function(s) to
//!    `qwencraft-client/src/textures.wgsl` (one per new texture id,
//!    next to the others) and mirror them in
//!    `qwencraft-web/src/verify_gl.rs` — `cargo test` validates the WGSL
//!    immediately, `./scripts/verify.sh` checks the pixels.
//!
//! Terrain meshing (faces, light, AO), physics, the hotbar and the
//! dashboard all read this table — no other code needs to change.

/// A voxel block. Stored as a single `u8` (see [`BLOCKS`]).
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Block {
    Air = 0,
    Grass = 1,
    Dirt = 2,
    Stone = 3,
    Sand = 4,
    Water = 5,
    Log = 6,
    Leaves = 7,
    SnowGrass = 8,
    FlowerRed = 9,
    FlowerYellow = 10,
    Planks = 11,
    Cobblestone = 12,
    Brick = 13,
    Glass = 14,
    Tnt = 15,
    Obsidian = 16,
}

// ---------------------------------------------------------------------------
// Texture ids
//
// The WGSL `sample_tex` dispatch (`qwencraft-client/src/textures.rs`) and
// its GLSL mirror (`qwencraft-web/src/verify_gl.rs`) are indexed by these
// — keep the three in lockstep.
// ---------------------------------------------------------------------------
pub const TEX_GRASS_TOP: u8 = 0;
pub const TEX_GRASS_SIDE: u8 = 1;
pub const TEX_DIRT: u8 = 2;
pub const TEX_STONE: u8 = 3;
pub const TEX_SAND: u8 = 4;
pub const TEX_WATER: u8 = 5;
pub const TEX_LOG_SIDE: u8 = 6;
pub const TEX_LOG_TOP: u8 = 7;
pub const TEX_LEAVES: u8 = 8;
pub const TEX_SNOW_TOP: u8 = 9;
pub const TEX_SNOW_SIDE: u8 = 10;
pub const TEX_FLOWER_RED: u8 = 11;
pub const TEX_FLOWER_YELLOW: u8 = 12;
pub const TEX_PLANKS: u8 = 13;
pub const TEX_COBBLE: u8 = 14;
pub const TEX_BRICK: u8 = 15;
pub const TEX_GLASS: u8 = 16;
pub const TEX_TNT_SIDE: u8 = 17;
pub const TEX_TNT_TOP: u8 = 18;
pub const TEX_OBSIDIAN: u8 = 19;
/// Constant dark colour: the wireframe block highlight is drawn through
/// the normal terrain pipeline with this texture id.
pub const TEX_HIGHLIGHT: u8 = 20;

/// Everything the game (and the UI) needs to know about a block.
#[derive(Clone, Copy, Debug)]
pub struct BlockInfo {
    pub id: u8,
    pub name: &'static str,
    /// Solid: participates in collision, lighting occlusion and AO.
    pub solid: bool,
    /// Water: passable, swim physics, rendered translucent.
    pub water: bool,
    /// Solid but translucent (glass): collision + light occlusion, drawn in
    /// the translucent pass (no depth writes, src-alpha blend).
    pub translucent: bool,
    /// Passable ground decal (flowers): no collision, drawn as a flat cross.
    pub flower: bool,
    /// May be placed by players: appears in the hotbar, and the server
    /// honours a Place action only for blocks flagged here.
    pub placeable: bool,
    /// May be broken by players.
    pub breakable: bool,
    /// Texture id of the top face (see the `TEX_*` ids above).
    pub tex_top: u8,
    /// Texture id of the four side faces.
    pub tex_side: u8,
    /// Texture id of the bottom face.
    pub tex_bottom: u8,
    /// CPU fallback colour of the top face (linear-ish RGB, 0..1).
    pub color_top: [f32; 3],
    /// CPU fallback colour of the side faces.
    pub color_side: [f32; 3],
    /// CPU fallback colour of the bottom face.
    pub color_bottom: [f32; 3],
}

const fn block(
    id: u8,
    name: &'static str,
    solid: bool,
    water: bool,
    translucent: bool,
    flower: bool,
    placeable: bool,
    breakable: bool,
    tex_top: u8,
    tex_side: u8,
    tex_bottom: u8,
    color_top: [f32; 3],
    color_side: [f32; 3],
    color_bottom: [f32; 3],
) -> BlockInfo {
    BlockInfo {
        id,
        name,
        solid,
        water,
        translucent,
        flower,
        placeable,
        breakable,
        tex_top,
        tex_side,
        tex_bottom,
        color_top,
        color_side,
        color_bottom,
    }
}

/// The registry, indexed by block id (`BLOCKS[b as usize]`).
pub const BLOCKS: [BlockInfo; 17] = [
    // id  name           solid water trans flower place break  top  side  bottom  colour (top, side, bottom)
    block(0, "Air", false, false, false, false, false, false,
          0, 0, 0,
          [1.0, 1.0, 1.0], [1.0, 1.0, 1.0], [1.0, 1.0, 1.0]),
    block(1, "Grass", true, false, false, false, true, true,
          TEX_GRASS_TOP, TEX_GRASS_SIDE, TEX_DIRT,
          [0.36, 0.65, 0.28], [0.45, 0.52, 0.26], [0.55, 0.39, 0.26]),
    block(2, "Dirt", true, false, false, false, true, true,
          TEX_DIRT, TEX_DIRT, TEX_DIRT,
          [0.55, 0.39, 0.26], [0.55, 0.39, 0.26], [0.48, 0.34, 0.22]),
    block(3, "Stone", true, false, false, false, true, true,
          TEX_STONE, TEX_STONE, TEX_STONE,
          [0.52, 0.53, 0.55], [0.50, 0.51, 0.53], [0.44, 0.45, 0.47]),
    block(4, "Sand", true, false, false, false, true, true,
          TEX_SAND, TEX_SAND, TEX_SAND,
          [0.87, 0.82, 0.60], [0.85, 0.80, 0.58], [0.78, 0.73, 0.53]),
    block(5, "Water", false, true, false, false, false, false,
          TEX_WATER, TEX_WATER, TEX_WATER,
          [0.24, 0.45, 0.85], [0.21, 0.41, 0.81], [0.19, 0.37, 0.76]),
    block(6, "Log", true, false, false, false, true, true,
          TEX_LOG_TOP, TEX_LOG_SIDE, TEX_LOG_TOP,
          [0.58, 0.45, 0.28], [0.42, 0.30, 0.17], [0.58, 0.45, 0.28]),
    block(7, "Leaves", true, false, false, false, true, true,
          TEX_LEAVES, TEX_LEAVES, TEX_LEAVES,
          [0.27, 0.52, 0.20], [0.20, 0.44, 0.16], [0.16, 0.38, 0.13]),
    block(8, "Snow Grass", true, false, false, false, true, true,
          TEX_SNOW_TOP, TEX_SNOW_SIDE, TEX_DIRT,
          [0.92, 0.94, 0.97], [0.62, 0.53, 0.40], [0.48, 0.34, 0.22]),
    block(9, "Red Flower", false, false, false, true, false, true,
          TEX_FLOWER_RED, TEX_FLOWER_RED, TEX_FLOWER_RED,
          [0.84, 0.20, 0.18], [0.78, 0.16, 0.14], [0.72, 0.14, 0.12]),
    block(10, "Yellow Flower", false, false, false, true, false, true,
          TEX_FLOWER_YELLOW, TEX_FLOWER_YELLOW, TEX_FLOWER_YELLOW,
          [0.92, 0.78, 0.22], [0.88, 0.72, 0.18], [0.82, 0.66, 0.15]),
    block(11, "Planks", true, false, false, false, true, true,
          TEX_PLANKS, TEX_PLANKS, TEX_PLANKS,
          [0.72, 0.55, 0.34], [0.70, 0.53, 0.32], [0.66, 0.50, 0.30]),
    block(12, "Cobblestone", true, false, false, false, true, true,
          TEX_COBBLE, TEX_COBBLE, TEX_COBBLE,
          [0.55, 0.56, 0.58], [0.52, 0.53, 0.55], [0.46, 0.47, 0.49]),
    block(13, "Brick", true, false, false, false, true, true,
          TEX_BRICK, TEX_BRICK, TEX_BRICK,
          [0.62, 0.30, 0.24], [0.60, 0.28, 0.22], [0.52, 0.24, 0.19]),
    block(14, "Glass", true, false, true, false, true, true,
          TEX_GLASS, TEX_GLASS, TEX_GLASS,
          [0.75, 0.85, 0.90], [0.75, 0.85, 0.90], [0.75, 0.85, 0.90]),
    block(15, "TNT", true, false, false, false, true, true,
          TEX_TNT_TOP, TEX_TNT_SIDE, TEX_TNT_TOP,
          [0.85, 0.35, 0.25], [0.80, 0.30, 0.22], [0.75, 0.28, 0.20]),
    block(16, "Obsidian", true, false, false, false, true, true,
          TEX_OBSIDIAN, TEX_OBSIDIAN, TEX_OBSIDIAN,
          [0.16, 0.10, 0.20], [0.14, 0.09, 0.18], [0.12, 0.08, 0.16]),
];

/// The player's placeable blocks, in hotbar order (id order). The hotbar
/// shows a 9-slot window over this list; the server accepts a Place action
/// only for blocks flagged `placeable` in [`BLOCKS`].
pub const PLACEABLE: [Block; 13] = [
    Block::Grass,
    Block::Dirt,
    Block::Stone,
    Block::Sand,
    Block::Log,
    Block::Leaves,
    Block::SnowGrass,
    Block::Planks,
    Block::Cobblestone,
    Block::Brick,
    Block::Glass,
    Block::Tnt,
    Block::Obsidian,
];

impl Block {
    #[inline]
    pub fn from_u8(v: u8) -> Block {
        match v {
            1 => Block::Grass,
            2 => Block::Dirt,
            3 => Block::Stone,
            4 => Block::Sand,
            5 => Block::Water,
            6 => Block::Log,
            7 => Block::Leaves,
            8 => Block::SnowGrass,
            9 => Block::FlowerRed,
            10 => Block::FlowerYellow,
            11 => Block::Planks,
            12 => Block::Cobblestone,
            13 => Block::Brick,
            14 => Block::Glass,
            15 => Block::Tnt,
            16 => Block::Obsidian,
            _ => Block::Air,
        }
    }

    #[inline]
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// The registry entry for this block.
    #[inline]
    pub const fn info(self) -> &'static BlockInfo {
        &BLOCKS[self as usize]
    }

    /// Solid blocks participate in collision, lighting occlusion and AO.
    /// Water, glass (translucent) and flowers (decals) are passable.
    #[inline]
    pub fn is_solid(self) -> bool {
        self.info().solid
    }

    /// Water (passable, translucent, swim physics).
    #[inline]
    pub fn is_water(self) -> bool {
        self.info().water
    }

    /// Solid but translucent (glass): collides and occludes light, drawn in
    /// the translucent pass.
    #[inline]
    pub fn is_translucent(self) -> bool {
        self.info().translucent
    }

    /// Passable ground decal (flowers): no collision, flat cross geometry.
    #[inline]
    pub fn is_flower(self) -> bool {
        self.info().flower
    }

    /// May be placed by players (hotbar + server validation).
    #[inline]
    pub fn is_placeable(self) -> bool {
        self.info().placeable
    }

    /// May be broken by players.
    #[inline]
    pub fn is_breakable(self) -> bool {
        self.info().breakable
    }

    /// Base colour of the top face (linear-ish RGB, 0..1) — CPU fallback.
    #[inline]
    pub fn color_top(self) -> [f32; 3] {
        self.info().color_top
    }

    /// Base colour of side faces (CPU fallback).
    #[inline]
    pub fn color_side(self) -> [f32; 3] {
        self.info().color_side
    }

    /// Base colour of the bottom face (CPU fallback).
    #[inline]
    pub fn color_bottom(self) -> [f32; 3] {
        self.info().color_bottom
    }

    /// Colour for a face with the given direction (0=top, 1=bottom, 2=side).
    #[inline]
    pub fn color_for_dir(self, dir: u8) -> [f32; 3] {
        match dir {
            0 => self.color_top(),
            1 => self.color_bottom(),
            _ => self.color_side(),
        }
    }

    /// Texture id of a face with the given direction (0=top, 1=bottom,
    /// 2=side) — the id the mesh builder stamps into the vertex.
    #[inline]
    pub fn tex_for_dir(self, dir: u8) -> u8 {
        match dir {
            0 => self.info().tex_top,
            1 => self.info().tex_bottom,
            _ => self.info().tex_side,
        }
    }
}

impl From<Block> for u8 {
    fn from(b: Block) -> Self {
        b.as_u8()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_consistent() {
        // Every id in 0..BLOCKS.len() maps to the right entry.
        for b in 0..BLOCKS.len() as u8 {
            let block = Block::from_u8(b);
            assert_eq!(block.as_u8(), b);
            assert_eq!(block.info().id, b);
        }
        // Unknown ids fall back to air (and are therefore never placeable).
        for v in [17, 200, 255u8] {
            assert_eq!(Block::from_u8(v), Block::Air);
            assert!(!Block::from_u8(v).is_placeable());
        }
        // PLACEABLE is exactly the placeable-flagged set, in id order.
        let flagged: Vec<Block> = BLOCKS
            .iter()
            .filter(|i| i.placeable)
            .map(|i| Block::from_u8(i.id))
            .collect();
        assert_eq!(flagged, PLACEABLE.to_vec());
        // Placeable blocks must be solid (you build with solid stuff);
        // air/water/flowers must not be placeable.
        for &b in &PLACEABLE {
            assert!(b.is_solid(), "{:?} placeable but not solid", b);
            assert!(b.is_breakable());
        }
        assert!(!Block::Air.is_placeable());
        assert!(!Block::Water.is_placeable());
        assert!(!Block::FlowerRed.is_placeable());
        assert!(!Block::FlowerYellow.is_placeable());
        // Physics flags are mutually exclusive.
        for info in &BLOCKS {
            let kinds = [info.solid, info.water, info.flower]
                .iter()
                .filter(|&&k| k)
                .count();
            assert!(kinds <= 1, "{:?} has multiple physics kinds", info.name);
            assert!(!info.translucent || info.solid, "{:?} translucent but not solid", info.name);
        }
    }

    #[test]
    fn face_textures_and_colours_are_set() {
        // No block may reference a texture id without a WGSL function:
        // the dispatch (and its GLSL mirror) covers 0..=TEX_HIGHLIGHT.
        for info in &BLOCKS {
            for t in [info.tex_top, info.tex_side, info.tex_bottom] {
                assert!(
                    (t as usize) <= TEX_HIGHLIGHT as usize,
                    "{:?} references unknown texture {t}",
                    info.name
                );
            }
            for c in [info.color_top, info.color_side, info.color_bottom] {
                for ch in c {
                    assert!((0.0..=1.0).contains(&ch), "{:?} colour out of range", info.name);
                }
            }
        }
        // Spot-check the multi-texture blocks.
        assert_eq!(Block::Log.tex_for_dir(0), TEX_LOG_TOP);
        assert_eq!(Block::Log.tex_for_dir(2), TEX_LOG_SIDE);
        assert_eq!(Block::Grass.tex_for_dir(0), TEX_GRASS_TOP);
        assert_eq!(Block::Grass.tex_for_dir(1), TEX_DIRT);
        assert_eq!(Block::Tnt.tex_for_dir(2), TEX_TNT_SIDE);
    }
}
