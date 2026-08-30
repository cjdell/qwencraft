//! World save format.
//!
//! The world's state is the seed plus its **block overrides** — terrain is
//! a pure function of (seed, coordinates), so nothing else is derivable —
//! plus (v2) the **player identities** of the rejoin feature: token → last
//! known position/view/name/colour. The identities live in the save (not a
//! sidecar) on purpose: the save is bound to the seed, so a token only
//! works against the world it was minted for ("same world only" for free),
//! and they ride the existing atomic-write / cadence / final-save machinery.
//!
//! ```text
//!   offset 0:  magic    b"QWCS"
//!   offset 4:  version  u32 LE  (= 2)
//!   offset 8:  seed     u64 LE
//!   offset 16: edits    u64 LE
//!   offset 24: edits × (x i32 LE, y i32 LE, z i32 LE, block u8)
//!   (v2 only) players  u64 LE
//!   (v2 only) players × (
//!       token     16 bytes
//!       pos       f32 LE × 3   (feet)
//!       yaw       f32 LE
//!       pitch     f32 LE
//!       name      u16 LE len + UTF-8
//!       color     u8 × 3
//!       last_seen u64 LE       (unix seconds)
//!   )
//! ```
//!
//! The override entries are the world's last-wins override set
//! (`World::overrides`): each position appears **at most once**, so the
//! body is a plain set — no order, no replay semantics, no history.
//! Loading a save is therefore just "fold the entries into a fresh
//! world's override layer" (see
//! [`World::load`](crate::world::World::load)). The player records are
//! likewise a set keyed by token (order irrelevant; a duplicate token is
//! last-wins).
//!
//! v1 files (no player section) still load — as "no identities".
//!
//! This module is a pure byte codec (no I/O) so it runs on wasm and stays
//! host-testable; the network server owns the file handling (path, atomic
//! replace, save cadence).

use qwencraft_world::{Block, BlockPos, Vec3, BLOCKS, WORLD_HEIGHT};

use crate::world::Edit;

/// Save file magic.
pub const SAVE_MAGIC: &[u8; 4] = b"QWCS";
/// Save format version.
pub const SAVE_VERSION: u32 = 2;
/// The first version that carries the player-identity section.
const PLAYERS_FROM_VERSION: u32 = 2;
/// The save file's name inside the data directory.
pub const SAVE_FILE_NAME: &str = "world.save";

/// Bytes per override entry (x, y, z, block).
const ENTRY_BYTES: usize = 13;
/// Header size: magic + version + seed + count.
const HEADER_BYTES: usize = 4 + 4 + 8 + 8;
/// Fixed size of a player record with an EMPTY name (token + pos + yaw +
/// pitch + name len + color + last_seen); the name bytes sit in the middle.
const PLAYER_FIXED_BYTES: usize = 16 + 12 + 4 + 4 + 2 + 3 + 8;
/// Names are sanitised far below this on the way in (24-char cap).
const MAX_NAME_BYTES: usize = 64;

/// One player's persistent identity (the rejoin feature — see
/// `ClientMsg::Rejoin` and the `token` in `ServerMsg::Hello`). The token
/// is the identity; everything else is the last known state, restored when
/// the token is presented to the world it was minted for.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerRecord {
    /// Feet position (world space).
    pub pos: Vec3,
    /// View orientation at disconnect.
    pub yaw: f32,
    pub pitch: f32,
    /// Display name (sanitised by `Server::set_profile`).
    pub name: String,
    /// Sphere colour.
    pub color: [u8; 3],
    /// Unix seconds of the last disconnect (ordering for cap eviction).
    pub last_seen: u64,
}

/// Serialize the world's persistent state (seed + block overrides + player
/// identities).
///
/// `edits` is normally `world.overrides().collect()`; each position must
/// appear at most once (it is a last-wins set), so entry order is
/// irrelevant. `players` is the rejoin registry's snapshot; order is
/// irrelevant (a set keyed by token).
pub fn encode(seed: u64, edits: &[Edit], players: &[( [u8; 16], PlayerRecord )]) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        HEADER_BYTES + edits.len() * ENTRY_BYTES + 8 + players.len() * (PLAYER_FIXED_BYTES + 16),
    );
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
    out.extend_from_slice(&(players.len() as u64).to_le_bytes());
    for (token, r) in players {
        out.extend_from_slice(token);
        out.extend_from_slice(&r.pos.x.to_le_bytes());
        out.extend_from_slice(&r.pos.y.to_le_bytes());
        out.extend_from_slice(&r.pos.z.to_le_bytes());
        out.extend_from_slice(&r.yaw.to_le_bytes());
        out.extend_from_slice(&r.pitch.to_le_bytes());
        out.extend_from_slice(&(r.name.len() as u16).to_le_bytes());
        out.extend_from_slice(r.name.as_bytes());
        for c in r.color {
            out.push(c);
        }
        out.extend_from_slice(&r.last_seen.to_le_bytes());
    }
    out
}

/// Parse a save file. Returns `(seed, block overrides, player identities)`.
///
/// Anything malformed — wrong magic, unknown version, truncated body,
/// out-of-world position, unknown block id, non-finite restored state,
/// non-UTF-8 or over-long name, trailing bytes — is an error. A corrupt
/// save must fail loudly at startup rather than silently start a fresh
/// world (which would make players' builds "disappear"). v1 files
/// (predating the player section) decode with an empty identity list.
pub fn decode(
    bytes: &[u8],
) -> Result<(u64, Vec<Edit>, Vec<([u8; 16], PlayerRecord)>), String> {
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
    if version < 1 || version > SAVE_VERSION {
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
    let edits_bytes = &body[..count as usize * ENTRY_BYTES];
    let mut edits = Vec::with_capacity(count as usize);
    for entry in edits_bytes.chunks_exact(ENTRY_BYTES) {
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
            return Err(format!(
                "entry ({x}, {y}, {z}) has unknown block id {b}"
            ));
        }
        edits.push(Edit {
            pos: BlockPos::new(x, y, z),
            block: Block::from_u8(b),
        });
    }
    // v1: nothing may follow the override entries (a v2 file truncated
    // down to a v1 version must not silently drop its identities).
    let rest = &body[edits_bytes.len()..];
    if version < PLAYERS_FROM_VERSION {
        if !rest.is_empty() {
            return Err(format!(
                "v1 save with {} trailing bytes (expected none)",
                rest.len()
            ));
        }
        return Ok((seed, edits, Vec::new()));
    }
    // v2: the player-identity section.
    if rest.len() < 8 {
        return Err("truncated player section (missing count)".to_string());
    }
    let pcount = u64::from_le_bytes(rest[0..8].try_into().unwrap()) as usize;
    if pcount > 65536 {
        return Err(format!("implausible player count {pcount}"));
    }
    let mut off = 8;
    let mut players = Vec::with_capacity(pcount);
    for i in 0..pcount {
        let rec = &rest[off..];
        if rec.len() < PLAYER_FIXED_BYTES {
            return Err(format!("truncated player record {i}"));
        }
        let token: [u8; 16] = rec[0..16].try_into().unwrap();
        let pos = Vec3::new(
            f32::from_le_bytes(rec[16..20].try_into().unwrap()),
            f32::from_le_bytes(rec[20..24].try_into().unwrap()),
            f32::from_le_bytes(rec[24..28].try_into().unwrap()),
        );
        let yaw = f32::from_le_bytes(rec[28..32].try_into().unwrap());
        let pitch = f32::from_le_bytes(rec[32..36].try_into().unwrap());
        if !pos.x.is_finite() || !pos.y.is_finite() || !pos.z.is_finite()
            || !yaw.is_finite()
            || !pitch.is_finite()
        {
            return Err(format!("player record {i} has non-finite state"));
        }
        let name_len = u16::from_le_bytes(rec[36..38].try_into().unwrap()) as usize;
        if name_len > MAX_NAME_BYTES {
            return Err(format!(
                "player record {i} has an over-long name ({name_len} bytes)"
            ));
        }
        let name_bytes =
            rec.get(38..38 + name_len).ok_or_else(|| format!("truncated name in player record {i}"))?;
        let name = String::from_utf8(name_bytes.to_vec())
            .map_err(|_| format!("player record {i} has a non-UTF-8 name"))?;
        let color_slice = rec
            .get(38 + name_len..41 + name_len)
            .ok_or_else(|| format!("truncated colour in player record {i}"))?;
        let color = [color_slice[0], color_slice[1], color_slice[2]];
        let last_seen_slice = rec
            .get(41 + name_len..49 + name_len)
            .ok_or_else(|| format!("truncated last_seen in player record {i}"))?;
        let last_seen = u64::from_le_bytes(last_seen_slice.try_into().unwrap());
        off += PLAYER_FIXED_BYTES + name_len;
        players.push((
            token,
            PlayerRecord {
                pos,
                yaw,
                pitch,
                name,
                color,
                last_seen,
            },
        ));
    }
    if off != rest.len() {
        return Err(format!(
            "trailing bytes after the player section ({} left)",
            rest.len() - off
        ));
    }
    Ok((seed, edits, players))
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

    fn sample_players() -> Vec<([u8; 16], PlayerRecord)> {
        vec![
            (
                [1; 16],
                PlayerRecord {
                    pos: Vec3::new(8.5, 34.0, 8.5),
                    yaw: 1.2,
                    pitch: -0.3,
                    name: "Alice".to_string(),
                    color: [255, 0, 128],
                    last_seen: 1_700_000_000,
                },
            ),
            (
                [0xAB; 16],
                PlayerRecord {
                    pos: Vec3::new(-42.5, 21.0, 100.5),
                    yaw: 0.0,
                    pitch: 0.0,
                    name: String::new(),
                    color: [0, 0, 0],
                    last_seen: 1_700_000_001,
                },
            ),
            (
                [7; 16],
                PlayerRecord {
                    pos: Vec3::new(1000.5, 5.0, -1000.5),
                    yaw: -3.1,
                    pitch: 1.5,
                    name: "Zoë-ñ".to_string(),
                    color: [1, 2, 3],
                    last_seen: 0,
                },
            ),
        ]
    }

    #[test]
    fn roundtrip() {
        let edits = sample_edits();
        let players = sample_players();
        let bytes = encode(1337, &edits, &players);
        let (seed, back, back_players) = decode(&bytes).expect("decode");
        assert_eq!(seed, 1337);
        assert_eq!(back, edits);
        assert_eq!(back_players, players);
    }

    #[test]
    fn roundtrip_empty() {
        let bytes = encode(42, &[], &[]);
        assert_eq!(bytes.len(), HEADER_BYTES + 8); // header + zero player count
        let (seed, back, players) = decode(&bytes).expect("decode");
        assert_eq!(seed, 42);
        assert!(back.is_empty());
        assert!(players.is_empty());
    }

    #[test]
    fn deterministic() {
        let edits = sample_edits();
        let players = sample_players();
        assert_eq!(
            encode(7, &edits, &players),
            encode(7, &edits, &players)
        );
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = encode(1, &[], &[]);
        bytes[0] = b'X';
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn rejects_unknown_version() {
        let mut bytes = encode(1, &[], &[]);
        bytes[4..8].copy_from_slice(&99u32.to_le_bytes());
        assert!(decode(&bytes).is_err());
        bytes[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert!(decode(&bytes).is_err());
    }

    /// Hand-build one player record (for corruption tests).
    fn player_bytes(
        token: [u8; 16],
        pos: Vec3,
        yaw: f32,
        pitch: f32,
        name: &[u8],
        color: [u8; 3],
        last_seen: u64,
    ) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&token);
        b.extend_from_slice(&pos.x.to_le_bytes());
        b.extend_from_slice(&pos.y.to_le_bytes());
        b.extend_from_slice(&pos.z.to_le_bytes());
        b.extend_from_slice(&yaw.to_le_bytes());
        b.extend_from_slice(&pitch.to_le_bytes());
        b.extend_from_slice(&(name.len() as u16).to_le_bytes());
        b.extend_from_slice(name);
        b.extend_from_slice(&color);
        b.extend_from_slice(&last_seen.to_le_bytes());
        b
    }

    /// A v2 header + `edits` + one player record (for corruption tests).
    fn save_with_player(name: &[u8], pos: Vec3) -> Vec<u8> {
        let mut out = encode(5, &[], &[]);
        // Replace the zero player count with one and append the record.
        out[24..32].copy_from_slice(&1u64.to_le_bytes());
        out.extend_from_slice(&player_bytes(
            [9; 16],
            pos,
            0.5,
            -0.25,
            name,
            [9, 8, 7],
            42,
        ));
        out
    }

    #[test]
    fn v1_save_decodes_without_players() {
        // Hand-build a v1 file: header (version 1) + one edit, nothing else.
        let mut v1 = Vec::new();
        v1.extend_from_slice(SAVE_MAGIC);
        v1.extend_from_slice(&1u32.to_le_bytes());
        v1.extend_from_slice(&11u64.to_le_bytes());
        v1.extend_from_slice(&1u64.to_le_bytes());
        v1.extend_from_slice(&8i32.to_le_bytes());
        v1.extend_from_slice(&30i32.to_le_bytes());
        v1.extend_from_slice(&8i32.to_le_bytes());
        v1.push(Block::Stone.as_u8());
        let (seed, edits, players) = decode(&v1).expect("v1 decodes");
        assert_eq!(seed, 11);
        assert_eq!(edits.len(), 1);
        assert!(players.is_empty());
        // A v1 file with trailing bytes is corrupt (a truncated v2 with a
        // downgraded version must not silently drop its identities).
        let mut bad = v1;
        bad.push(0);
        assert!(decode(&bad).is_err());
    }

    #[test]
    fn rejects_truncated_header_and_body() {
        let bytes = encode(1, &sample_edits(), &[]);
        assert!(decode(&bytes[..HEADER_BYTES - 1]).is_err());
        // Header intact, one entry short.
        assert!(decode(&bytes[..bytes.len() - 5]).is_err());
        // Count claims more entries than are present.
        let mut bad = encode(1, &[], &[]);
        bad[16..24].copy_from_slice(&5u64.to_le_bytes());
        assert!(decode(&bad).is_err());
    }

    #[test]
    fn rejects_out_of_world_y_and_unknown_block() {
        // v2 header with one bad edit entry and an empty player section.
        fn one_edit(x: i32, y: i32, z: i32, block: u8) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(SAVE_MAGIC);
            out.extend_from_slice(&SAVE_VERSION.to_le_bytes());
            out.extend_from_slice(&1u64.to_le_bytes()); // seed
            out.extend_from_slice(&1u64.to_le_bytes()); // one edit
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
            out.extend_from_slice(&z.to_le_bytes());
            out.push(block);
            out.extend_from_slice(&0u64.to_le_bytes()); // zero players
            out
        }
        // y = WORLD_HEIGHT (out of range).
        let bytes = one_edit(8, WORLD_HEIGHT, 8, Block::Stone.as_u8());
        assert!(decode(&bytes).is_err());
        // y = -1.
        let bytes = one_edit(8, -1, 8, Block::Stone.as_u8());
        assert!(decode(&bytes).is_err());
        // Unknown block id.
        let bytes = one_edit(8, 10, 8, BLOCKS.len() as u8);
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn rejects_truncated_player_section() {
        let bytes = save_with_player(b"Alice", Vec3::new(1.0, 2.0, 3.0));
        // Chop the tail of the record (name + colour + last_seen).
        assert!(decode(&bytes[..bytes.len() - 5]).is_err());
        // Player count present but no record at all.
        let mut no_rec = encode(5, &[], &[]);
        no_rec[24..32].copy_from_slice(&1u64.to_le_bytes());
        assert!(decode(&no_rec).is_err());
    }

    #[test]
    fn rejects_bad_player_records() {
        let pos = Vec3::new(1.0, 2.0, 3.0);
        // Non-UTF-8 name.
        assert!(decode(&save_with_player(b"\xFF\xFE", pos)).is_err());
        // Over-long name (65 bytes).
        assert!(decode(&save_with_player(&[b'a'; 65], pos)).is_err());
        // Non-finite position.
        let mut bytes = encode(5, &[], &[]);
        bytes[24..32].copy_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&player_bytes(
            [9; 16],
            Vec3::new(f32::NAN, 2.0, 3.0),
            0.0,
            0.0,
            b"x",
            [1, 2, 3],
            0,
        ));
        assert!(decode(&bytes).is_err());
        // Trailing bytes after the section.
        let mut bad = save_with_player(b"Alice", pos);
        bad.push(0);
        assert!(decode(&bad).is_err());
    }

    #[test]
    fn seed_mismatch_is_detectable() {
        let (a, _, _) = decode(&encode(111, &[], &[])).unwrap();
        let (b, _, _) = decode(&encode(222, &[], &[])).unwrap();
        assert_ne!(a, b);
    }
}
