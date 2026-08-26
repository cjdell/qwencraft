//! Block types and their presentation (solid colours for now; textures later).

/// A voxel block. Stored as a single `u8`.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Block {
    Air = 0,
    Grass = 1,
    Dirt = 2,
    Stone = 3,
    Sand = 4,
}

impl Block {
    pub const ALL: [Block; 5] = [Block::Air, Block::Grass, Block::Dirt, Block::Stone, Block::Sand];

    #[inline]
    pub fn from_u8(v: u8) -> Block {
        match v {
            1 => Block::Grass,
            2 => Block::Dirt,
            3 => Block::Stone,
            4 => Block::Sand,
            _ => Block::Air,
        }
    }

    #[inline]
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Solid blocks participate in collision, lighting occlusion and AO.
    #[inline]
    pub fn is_solid(self) -> bool {
        self != Block::Air
    }

    /// Base colour of the top face (linear-ish RGB, 0..1).
    pub fn color_top(self) -> [f32; 3] {
        match self {
            Block::Grass => [0.36, 0.65, 0.28],
            Block::Dirt => [0.55, 0.39, 0.26],
            Block::Stone => [0.52, 0.53, 0.55],
            Block::Sand => [0.87, 0.82, 0.60],
            Block::Air => [1.0, 1.0, 1.0],
        }
    }

    /// Base colour of side faces.
    pub fn color_side(self) -> [f32; 3] {
        match self {
            Block::Grass => [0.45, 0.52, 0.26], // grassy dirt
            Block::Dirt => [0.55, 0.39, 0.26],
            Block::Stone => [0.50, 0.51, 0.53],
            Block::Sand => [0.85, 0.80, 0.58],
            Block::Air => [1.0, 1.0, 1.0],
        }
    }

    /// Base colour of the bottom face.
    pub fn color_bottom(self) -> [f32; 3] {
        match self {
            Block::Grass => [0.55, 0.39, 0.26], // dirt underside
            Block::Dirt => [0.48, 0.34, 0.22],
            Block::Stone => [0.44, 0.45, 0.47],
            Block::Sand => [0.78, 0.73, 0.53],
            Block::Air => [1.0, 1.0, 1.0],
        }
    }

    /// Colour for a face with the given direction (0=top, 1=bottom, 2=side).
    pub fn color_for_dir(self, dir: u8) -> [f32; 3] {
        match dir {
            0 => self.color_top(),
            1 => self.color_bottom(),
            _ => self.color_side(),
        }
    }
}

impl From<Block> for u8 {
    fn from(b: Block) -> Self {
        b.as_u8()
    }
}
