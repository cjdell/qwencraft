//! Binary wire protocol shared by the headless server (`qwencraft-net`) and
//! the browser client.
//!
//! Frame layout (all little-endian):
//!
//! ```text
//! u8   message type
//! u32  payload length in bytes
//! ...  payload
//! ```
//!
//! Each frame is one self-contained binary WebSocket message, so there is no
//! inter-frame state. `decode_stream` additionally tolerates several frames
//! in one buffer (and leaves a partial trailing frame unconsumed), which
//! keeps the codec stream-safe if the transport framing ever changes.
//!
//! The codec is hand-rolled on purpose: this crate must stay dependency-free
//! (it also compiles to wasm for the client end), and the messages are tiny
//! fixed-layout values plus one large byte blob (chunk regions).

use crate::{Action, AgentState, ServerStats};
use qwencraft_world::{BlockPos, ChunkPos};

/// Protocol layout version. The server announces it in `ServerMsg::Hello`;
/// the client refuses to play against a mismatch. Bump on any layout change.
///
/// v2: `ResendChunk` (a single-chunk pull) became `Evicted` (a batch of
/// pool-eviction reports): the server's streamer forgets the reported
/// chunks and its normal stream re-sends the ones that are visible again,
/// rate-limited and nearest-first — the client no longer has to track and
/// re-request its own pool.
///
/// v3: `Hello` carries the connection's own `player_id` (so the client can
/// render the *other* players in the shared world without drawing its own
/// first-person sphere), `AgentState` carries a `name` (rendered as a tag
/// above other players' spheres), and clients send `Profile` (name +
/// sphere colour) so the shared world shows who is who.
pub const PROTOCOL_VERSION: u8 = 3;

// ---- client -> server message types --------------------------------------
const T_INPUT: u8 = 0x01;
const T_ACTION: u8 = 0x02;
const T_EVICTED: u8 = 0x03;
const T_SET_NPC_LOAD: u8 = 0x04;
const T_PROFILE: u8 = 0x05;

// ---- server -> client message types --------------------------------------
const T_HELLO: u8 = 0x10;
const T_PLAYER: u8 = 0x11;
const T_AGENTS: u8 = 0x12;
const T_CHUNK: u8 = 0x13;
const T_STATS: u8 = 0x14;
const T_NPC_LOAD: u8 = 0x15;

// ---- Action discriminants (payload of T_ACTION) ---------------------------
const A_BREAK: u8 = 0;
const A_PLACE: u8 = 1;
const A_TOGGLE_FLY: u8 = 2;
const A_FLY_FASTER: u8 = 3;
const A_FLY_SLOWER: u8 = 4;
const A_NPC_LOAD: u8 = 5;
const A_NPC_CLEAR: u8 = 6;
const A_NPC_COUNT_UP: u8 = 7;
const A_NPC_COUNT_DOWN: u8 = 8;
const A_NPC_SPACING_UP: u8 = 9;
const A_NPC_SPACING_DOWN: u8 = 10;

/// Messages the client sends to the server.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientMsg {
    /// Per-frame input snapshot: key bitmask + accumulated look deltas.
    Input {
        keys: u32,
        dx: f32,
        dy: f32,
    },
    /// One-shot action (break/place carry the click-time aim).
    Action(Action),
    /// The client's terrain pool evicted these chunks (pool compaction). The
    /// server's streamer forgets them and re-sends the ones that are visible
    /// again (its normal stream does this, nearest-first); without this the
    /// server would never re-send them and they would stay holes.
    Evicted(Vec<ChunkPos>),
    /// Set the NPC load-test dial (count, spacing) and spawn the load —
    /// the network form of `Server::set_npc_load` + `Action::NpcLoad`
    /// (armed via `?npcs=` for headless runs).
    SetNpcLoad { count: u32, spacing: f32 },
    /// This player's identity: display name + sphere colour. Sent on
    /// connect and on change; broadcast to everyone via the agent list so
    /// players can see each other (sphere + name tag).
    Profile { name: String, color: [u8; 3] },
}

/// Messages the server sends to the client.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerMsg {
    /// First message on the connection: protocol version + world seed +
    /// this connection's own player id (the client skips rendering it —
    /// first person — and renders every other player as a sphere).
    Hello { version: u8, seed: u64, player_id: u32 },
    /// The player's state (camera source of truth), every tick.
    PlayerState(AgentState),
    /// All agents (player first), every tick — the client renders them.
    Agents(Vec<AgentState>),
    /// A chunk region (re)appeared or changed.
    Chunk { pos: ChunkPos, data: Vec<u8> },
    /// HUD statistics, every tick.
    Stats(ServerStats),
    /// The NPC load dial changed (count, spacing).
    NpcLoad { count: u32, spacing: f32 },
}

impl ClientMsg {
    /// Encode as one complete wire frame.
    pub fn encode(&self) -> Vec<u8> {
        let mut p = Enc::new();
        let ty = match self {
            ClientMsg::Input { keys, dx, dy } => {
                p.u32(*keys);
                p.f32(*dx);
                p.f32(*dy);
                T_INPUT
            }
            ClientMsg::Action(a) => {
                match a {
                    Action::Break { yaw, pitch } => {
                        p.u8(A_BREAK);
                        p.f32(*yaw);
                        p.f32(*pitch);
                    }
                    Action::Place { yaw, pitch } => {
                        p.u8(A_PLACE);
                        p.f32(*yaw);
                        p.f32(*pitch);
                    }
                    Action::ToggleFly => p.u8(A_TOGGLE_FLY),
                    Action::FlyFaster => p.u8(A_FLY_FASTER),
                    Action::FlySlower => p.u8(A_FLY_SLOWER),
                    Action::NpcLoad => p.u8(A_NPC_LOAD),
                    Action::NpcClear => p.u8(A_NPC_CLEAR),
                    Action::NpcCountUp => p.u8(A_NPC_COUNT_UP),
                    Action::NpcCountDown => p.u8(A_NPC_COUNT_DOWN),
                    Action::NpcSpacingUp => p.u8(A_NPC_SPACING_UP),
                    Action::NpcSpacingDown => p.u8(A_NPC_SPACING_DOWN),
                }
                T_ACTION
            }
            ClientMsg::Evicted(v) => {
                p.u32(v.len() as u32);
                for c in v {
                    p.i32(c.x);
                    p.i32(c.y);
                    p.i32(c.z);
                }
                T_EVICTED
            }
            ClientMsg::SetNpcLoad { count, spacing } => {
                p.u32(*count);
                p.f32(*spacing);
                T_SET_NPC_LOAD
            }
            ClientMsg::Profile { name, color } => {
                p.u16(name.len() as u16);
                p.bytes(name.as_bytes());
                for c in *color {
                    p.u8(c);
                }
                T_PROFILE
            }
        };
        p.frame(ty)
    }

    /// Decode one complete wire frame.
    pub fn decode(frame: &[u8]) -> Option<Self> {
        let (ty, payload) = frame_header(frame)?;
        let mut d = Dec::new(payload);
        let msg = match ty {
            T_INPUT => {
                let keys = d.u32()?;
                let dx = d.f32()?;
                let dy = d.f32()?;
                ClientMsg::Input { keys, dx, dy }
            }
            T_ACTION => {
                let kind = d.u8()?;
                let a = match kind {
                    A_BREAK => Action::Break {
                        yaw: d.f32()?,
                        pitch: d.f32()?,
                    },
                    A_PLACE => Action::Place {
                        yaw: d.f32()?,
                        pitch: d.f32()?,
                    },
                    A_TOGGLE_FLY => Action::ToggleFly,
                    A_FLY_FASTER => Action::FlyFaster,
                    A_FLY_SLOWER => Action::FlySlower,
                    A_NPC_LOAD => Action::NpcLoad,
                    A_NPC_CLEAR => Action::NpcClear,
                    A_NPC_COUNT_UP => Action::NpcCountUp,
                    A_NPC_COUNT_DOWN => Action::NpcCountDown,
                    A_NPC_SPACING_UP => Action::NpcSpacingUp,
                    A_NPC_SPACING_DOWN => Action::NpcSpacingDown,
                    _ => return None,
                };
                if !d.exhausted() {
                    return None;
                }
                ClientMsg::Action(a)
            }
            T_EVICTED => {
                let n = d.u32()? as usize;
                if n > 65536 {
                    return None; // implausible eviction batch
                }
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    let x = d.i32()?;
                    let y = d.i32()?;
                    let z = d.i32()?;
                    v.push(ChunkPos::new(x, y, z));
                }
                ClientMsg::Evicted(v)
            }
            T_SET_NPC_LOAD => {
                let count = d.u32()?;
                let spacing = d.f32()?;
                ClientMsg::SetNpcLoad { count, spacing }
            }
            T_PROFILE => {
                let len = d.u16()? as usize;
                if len > 64 {
                    return None; // names are clamped far below this
                }
                let name = String::from_utf8(d.bytes(len)?.to_vec()).ok()?;
                let color = [d.u8()?, d.u8()?, d.u8()?];
                ClientMsg::Profile { name, color }
            }
            _ => return None,
        };
        if !d.exhausted() {
            return None;
        }
        Some(msg)
    }

    /// Decode all complete frames in `buf`. Returns the messages and the
    /// number of bytes consumed (a partial trailing frame is left over).
    pub fn decode_stream(buf: &[u8]) -> (Vec<Self>, usize) {
        let mut out = Vec::new();
        let mut off = 0usize;
        while let Some((len, total)) = frame_len(buf, off) {
            let msg = Self::decode(&buf[off..off + total]);
            off += total;
            match msg {
                Some(m) => out.push(m),
                None => {
                    // Unknown/corrupt frame: skip it, keep the stream alive.
                }
            }
            let _ = len;
        }
        (out, off)
    }
}

impl ServerMsg {
    /// Encode as one complete wire frame.
    pub fn encode(&self) -> Vec<u8> {
        let mut p = Enc::new();
        let ty = match self {
            ServerMsg::Hello { version, seed, player_id } => {
                p.u8(*version);
                p.u64(*seed);
                p.u32(*player_id);
                T_HELLO
            }
            ServerMsg::PlayerState(s) => {
                encode_agent(&mut p, s);
                T_PLAYER
            }
            ServerMsg::Agents(v) => {
                p.u32(v.len() as u32);
                for s in v {
                    encode_agent(&mut p, s);
                }
                T_AGENTS
            }
            ServerMsg::Chunk { pos, data } => {
                p.i32(pos.x);
                p.i32(pos.y);
                p.i32(pos.z);
                p.u32(data.len() as u32);
                p.bytes(data);
                T_CHUNK
            }
            ServerMsg::Stats(s) => {
                p.u32(s.chunks_generated as u32);
                p.u32(s.chunks_sent as u32);
                p.u32(s.deltas as u32);
                p.u32(s.agents as u32);
                p.u32(s.npcs as u32);
                p.u64(s.cache.lookups);
                p.u64(s.cache.hits);
                p.u64(s.cache.solid_misses);
                p.u64(s.cache.rebuilds);
                p.u64(s.cache.rebuild_probes);
                T_STATS
            }
            ServerMsg::NpcLoad { count, spacing } => {
                p.u32(*count);
                p.f32(*spacing);
                T_NPC_LOAD
            }
        };
        p.frame(ty)
    }

    /// Decode one complete wire frame.
    pub fn decode(frame: &[u8]) -> Option<Self> {
        let (ty, payload) = frame_header(frame)?;
        let mut d = Dec::new(payload);
        let msg = match ty {
            T_HELLO => {
                let version = d.u8()?;
                let seed = d.u64()?;
                let player_id = d.u32()?;
                ServerMsg::Hello { version, seed, player_id }
            }
            T_PLAYER => {
                let s = decode_agent(&mut d)?;
                ServerMsg::PlayerState(s)
            }
            T_AGENTS => {
                let n = d.u32()? as usize;
                let mut v = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    v.push(decode_agent(&mut d)?);
                }
                ServerMsg::Agents(v)
            }
            T_CHUNK => {
                let x = d.i32()?;
                let y = d.i32()?;
                let z = d.i32()?;
                let len = d.u32()? as usize;
                let data = d.bytes(len)?.to_vec();
                ServerMsg::Chunk {
                    pos: ChunkPos::new(x, y, z),
                    data,
                }
            }
            T_STATS => {
                let chunks_generated = d.u32()? as usize;
                let chunks_sent = d.u32()? as usize;
                let deltas = d.u32()? as usize;
                let agents = d.u32()? as usize;
                let npcs = d.u32()? as usize;
                let lookups = d.u64()?;
                let hits = d.u64()?;
                let solid_misses = d.u64()?;
                let rebuilds = d.u64()?;
                let rebuild_probes = d.u64()?;
                ServerMsg::Stats(ServerStats {
                    chunks_generated,
                    chunks_sent,
                    deltas,
                    agents,
                    npcs,
                    cache: crate::CacheStats {
                        lookups,
                        hits,
                        solid_misses,
                        rebuilds,
                        rebuild_probes,
                    },
                })
            }
            T_NPC_LOAD => {
                let count = d.u32()?;
                let spacing = d.f32()?;
                ServerMsg::NpcLoad { count, spacing }
            }
            _ => return None,
        };
        if !d.exhausted() {
            return None;
        }
        Some(msg)
    }

    /// Decode all complete frames in `buf` (see `ClientMsg::decode_stream`).
    pub fn decode_stream(buf: &[u8]) -> (Vec<Self>, usize) {
        let mut out = Vec::new();
        let mut off = 0usize;
        while let Some((_, total)) = frame_len(buf, off) {
            let msg = Self::decode(&buf[off..off + total]);
            off += total;
            if let Some(m) = msg {
                out.push(m);
            }
        }
        (out, off)
    }
}

/// One agent's state (shared by PlayerState and Agents).
fn encode_agent(p: &mut Enc, s: &AgentState) {
    p.u32(s.id);
    let mut flags = 0u8;
    if s.is_player {
        flags |= 0x1;
    }
    if s.on_ground {
        flags |= 0x2;
    }
    if s.fly {
        flags |= 0x4;
    }
    p.u8(flags);
    p.f32(s.pos.x);
    p.f32(s.pos.y);
    p.f32(s.pos.z);
    p.f32(s.yaw);
    p.f32(s.pitch);
    p.f32(s.fly_speed);
    for c in s.color {
        p.u8(c);
    }
    p.f32(s.radius);
    match s.target {
        Some(t) => {
            p.u8(1);
            p.i32(t.x);
            p.i32(t.y);
            p.i32(t.z);
        }
        None => p.u8(0),
    }
    p.u16(s.name.len() as u16);
    p.bytes(s.name.as_bytes());
}

fn decode_agent(d: &mut Dec) -> Option<AgentState> {
    let id = d.u32()?;
    let flags = d.u8()?;
    let pos = crate::Vec3::new(d.f32()?, d.f32()?, d.f32()?);
    let yaw = d.f32()?;
    let pitch = d.f32()?;
    let fly_speed = d.f32()?;
    let color = [d.u8()?, d.u8()?, d.u8()?];
    let radius = d.f32()?;
    let target = if d.u8()? == 1 {
        Some(BlockPos::new(d.i32()?, d.i32()?, d.i32()?))
    } else {
        None
    };
    let name_len = d.u16()? as usize;
    if name_len > 64 {
        return None; // names are clamped far below this
    }
    let name = String::from_utf8(d.bytes(name_len)?.to_vec()).ok()?;
    Some(AgentState {
        id,
        is_player: flags & 0x1 != 0,
        name,
        pos,
        yaw,
        pitch,
        on_ground: flags & 0x2 != 0,
        fly: flags & 0x4 != 0,
        fly_speed,
        color,
        radius,
        target,
    })
}

/// Split a complete frame into (type, payload).
fn frame_header(frame: &[u8]) -> Option<(u8, &[u8])> {
    let hdr = frame.get(..5)?;
    let ty = hdr[0];
    let len = u32::from_le_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    let payload = frame.get(5..5 + len)?;
    Some((ty, payload))
}

/// Total size of the frame starting at `off`, if a complete one fits.
fn frame_len(buf: &[u8], off: usize) -> Option<(usize, usize)> {
    let hdr = buf.get(off..off + 5)?;
    let len = u32::from_le_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]) as usize;
    let total = 5 + len;
    buf.get(off..off + total)?;
    Some((len, total))
}

/// Little-endian payload builder.
struct Enc(Vec<u8>);

impl Enc {
    fn new() -> Self {
        Self(Vec::with_capacity(64))
    }
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    /// Seal as a complete frame: type + length + payload.
    fn frame(self, ty: u8) -> Vec<u8> {
        let mut out = Vec::with_capacity(5 + self.0.len());
        out.push(ty);
        out.extend_from_slice(&(self.0.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.0);
        out
    }
}

/// Little-endian cursor over a payload.
struct Dec<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Dec<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }
    fn exhausted(&self) -> bool {
        self.i == self.b.len()
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.i..self.i + n)?;
        self.i += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(*self.take(1)?.first()?)
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        self.take(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Key, KeySet};
    use qwencraft_world::BlockPos;

    /// An agent with every field non-default (all flags set, target present).
    fn full_agent() -> AgentState {
        AgentState {
            id: 7,
            is_player: true,
            name: "Zörg".to_string(),
            pos: crate::Vec3::new(-12.5, 34.25, 99.125),
            yaw: 1.23,
            pitch: -0.45,
            on_ground: true,
            fly: true,
            fly_speed: 42.0,
            color: [10, 200, 255],
            radius: 0.42,
            target: Some(BlockPos::new(-3, 12, 77)),
        }
    }

    #[test]
    fn client_messages_round_trip() {
        let mut keys = KeySet::default();
        keys.insert(Key::W);
        keys.insert(Key::Space);
        keys.insert(Key::BracketRight);
        let msgs = [
            ClientMsg::Input {
                keys: keys.bits(),
                dx: -12.5,
                dy: 3.25,
            },
            ClientMsg::Action(Action::Break {
                yaw: 0.5,
                pitch: -0.7,
            }),
            ClientMsg::Action(Action::Place {
                yaw: -1.5,
                pitch: 1.55,
            }),
            ClientMsg::Action(Action::ToggleFly),
            ClientMsg::Action(Action::FlyFaster),
            ClientMsg::Action(Action::FlySlower),
            ClientMsg::Action(Action::NpcLoad),
            ClientMsg::Action(Action::NpcClear),
            ClientMsg::Action(Action::NpcCountUp),
            ClientMsg::Action(Action::NpcCountDown),
            ClientMsg::Action(Action::NpcSpacingUp),
            ClientMsg::Action(Action::NpcSpacingDown),
            ClientMsg::Evicted(vec![
                ChunkPos::new(-4, 2, 17),
                ChunkPos::new(0, 0, -1),
                ChunkPos::new(123, 1, -50),
            ]),
            ClientMsg::SetNpcLoad {
                count: 128,
                spacing: 24.0,
            },
            ClientMsg::Profile {
                name: "Alice".to_string(),
                color: [10, 200, 255],
            },
            ClientMsg::Profile {
                name: String::new(),
                color: [0, 0, 0],
            },
        ];
        for m in &msgs {
            let enc = m.encode();
            let dec = ClientMsg::decode(&enc).unwrap_or_else(|| panic!("decode failed for {m:?}"));
            assert_eq!(&dec, m, "round-trip mismatch for {m:?}");
        }
    }

    #[test]
    fn server_messages_round_trip() {
        let agent = full_agent();
        let chunk_data: Vec<u8> = (0..17576).map(|i| (i % 11) as u8).collect();
        let stats = ServerStats {
            chunks_generated: 500,
            chunks_sent: 420,
            deltas: 3,
            agents: 65,
            npcs: 64,
            cache: crate::CacheStats {
                lookups: 1_000_003,
                hits: 999_999,
                solid_misses: 7,
                rebuilds: 4242,
                rebuild_probes: 1_400_000,
            },
        };
        let msgs = [
            ServerMsg::Hello {
                version: PROTOCOL_VERSION,
                seed: 0xDEAD_BEEF_CAFE_F00D,
                player_id: 3,
            },
            ServerMsg::PlayerState(agent.clone()),
            ServerMsg::Agents(vec![agent.clone(), AgentState::default()]),
            ServerMsg::Chunk {
                pos: ChunkPos::new(-2, 1, 3),
                data: chunk_data,
            },
            ServerMsg::Stats(stats),
            ServerMsg::NpcLoad {
                count: 64,
                spacing: 16.0,
            },
        ];
        for m in &msgs {
            let enc = m.encode();
            let dec = ServerMsg::decode(&enc).unwrap_or_else(|| panic!("decode failed for {m:?}"));
            assert_eq!(dec, *m, "round-trip mismatch for {m:?}");
        }
    }

    #[test]
    fn agent_round_trips_with_and_without_target() {
        let mut no_target = full_agent();
        no_target.target = None;
        no_target.is_player = false;
        no_target.on_ground = false;
        no_target.fly = false;
        for s in [full_agent(), no_target] {
            let enc = ServerMsg::PlayerState(s.clone()).encode();
            let ServerMsg::PlayerState(d) = ServerMsg::decode(&enc).unwrap() else {
                panic!("wrong message type");
            };
            assert_eq!(d, s);
        }
    }

    #[test]
    fn decode_stream_handles_multiple_and_partial_frames() {
        let m1 = ClientMsg::Action(Action::ToggleFly);
        let m2 = ClientMsg::Evicted(vec![ChunkPos::new(1, 0, -2)]);
        let mut buf = m1.encode();
        buf.extend_from_slice(&m2.encode());
        // Append a partial third frame: it must be left unconsumed.
        let partial = ClientMsg::Action(Action::FlyFaster).encode();
        buf.extend_from_slice(&partial[..3]);

        let (msgs, consumed) = ClientMsg::decode_stream(&buf);
        assert_eq!(consumed, m1.encode().len() + m2.encode().len());
        assert_eq!(
            msgs,
            vec![ClientMsg::Action(Action::ToggleFly), m2],
            "two complete frames decoded"
        );
        assert_eq!(&buf[consumed..], &partial[..3], "partial frame left over");

        // Server side: same stream semantics.
        let s1 = ServerMsg::NpcLoad {
            count: 2,
            spacing: 8.0,
        };
        let s2 = ServerMsg::Hello {
            version: 1,
            seed: 9,
            player_id: 0,
        };
        let mut sbuf = s1.encode();
        sbuf.extend_from_slice(&s2.encode());
        let (smsgs, sconsumed) = ServerMsg::decode_stream(&sbuf);
        assert_eq!(sconsumed, sbuf.len());
        assert_eq!(smsgs, vec![s1, s2]);
    }

    #[test]
    fn decode_rejects_garbage() {
        // Unknown type.
        assert!(ClientMsg::decode(&[0x7F, 0, 0, 0, 0]).is_none());
        assert!(ServerMsg::decode(&[0x01, 0, 0, 0, 0]).is_none());
        // Truncated header.
        assert!(ClientMsg::decode(&[0x01, 0, 0]).is_none());
        // Length beyond the frame.
        assert!(ClientMsg::decode(&[0x01, 9, 0, 0, 0, 1, 2]).is_none());
        // Right length, wrong content (truncated payload).
        assert!(ClientMsg::decode(&[0x01, 12, 0, 0, 0, 1, 2, 3]).is_none());
        // Trailing garbage after a valid action payload.
        let mut bad = ClientMsg::Action(Action::NpcClear).encode();
        bad.extend_from_slice(&[1, 2, 3]);
        bad[1] = 4; // lie about the length to swallow the garbage
        assert!(ClientMsg::decode(&bad).is_none());
        // Unknown action discriminant.
        let bad = [0x02u8, 1, 0, 0, 0, 99];
        assert!(ClientMsg::decode(&bad).is_none());
    }
}
