//! World save format.
//!
//! The world's state is the seed plus its **block overrides** — terrain is
//! a pure function of (seed, coordinates), so nothing else is derivable.
//! The save file stores exactly that pair and nothing else:
//!
//! ```text
//!   offset 0:  magic    b"QWCS"
//!   offset 4:  version  u32 LE  (= 1)
//!   offset 8:  seed     u64 LE
//!   offset 16: count    u64 LE
//!   offset 24: count × (x i32 LE, y i32 LE, z i32 LE, block u8)
//! ```
//!
//! The entries are the world's last-wins override set (`World::overrides`):
//! each position appears **at most once**, so the body is a plain set — no
//! order, no replay semantics, no history. Loading a save is therefore just
//! "fold the entries into a fresh world's override layer" (see
//! [`World::load`](crate::world::World::load)).
//!
//! This module is a pure byte codec (no I/O) so it runs on wasm and stays
//! host-testable; the network server owns the file handling (path, atomic
//! replace, save cadence).

use qwencraft_world::{Block, BlockPos, BLOCKS, WORLD_HEIGHT};

use crate::world::Edit;

/// Save file magic.
pub const SAVE_MAGIC: &[u8; 4] = b"QWCS";
/// Save format version.
pub const SAVE_VERSION: u32 = 1;
/// The save file's name inside the data directory.
pub const SAVE_FILE_NAME: &str = "world.save";

/// Bytes per override entry (x, y, z, block).
const ENTRY_BYTES: usize = 13;
/// Header size: magic + version + seed + count.
const HEADER_BYTES: usize = 4 + 4 + 8 + 8;

/// Serialize the world's persistent state (seed + block overrides).
///
/// `edits` is normally `world.overrides().collect()`; each position must
/// appear at most once (it is a last-wins set), so entry order is
/// irrelevant.
pub fn encode(seed: u64, edits: &[Edit]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_BYTES + edits.len() * ENTRY_BYTES);
    out.extend_from_slice(SAVE_MAGIC);
    out.extend_from_slice(&SAVE_VERSION.to_le_bytes());
    out.extend_from_slice(&seed.to_le_bytes());
    out.extend_from_slice(&(edits.len() as u64).to_le_bytes());
    for e in edits {
        out.extend_from_slice(&e.pos.x.to_le_bytes());
        out.extend_from_slice(&e.pos.y.to_le_bytes());
        out.extend_from_slice(&e.pos.z.to_le_bytes());
        out.push(e.block.as_u8());
    }
    out
}

/// Parse a save file. Returns `(seed, block overrides)`.
///
/// Anything malformed — wrong magic, unknown version, truncated body,
/// out-of-world position, unknown block id — is an error. A corrupt save
/// must fail loudly at startup rather than silently start a fresh world
/// (which would make players' builds "disappear").
pub fn decode(bytes: &[u8]) -> Result<(u64, Vec<Edit>), String> {
    if bytes.len() < HEADER_BYTES {
        return Err(format!(
            "truncated header ({} bytes, need {})",
            bytes.len(),
            HEADER_BYTES
        ));
    }
    if &bytes[0..4] != SAVE_MAGIC {
        return Err("bad magic (not a qwencraft world save)".to_string());
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if version != SAVE_VERSION {
        return Err(format!(
            "unsupported save version {version} (this build has {SAVE_VERSION})"
        ));
    }
    let seed = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    let count = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
    let body = &bytes[HEADER_BYTES..];
    if body.len() < count as usize * ENTRY_BYTES {
        return Err(format!(
            "truncated body ({count} entries declared, {} bytes present)",
            body.len()
        ));
    }
    let mut edits = Vec::with_capacity(count as usize);
    for entry in body.chunks_exact(ENTRY_BYTES) {
        let x = i32::from_le_bytes(entry[0..4].try_into().unwrap());
        let y = i32::from_le_bytes(entry[4..8].try_into().unwrap());
        let z = i32::from_le_bytes(entry[8..12].try_into().unwrap());
        let b = entry[12];
        if !(0..WORLD_HEIGHT).contains(&y) {
            return Err(format!(
                "entry ({x}, {y}, {z}) is outside the world's Y range (0..{WORLD_HEIGHT})"
            ));
        }
        if b as usize >= BLOCKS.len() {
            return Err(format!("entry ({x}, {y}, {z}) has unknown block id {b}"));
        }
        edits.push(Edit {
            pos: BlockPos::new(x, y, z),
            block: Block::from_u8(b),
        });
    }
    Ok((seed, edits))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qwencraft_world::Block;

    fn sample_edits() -> Vec<Edit> {
        vec![
            Edit { pos: BlockPos::new(8, 30, 8), block: Block::Stone },
            Edit { pos: BlockPos::new(-120, 12, 240), block: Block::Air },
            Edit { pos: BlockPos::new(0, 0, 0), block: Block::Obsidian },
            Edit { pos: BlockPos::new(-1, 63, 1), block: Block::Water },
        ]
    }

    #[test]
    fn roundtrip() {
        let edits = sample_edits();
        let bytes = encode(1337, &edits);
        let (seed, back) = decode(&bytes).expect("decode");
        assert_eq!(seed, 1337);
        assert_eq!(back, edits);
    }

    #[test]
    fn roundtrip_empty() {
        let bytes = encode(42, &[]);
        assert_eq!(bytes.len(), HEADER_BYTES);
        let (seed, back) = decode(&bytes).expect("decode");
        assert_eq!(seed, 42);
        assert!(back.is_empty());
    }

    #[test]
    fn deterministic() {
        let edits = sample_edits();
        assert_eq!(encode(7, &edits), encode(7, &edits));
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = encode(1, &[]);
        bytes[0] = b'X';
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn rejects_unknown_version() {
        let mut bytes = encode(1, &[]);
        bytes[4..8].copy_from_slice(&99u32.to_le_bytes());
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn rejects_truncated_header_and_body() {
        let bytes = encode(1, &sample_edits());
        assert!(decode(&bytes[..HEADER_BYTES - 1]).is_err());
        // Header intact, one entry short.
        assert!(decode(&bytes[..bytes.len() - 5]).is_err());
        // Count claims more entries than are present.
        let mut bad = encode(1, &[]);
        bad[16..24].copy_from_slice(&5u64.to_le_bytes());
        assert!(decode(&bad).is_err());
    }

    #[test]
    fn rejects_out_of_world_y_and_unknown_block() {
        let mut bytes = encode(1, &[]);
        // y = WORLD_HEIGHT (out of range).
        bytes[16..24].copy_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&8i32.to_le_bytes());
        bytes.extend_from_slice(&WORLD_HEIGHT.to_le_bytes());
        bytes.extend_from_slice(&8i32.to_le_bytes());
        bytes.push(Block::Stone.as_u8());
        assert!(decode(&bytes).is_err());
        // y = -1.
        let mut bytes = encode(1, &[]);
        bytes[16..24].copy_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&8i32.to_le_bytes());
        bytes.extend_from_slice(&(-1i32).to_le_bytes());
        bytes.extend_from_slice(&8i32.to_le_bytes());
        bytes.push(Block::Stone.as_u8());
        assert!(decode(&bytes).is_err());
        // Unknown block id.
        let mut bytes = encode(1, &[]);
        bytes[16..24].copy_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&8i32.to_le_bytes());
        bytes.extend_from_slice(&10i32.to_le_bytes());
        bytes.extend_from_slice(&8i32.to_le_bytes());
        bytes.push(BLOCKS.len() as u8);
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn seed_mismatch_is_detectable() {
        let (a, _) = decode(&encode(111, &[])).unwrap();
        let (b, _) = decode(&encode(222, &[])).unwrap();
        assert_ne!(a, b);
    }
}
