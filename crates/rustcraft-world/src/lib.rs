//! Procedural world generation for RustCraft.
//!
//! The world is an infinite column of chunks (`CHUNK` x `WORLD_HEIGHT` x `CHUNK`
//! split into `CHUNK`-sized cubes) generated on demand from a seed.
//! Nothing here allocates global state: all generation is a pure function of
//! (seed, chunk coordinates).

pub mod block;
pub mod camera;
pub mod mesh;
pub mod noise;
pub mod terrain;

pub use block::Block;
pub use noise::Noise;
pub use terrain::WorldGen;

/// Edge length of a chunk in blocks.
pub const CHUNK: i32 = 16;
/// Total height of the world in blocks (4 chunk columns of 16).
pub const WORLD_HEIGHT: i32 = 64;
/// Chunk columns above `TERRAIN_MAX` are guaranteed to be air (used to avoid
/// generating them).
pub const TERRAIN_MAX: i32 = 47;
/// Minimum terrain surface height.
pub const TERRAIN_MIN: i32 = 8;

/// Number of block columns in a streamed "chunk region" payload:
/// the 16^3 chunk plus a `REGION_MARGIN` block border so clients can compute
/// lighting/AO without waiting for neighbouring chunks.
pub const REGION_MARGIN: i32 = 5;
pub const REGION: i32 = CHUNK + 2 * REGION_MARGIN; // 26

/// Euclidean modulo (always in `0..m` for `m > 0`).
#[inline]
pub fn imod(a: i32, m: i32) -> i32 {
    let r = a % m;
    if r < 0 { r + m } else { r }
}

/// Position of a chunk in chunk space.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ChunkPos {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl ChunkPos {
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    /// Chunk containing the given block position (works for negative coords).
    pub fn of(p: BlockPos) -> Self {
        Self::new(
            p.x.div_euclid(CHUNK),
            p.y.div_euclid(CHUNK),
            p.z.div_euclid(CHUNK),
        )
    }

    /// World position of the chunk's minimum corner.
    pub fn origin(&self) -> BlockPos {
        BlockPos::new(self.x * CHUNK, self.y * CHUNK, self.z * CHUNK)
    }

    pub fn xz_distance2(self, o: Self) -> i64 {
        let dx = (self.x - o.x) as i64;
        let dz = (self.z - o.z) as i64;
        dx * dx + dz * dz
    }

    /// True when this chunk column can never contain terrain.
    pub fn guaranteed_air(self) -> bool {
        self.y * CHUNK > TERRAIN_MAX || self.y * CHUNK + (CHUNK - 1) < 0
    }
}

/// Absolute block position in world space.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BlockPos {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl BlockPos {
    pub const fn new(x: i32, y: i32, z: i32) -> Self {
        Self { x, y, z }
    }

    pub fn add(self, o: BlockPos) -> Self {
        Self::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }

    pub fn sub(self, o: BlockPos) -> Self {
        Self::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }

    /// Local (0..CHUNK) position inside the chunk containing `self`.
    pub fn local(&self) -> BlockPos {
        BlockPos::new(imod(self.x, CHUNK), imod(self.y, CHUNK), imod(self.z, CHUNK))
    }

    /// World-space position from chunk + local offset.
    pub fn from_chunk(c: ChunkPos, l: BlockPos) -> Self {
        Self::new(c.x * CHUNK + l.x, c.y * CHUNK + l.y, c.z * CHUNK + l.z)
    }

    /// True when the position is inside the world's Y range.
    pub fn in_world_y(self) -> bool {
        (0..WORLD_HEIGHT).contains(&self.y)
    }
}

/// Flat index into a 16^3 chunk block array.
#[inline]
pub fn chunk_index(l: BlockPos) -> usize {
    let c = CHUNK as usize;
    ((l.y as usize) * c + (l.z as usize)) * c + l.x as usize
}

/// Number of blocks in one chunk.
pub const CHUNK_BLOCKS: usize = (CHUNK * CHUNK * CHUNK) as usize;

/// Flat index into a `REGION`^3 region array.
#[inline]
pub fn region_index(l: BlockPos) -> usize {
    let r = REGION as usize;
    ((l.y as usize) * r + (l.z as usize)) * r + l.x as usize
}

/// Number of blocks in one streamed region payload.
pub const REGION_BLOCKS: usize = (REGION * REGION * REGION) as usize;
