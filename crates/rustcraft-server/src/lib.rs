//! RustCraft game server.
//!
//! Transport-agnostic: the server exposes a small command/query API that the
//! embedded browser client uses directly (and that a real network server could
//! serve over a socket later).
//!
//! Key properties:
//! - Infinite world, chunks generated lazily from a seed (never all at once).
//! - World edits are stored as deltas and applied to chunks when they exist.
//! - Each agent keeps a dense local block window (a small 3D volume of the
//!   entire world around it) so steady-state physics lookups are served from
//!   the window and never touch the full chunk buffers.

pub mod agent;
pub mod local_block_cache;
pub mod protocol;
pub mod sphere;
pub mod world;

mod input;

pub use agent::{
    Agent, AgentKind, AgentState, FLY_BASE_SPEED, FLY_MAX_SPEED, FLY_MIN_SPEED, FLY_STEP,
};
pub use local_block_cache::{CacheStats, LocalBlockCache};
pub use sphere::sphere_mesh;
pub use input::{Action, Input, Key, KeySet};
pub use world::{Edit, World, WorldUpdate};

use std::sync::Arc;

use rustcraft_world::{Block, BlockPos, ChunkPos, WORLD_HEIGHT};

/// Fixed physics tick rate.
pub const TICK_HZ: f32 = 60.0;
const TICK_DT: f64 = 1.0 / 60.0;

/// XZ streaming radius in chunks.
pub const VIEW_RADIUS: i32 = 7;

/// NPC load-test limits (NpcCountUp/Down clamp into these ranges).
pub const NPC_COUNT_MIN: u32 = 1;
pub const NPC_COUNT_MAX: u32 = 2048;
pub const NPC_SPACING_MIN: f32 = 4.0;
pub const NPC_SPACING_MAX: f32 = 128.0;
/// Default NPC load: count and spacing (blocks between spiral arms).
pub const NPC_COUNT_DEFAULT: u32 = 64;
pub const NPC_SPACING_DEFAULT: f32 = 16.0;
/// Golden angle (radians): successive phyllotaxis points are ~spacing apart.
const GOLDEN_ANGLE: f32 = 2.3999632;
/// Max region payloads emitted per tick.
const STREAM_PER_TICK: usize = 4;
/// Max chunks generated per tick by the streamer.
const GEN_PER_TICK: usize = 6;

/// The game server. Owns the world and every agent (any number of players
/// plus NPCs). Per-player state (input snapshot, queued actions, crosshair
/// target) is keyed by agent id, and per-viewer chunk streaming lives in a
/// [`Streamer`], so one `Server` can be the shared world behind any number
/// of clients.
pub struct Server {
    seed: u64,
    world: World,
    agents: Vec<Agent>,
    /// Current input snapshot per player agent (level-triggered: the latest
    /// snapshot is applied every tick until replaced).
    inputs: std::collections::HashMap<u32, Input>,
    /// Previous input snapshot per player agent (jump edge detection).
    prev_inputs: std::collections::HashMap<u32, Input>,
    /// One-shot actions queued per player agent (applied on the next tick).
    actions: std::collections::HashMap<u32, Vec<Action>>,
    /// The block under each player's crosshair (recomputed every tick);
    /// the client draws a wireframe highlight around it.
    targets: std::collections::HashMap<u32, Option<BlockPos>>,
    /// Chunks dirtied by edits since the last `drain_dirty`; each viewer
    /// that already holds the chunk gets a region resend for it.
    dirty: Vec<ChunkPos>,
    /// Next agent id to allocate (players and NPCs share the id space).
    next_id: u32,
    acc: f64,
    time: f64,
    /// Configured NPC load (applied by Action::NpcLoad): how many NPCs to
    /// spawn and how far apart the spiral arms sit. Load-test facility.
    npc_count: u32,
    npc_spacing: f32,
    /// Optional sink for human-readable event strings (the network server
    /// feeds its dashboard event log from this). `None` in the built-in
    /// client, so events cost nothing there.
    event_sink: Option<Arc<dyn Fn(&str) + Send + Sync>>,
}

impl Server {
    /// Create a world with the given seed and no agents. Network servers use
    /// this and add one player per connection ([`Self::add_player`]).
    pub fn new_world(seed: u64) -> Self {
        Server {
            seed,
            world: World::new(seed),
            agents: Vec::new(),
            inputs: std::collections::HashMap::new(),
            prev_inputs: std::collections::HashMap::new(),
            actions: std::collections::HashMap::new(),
            targets: std::collections::HashMap::new(),
            dirty: Vec::new(),
            next_id: 0,
            acc: 0.0,
            time: 0.0,
            npc_count: NPC_COUNT_DEFAULT,
            npc_spacing: NPC_SPACING_DEFAULT,
            event_sink: None,
        }
    }

    /// Set the event sink (see the `event_sink` field). The network server
    /// uses this to feed its dashboard's event log; the built-in client
    /// never sets one.
    pub fn set_event_sink(&mut self, sink: Option<Arc<dyn Fn(&str) + Send + Sync>>) {
        self.event_sink = sink;
    }

    /// Forward one event string to the sink (no-op when unset).
    fn emit(&self, msg: String) {
        if let Some(s) = &self.event_sink {
            s(&msg);
        }
    }

    /// Create a single-player world: one player plus a few ambient NPCs.
    pub fn new(seed: u64) -> Self {
        let mut s = Self::new_world(seed);
        s.add_player();
        // A couple of wandering NPC agents (also on dry, tree-free ground).
        for (dx, dz) in [(3, -4), (-5, 3), (2, 6)] {
            let (x, z) = Self::find_spawn(&s.world, 8 + dx, 8 + dz, 6);
            let h = s.world.height_at(x, z);
            let id = s.next_id;
            s.next_id += 1;
            s.agents.push(Agent::npc(
                id,
                Vec3::new(x as f32 + 0.5, (h + 1) as f32, z as f32 + 0.5),
            ));
        }
        s
    }

    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Borrow the world (read-only).
    pub fn world(&self) -> &World {
        &self.world
    }

    /// Borrow the world (mutable).
    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// Add a player agent at the spawn point and return its id. Successive
    /// players are nudged apart so they don't spawn in the same cell.
    pub fn add_player(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        // Spawn near the origin on dry, tree-free grass (skip water, snow
        // and tree columns so nobody starts underwater or in a trunk).
        let (sx, sz) = Self::find_spawn(&self.world, 8, 8, 16);
        let surface = self.world.height_at(sx, sz);
        let k = self.agents.iter().filter(|a| a.kind == AgentKind::Player).count() as f32;
        let off = k * 1.6;
        let pos = Vec3::new(sx as f32 + 0.5 + off, (surface + 2) as f32, sz as f32 + 0.5);
        self.agents.push(Agent::player(id, pos));
        self.inputs.insert(id, Input::default());
        self.prev_inputs.insert(id, Input::default());
        id
    }

    /// Remove a player agent (and its per-player state). NPCs are kept.
    pub fn remove_player(&mut self, id: u32) {
        if let Some(idx) = self.agents.iter().position(|a| a.id == id) {
            self.agents.remove(idx);
        }
        self.inputs.remove(&id);
        self.prev_inputs.remove(&id);
        self.actions.remove(&id);
        self.targets.remove(&id);
    }

    /// The ids of all live player agents, ascending.
    pub fn player_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self
            .agents
            .iter()
            .filter(|a| a.kind == AgentKind::Player)
            .map(|a| a.id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Replace player `id`'s current input snapshot.
    pub fn set_agent_input(&mut self, id: u32, input: Input) {
        self.inputs.insert(id, input);
    }

    /// Queue a one-shot action for player `id` (applied on the next tick).
    pub fn push_agent_action(&mut self, id: u32, a: Action) {
        self.actions.entry(id).or_default().push(a);
    }

    /// Snapshot of player `id` (crosshair target included).
    pub fn agent_state(&self, id: u32) -> AgentState {
        let idx = self.agent_index(id);
        let mut s = self.agents[idx].state();
        s.target = self.targets.get(&id).copied().flatten();
        s
    }

    /// Index of the agent with `id` (panics if absent — a caller bug).
    fn agent_index(&self, id: u32) -> usize {
        self.agents
            .iter()
            .position(|a| a.id == id)
            .expect("agent id must exist")
    }

    /// True when an agent can stand on column (x, z): dry grass (no water,
    /// no snow) with no tree trunk at the surface.
    fn spawnable(world: &World, x: i32, z: i32) -> bool {
        let h = world.height_at(x, z);
        h >= rustcraft_world::SEA_LEVEL
            && h < rustcraft_world::SNOW_LEVEL
            && world.tree_at(x, z).is_none()
    }

    /// Find a spawnable column on concentric rings around (cx, cz).
    fn find_spawn(world: &World, cx: i32, cz: i32, max_r: i32) -> (i32, i32) {
        for r in 0..=max_r {
            if r == 0 {
                if Self::spawnable(world, cx, cz) {
                    return (cx, cz);
                }
                continue;
            }
            for x in (cx - r)..=(cx + r) {
                for z in (cz - r)..=(cz + r) {
                    // Ring (Chebyshev) only — inner cells were checked already.
                    if x.abs_diff(cx).max(z.abs_diff(cz)) != r as u32 {
                        continue;
                    }
                    if Self::spawnable(world, x, z) {
                        return (x, z);
                    }
                }
            }
        }
        (cx, cz) // fallback (very unlikely: the map is mostly dry grass)
    }

    /// Queue a one-shot action (break/place) for the primary player (id 0).
    pub fn push_action(&mut self, a: Action) {
        self.push_agent_action(0, a);
    }

    /// Replace the current input state (keys + accumulated mouse deltas) for
    /// the primary player (id 0).
    pub fn set_input(&mut self, input: Input) {
        self.set_agent_input(0, input);
    }

    /// Advance simulation by real time `dt` seconds (clamped) using fixed
    /// steps. Chunk streaming is not done here: each viewer's [`Streamer`]
    /// drives it around its own player (see `Streamer::tick`).
    pub fn tick(&mut self, dt: f64) {
        self.acc += dt.min(0.25);
        while self.acc >= TICK_DT {
            self.acc -= TICK_DT;
            self.step(TICK_DT as f32);
        }
    }

    fn step(&mut self, dt: f32) {
        // Players (stable ascending-id order).
        let ids = self.player_ids();
        for &id in &ids {
            let input = self.inputs.get(&id).cloned().unwrap_or_default();
            let prev = self.prev_inputs.get(&id).cloned().unwrap_or_default();
            // Jump edge detection.
            let jump = input.keys.contains(Key::Space) && !prev.keys.contains(Key::Space);
            let idx = self.agent_index(id);
            let move_dir = input.move_direction(self.agents[idx].yaw);
            self.agents[idx].step(dt, &mut self.world, move_dir, jump, &input);
            // Consume this player's queued actions against the world.
            if let Some(actions) = self.actions.remove(&id) {
                for action in actions {
                    self.apply_action(id, action);
                }
            }
            // Record this tick's input for the next tick's jump edge, and
            // consume the look deltas (they are applied exactly once).
            self.prev_inputs.insert(id, input);
            if let Some(inp) = self.inputs.get_mut(&id) {
                inp.mouse_dx = 0.0;
                inp.mouse_dy = 0.0;
            }
        }

        // Recompute the crosshair target for each player (same raycast +
        // range as break/place, from the post-step aim — the same aim the
        // client is about to render).
        for &id in &ids {
            let idx = self.agent_index(id);
            self.targets.insert(
                id,
                self.world
                    .raycast(&self.agents[idx].eye(), &self.agents[idx].look_direction(), 6.0)
                    .map(|(hit, _)| hit),
            );
        }

        // NPCs.
        for npc in self.agents.iter_mut().filter(|a| a.kind == AgentKind::Npc) {
            npc.step_npc(dt, &mut self.world, self.time);
        }

        self.time += TICK_DT;
    }

    /// Apply a one-shot action for player `id`.
    fn apply_action(&mut self, id: u32, action: Action) {
        match action {
            Action::ToggleFly => {
                let idx = self.agent_index(id);
                self.agents[idx].toggle_fly();
                self.emit(format!(
                    "player {id} switched to {} mode",
                    if self.agents[idx].fly { "fly" } else { "walk" }
                ));
            }
            Action::FlyFaster => {
                let idx = self.agent_index(id);
                self.agents[idx].adjust_fly_speed(FLY_STEP);
            }
            Action::FlySlower => {
                let idx = self.agent_index(id);
                self.agents[idx].adjust_fly_speed(1.0 / FLY_STEP);
            }
            Action::NpcLoad => self.spawn_npcs(id),
            Action::NpcClear => self.clear_npcs(),
            Action::NpcCountUp => self.adjust_npc_count(true),
            Action::NpcCountDown => self.adjust_npc_count(false),
            Action::NpcSpacingUp => self.adjust_npc_spacing(true),
            Action::NpcSpacingDown => self.adjust_npc_spacing(false),
            Action::Break { .. } | Action::Place { .. } => {
                self.apply_world_edit(id, action)
            }
        }
    }

    /// Break/place: raycast from the acting player with the aim stamped into
    /// the action (the camera at click time), so post-click mouse movement
    /// can't move the target.
    fn apply_world_edit(&mut self, id: u32, action: Action) {
        let (eye, yaw, pitch) = match action {
            Action::Break { yaw, pitch } | Action::Place { yaw, pitch } => {
                let idx = self.agent_index(id);
                let p = &self.agents[idx];
                (p.eye(), yaw, pitch)
            }
            _ => unreachable!("only Break/Place reach apply_world_edit"),
        };
        let d = rustcraft_world::camera::look_direction(yaw, pitch);
        let dir = Vec3::new(d[0], d[1], d[2]);
        match self.world.raycast(&eye, &dir, 6.0) {
            Some((hit, prev)) => match action {
                Action::Break { .. } => {
                    if hit.y > 0 {
                        let removed = self.world.block_at(hit);
                        // World edits go through the delta layer.
                        let dirty = self.world.set_block(hit, Block::Air);
                        self.invalidate_caches_at(hit);
                        self.dirty.extend(dirty);
                        self.emit(format!(
                            "player {id} broke {:?} at ({}, {}, {})",
                            removed, hit.x, hit.y, hit.z
                        ));
                    }
                }
                Action::Place { .. } => {
                    // Don't place inside an agent.
                    let blocked = self.agents.iter().any(|a| {
                        let p = a.pos;
                        (prev.x as f32) < p.x + 0.3
                            && prev.x as f32 + 1.0 >= p.x - 0.3
                            && (prev.y as f32) < p.y + 1.8
                            && prev.y as f32 + 1.0 >= p.y
                            && (prev.z as f32) < p.z + 0.3
                            && prev.z as f32 + 1.0 >= p.z - 0.3
                    });
                    if prev.y >= 0 && prev.y < WORLD_HEIGHT && !blocked {
                        let dirty = self.world.set_block(prev, Block::Stone);
                        self.invalidate_caches_at(prev);
                        self.dirty.extend(dirty);
                        self.emit(format!(
                            "player {id} placed Stone at ({}, {}, {})",
                            prev.x, prev.y, prev.z
                        ));
                    }
                }
                // Fly/NPC actions never reach here (handled in apply_action).
                _ => unreachable!("only Break/Place reach apply_world_edit"),
            },
            None => {}
        }
    }

    /// Chunks dirtied by world edits since the last call. Each viewer that
    /// already holds one of these chunks gets a region resend for it
    /// (`Streamer::apply_edits`); new viewers get the current data when the
    /// chunk is first streamed to them.
    pub fn drain_dirty(&mut self) -> Vec<ChunkPos> {
        std::mem::take(&mut self.dirty)
    }

    /// The configured NPC load (count, spacing in blocks).
    pub fn npc_load_config(&self) -> (u32, f32) {
        (self.npc_count, self.npc_spacing)
    }

    /// Set the configured NPC load (clamped to the load-test limits).
    pub fn set_npc_load(&mut self, count: u32, spacing: f32) {
        self.npc_count = count.clamp(NPC_COUNT_MIN, NPC_COUNT_MAX);
        self.npc_spacing = spacing.clamp(NPC_SPACING_MIN, NPC_SPACING_MAX);
    }

    /// Double/halve the configured NPC count (clamped).
    pub fn adjust_npc_count(&mut self, up: bool) {
        self.npc_count = if up {
            (self.npc_count * 2).min(NPC_COUNT_MAX)
        } else {
            (self.npc_count / 2).max(NPC_COUNT_MIN)
        };
    }

    /// Double/halve the configured NPC spacing (clamped).
    pub fn adjust_npc_spacing(&mut self, up: bool) {
        self.npc_spacing = if up {
            (self.npc_spacing * 2.0).min(NPC_SPACING_MAX)
        } else {
            (self.npc_spacing / 2.0).max(NPC_SPACING_MIN)
        };
    }

    /// Spawn the configured NPC load around the player, replacing the
    /// existing NPCs so the count is exact.
    ///
    /// Layout: a phyllotaxis (sunflower) spiral centred on the player —
    /// point *i* sits at radius `spacing * sqrt(i+1)` and angle
    /// `i * golden_angle`, so neighbours are ~`spacing` blocks apart and
    /// the cloud grows outward evenly (outer radius ≈ `spacing * sqrt(count)`).
    /// Each target snaps to the nearest dry, tree-free column (small halo),
    /// and every NPC gets a fresh local block window. Spawning is one-shot
    /// (a brief frame hitch at high counts: each window's first build
    /// materialises its chunks on demand).
    pub fn spawn_npcs(&mut self, around: u32) {
        let (count, spacing) = self.npc_load_config();
        let idx = self.agent_index(around);
        let p = self.agents[idx].pos;
        self.agents
            .retain(|a| a.kind == AgentKind::Player); // keep players; replace the NPC set
        for i in 0..count {
            let angle = (i as f32) * GOLDEN_ANGLE;
            let radius = spacing * (i as f32 + 1.0).sqrt();
            let tx = (p.x + radius * angle.sin()) as i32;
            let tz = (p.z + radius * angle.cos()) as i32;
            let (x, z) = Self::find_spawn(&self.world, tx, tz, 2);
            let h = self.world.height_at(x, z);
            let id = self.next_id;
            self.next_id += 1;
            self.agents.push(Agent::npc(
                id,
                Vec3::new(x as f32 + 0.5, (h + 1) as f32, z as f32 + 0.5),
            ));
        }
        self.reset_cache_stats();
        self.emit(format!(
            "spawned {count} NPCs (spacing {spacing:.0}) around player {around}"
        ));
    }

    /// Remove all NPCs (every player remains).
    pub fn clear_npcs(&mut self) {
        let n = self.agents.iter().filter(|a| a.kind == AgentKind::Npc).count();
        self.agents.retain(|a| a.kind == AgentKind::Player);
        self.reset_cache_stats();
        self.emit(format!("cleared {n} NPCs"));
    }

    /// Zero the cache statistics of every agent (called when the load
    /// changes so the HUD shows the rate for the *current* load).
    pub fn reset_cache_stats(&mut self) {
        for a in &mut self.agents {
            a.cache.reset_stats();
        }
    }

    /// Invalidate every agent's local block window that covers `pos`
    /// (a world edit landed inside it; without this the window would serve
    /// the pre-edit block until the agent moved to a new centre cell).
    fn invalidate_caches_at(&mut self, pos: BlockPos) {
        for a in &mut self.agents {
            if a.cache.contains(pos) {
                a.cache.mark_dirty();
            }
        }
    }

    /// Snapshot of all agent states (players first, then NPCs).
    pub fn agents(&self) -> Vec<AgentState> {
        let mut out = Vec::with_capacity(self.agents.len());
        for a in &self.agents {
            if a.kind == AgentKind::Player {
                out.push(a.state());
            }
        }
        for a in &self.agents {
            if a.kind == AgentKind::Npc {
                out.push(a.state());
            }
        }
        out
    }

    /// The primary player's (id 0) state — the camera source of truth for
    /// the single-player (built-in) client.
    pub fn player_state(&self) -> AgentState {
        self.agent_state(0)
    }

    /// Simple stats for the HUD / debugging (cache counters aggregated over
    /// all agents; they reset when the NPC load changes). `chunks_sent` is
    /// per-viewer, so the caller supplies its own streamer's count.
    pub fn stats(&self, chunks_sent: usize) -> ServerStats {
        let mut cache = CacheStats::default();
        let mut npcs = 0usize;
        for a in &self.agents {
            cache.add(a.cache.stats());
            if a.kind == AgentKind::Npc {
                npcs += 1;
            }
        }
        ServerStats {
            chunks_generated: self.world.chunks_generated(),
            chunks_sent,
            deltas: self.world.delta_count(),
            agents: self.agents.len(),
            npcs,
            cache,
        }
    }
}

/// HUD-facing statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ServerStats {
    pub chunks_generated: usize,
    pub chunks_sent: usize,
    pub deltas: usize,
    pub agents: usize,
    /// NPC count (agents minus the player).
    pub npcs: usize,
    /// Local-block-window statistics, summed over all agents since the last
    /// `reset_cache_stats` (i.e. for the current NPC load).
    pub cache: CacheStats,
}

/// Per-viewer chunk streaming state. The world is shared, but each viewer
/// (a built-in page or a network connection) sees it from its own player and
/// receives each chunk region exactly once — so the `sent` set and the
/// outbound queue are per-viewer, not per-world.
pub struct Streamer {
    /// Chunks whose region has already been sent to this viewer.
    sent: std::collections::HashSet<ChunkPos>,
    /// Regions queued for delivery, nearest first.
    queue: Vec<WorldUpdate>,
}

impl Streamer {
    pub fn new() -> Self {
        Self {
            sent: std::collections::HashSet::new(),
            queue: Vec::new(),
        }
    }

    /// Stream the world around `viewpoint`: proactively generate chunks
    /// within VIEW_RADIUS+1 (nearest first) so streaming context is ready,
    /// then emit region payloads for the nearest ready, unsent chunks.
    pub fn tick(&mut self, world: &mut World, viewpoint: Vec3) {
        let pc = ChunkPos::of(BlockPos::new(viewpoint.x as i32, 0, viewpoint.z as i32));

        // 1) Generation pass: ungenerated terrain chunks, nearest first.
        let mut todo: Vec<(i64, ChunkPos)> = Vec::new();
        for dx in -(VIEW_RADIUS + 1)..=VIEW_RADIUS + 1 {
            for dz in -(VIEW_RADIUS + 1)..=VIEW_RADIUS + 1 {
                if dx * dx + dz * dz > (VIEW_RADIUS + 1) * (VIEW_RADIUS + 1) {
                    continue;
                }
                for cy in 0..(WORLD_HEIGHT / rustcraft_world::CHUNK) {
                    let c = ChunkPos::new(pc.x + dx, cy, pc.z + dz);
                    if c.guaranteed_air() || world.contains(&c) {
                        continue;
                    }
                    let d2 = (dx * dx + dz * dz) as i64 + (cy as i64).abs() * 4;
                    todo.push((d2, c));
                }
            }
        }
        todo.sort_by_key(|(d, _)| *d);
        todo.truncate(GEN_PER_TICK);
        for (_d, c) in todo {
            world.generate(c);
        }

        // 2) Send pass: ready, unsent chunks, nearest first.
        let mut candidates: Vec<(i64, ChunkPos)> = Vec::new();
        for dx in -VIEW_RADIUS..=VIEW_RADIUS {
            for dz in -VIEW_RADIUS..=VIEW_RADIUS {
                if dx * dx + dz * dz > (VIEW_RADIUS + 1) * (VIEW_RADIUS + 1) {
                    continue;
                }
                for cy in 0..(WORLD_HEIGHT / rustcraft_world::CHUNK) {
                    let c = ChunkPos::new(pc.x + dx, cy, pc.z + dz);
                    if c.guaranteed_air() || self.sent.contains(&c) {
                        continue;
                    }
                    if Self::region_ready(world, c) {
                        let d2 = (dx * dx + dz * dz) as i64 + (cy as i64).abs() * 2;
                        candidates.push((d2, c));
                    }
                }
            }
        }
        candidates.sort_by_key(|(d, _)| *d);
        candidates.truncate(STREAM_PER_TICK);
        for (_d, c) in candidates {
            let region = world.region(c);
            self.sent.insert(c);
            self.queue.push(WorldUpdate::Chunk { pos: c, data: region });
        }
    }

    /// Queue resends of edited chunks this viewer already holds (so a break
    /// in one client is seen by every other client that has the chunk).
    pub fn apply_edits(&mut self, world: &World, dirty: &[ChunkPos]) {
        for c in dirty {
            if self.sent.contains(c) {
                let region = world.region(*c);
                self.queue.push(WorldUpdate::Chunk { pos: *c, data: region });
            }
        }
    }

    /// Drain the regions queued for delivery since the last call.
    pub fn take(&mut self) -> Vec<WorldUpdate> {
        std::mem::take(&mut self.queue)
    }

    /// Note chunks the viewer's terrain pool evicted (compaction). The
    /// world keeps all data — only this viewer's bookkeeping changes: the
    /// chunks are forgotten so [`Self::tick`] re-sends the ones that are
    /// visible again (nearest-first, at the normal stream rate). Without
    /// this, chunks evicted while far away (fully fogged, dropped silently)
    /// would stay holes when the viewer walks back over them.
    pub fn note_evicted(&mut self, evicted: &[ChunkPos]) {
        for c in evicted {
            self.sent.remove(c);
        }
    }

    /// How many distinct chunk regions this viewer has been sent.
    pub fn sent_count(&self) -> usize {
        self.sent.len()
    }

    /// Whether the region for `pos` is recorded as already sent to this
    /// viewer (i.e. the streamer won't re-send it unless it is evicted).
    pub fn sent_contains(&self, pos: &ChunkPos) -> bool {
        self.sent.contains(pos)
    }

    /// A chunk's streamed region covers its 3x3x3 chunk neighbourhood
    /// (16 + 2*5 blocks). All of that must be known (generated or air).
    fn region_ready(world: &World, c: ChunkPos) -> bool {
        for k in Self::context_chunks(c) {
            if k.guaranteed_air() {
                continue;
            }
            if k.y * rustcraft_world::CHUNK < 0 || k.y * rustcraft_world::CHUNK >= WORLD_HEIGHT {
                continue; // outside world = air
            }
            if !world.contains(&k) {
                return false;
            }
        }
        true
    }

    /// The 3x3x3 chunk neighbourhood overlapping a chunk's region payload.
    fn context_chunks(c: ChunkPos) -> [ChunkPos; 27] {
        let mut out = [ChunkPos::new(0, 0, 0); 27];
        let mut i = 0;
        for dy in -1i32..=1 {
            for dz in -1i32..=1 {
                for dx in -1i32..=1 {
                    out[i] = ChunkPos::new(c.x + dx, c.y + dy, c.z + dz);
                    i += 1;
                }
            }
        }
        out
    }
}

impl Default for Streamer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustcraft_world::{Block, BlockPos, REGION_BLOCKS, TERRAIN_POOL_IDX, TERRAIN_POOL_VERTS, WORLD_HEIGHT};

    fn tick_n(s: &mut Server, n: u32) {
        for _ in 0..n {
            s.tick(1.0 / 60.0);
        }
    }

    /// The pool must hold the ENTIRE worst-case streamed view (all layers,
    /// opaque + water) with headroom — see `examples/pool_measure.rs` for
    /// the full multi-seed scan. These are the pinned known-worst
    /// positions from that scan: if a meshing/terrain change raises the
    /// demand above 80% of the caps, this fails and the caps (or the
    /// measurement) must be revisited. Keep the pins in sync with
    /// `pool_measure` when re-running it.
    #[test]
    fn worst_view_fits_terrain_pool_with_headroom() {
        let pinned: [(u64, i32, i32); 2] = [
            (888, 120, 0), // measured worst: 1,870,648 verts
            (888, 500, 500), // runner-up: 1,838,180 verts
        ];
        let mut worst = (0usize, 0usize, String::from("none"));
        for (seed, ox, oz) in pinned {
            let mut world = World::new(seed);
            let pc = ChunkPos::of(BlockPos::new(ox, 0, oz));
            // Pre-generate view + ±1 chunk halo: a chunk's region reads at
            // most 5 blocks into each neighbour, so ±1 generated context is
            // enough for exact meshes (missing context would sample as air
            // and inflate boundary meshes).
            let halo = VIEW_RADIUS + 1;
            for dx in -halo..=halo {
                for dz in -halo..=halo {
                    for cy in 0..=(WORLD_HEIGHT / rustcraft_world::CHUNK) {
                        world
                            .generate(ChunkPos::new(pc.x + dx, cy, pc.z + dz));
                    }
                }
            }
            let mut v = 0usize;
            let mut i = 0usize;
            for dx in -VIEW_RADIUS..=VIEW_RADIUS {
                for dz in -VIEW_RADIUS..=VIEW_RADIUS {
                    if dx * dx + dz * dz > (VIEW_RADIUS + 1) * (VIEW_RADIUS + 1) {
                        continue;
                    }
                    for cy in 0..(WORLD_HEIGHT / rustcraft_world::CHUNK) {
                        let pos = ChunkPos::new(pc.x + dx, cy, pc.z + dz);
                        let data = world.region(pos);
                        let mesh = rustcraft_world::mesh::build_chunk_mesh(
                            (pos.x * 16, pos.y * 16, pos.z * 16),
                            &data,
                        );
                        v += mesh.vertices.len() / 6 + mesh.water_vertices.len() / 6;
                        i += mesh.indices.len() + mesh.water_indices.len();
                    }
                }
            }
            if v > worst.0 {
                worst = (v, i, format!("seed {seed} @ ({ox},{oz})"));
            }
        }
        let v_pct = worst.0 as f64 / TERRAIN_POOL_VERTS as f64 * 100.0;
        let i_pct = worst.1 as f64 / TERRAIN_POOL_IDX as f64 * 100.0;
        assert!(
            worst.0 as f64 <= TERRAIN_POOL_VERTS as f64 * 0.8,
            "worst pinned view ({}) uses {v_pct:.0}% of TERRAIN_POOL_VERTS (> 80%): raise the caps in rustcraft-world or re-run pool_measure",
            worst.2
        );
        assert!(
            worst.1 as f64 <= TERRAIN_POOL_IDX as f64 * 0.8,
            "worst pinned view ({}) uses {i_pct:.0}% of TERRAIN_POOL_IDX (> 80%): raise the caps in rustcraft-world or re-run pool_measure",
            worst.2
        );
    }

    #[test]
    fn evicted_chunks_are_re_streamed_when_visible_again() {
        let mut s = Server::new(1337);
        let mut st = Streamer::new();
        for _ in 0..30 {
            s.tick(1.0 / 60.0); // let the streamer generate the spawn area
            let vp = s.player_state().pos;
            st.tick(s.world_mut(), vp);
        }
        let sent_before = st.sent_count();
        assert!(sent_before > 10, "the spawn view must have been streamed");
        let p = s.player_state().pos;
        // The player's own chunk: evict it (the client's pool dropped it) —
        // the streamer must re-send it on the next tick, since it is still
        // visible.
        let own = rustcraft_world::ChunkPos::of(rustcraft_world::BlockPos::new(
            p.x as i32,
            p.y as i32,
            p.z as i32,
        ));
        st.note_evicted(&[own]);
        assert!(!st.sent_contains(&own), "eviction must be forgotten");
        st.tick(s.world_mut(), p);
        let updates = st.take();
        let re = updates
            .iter()
            .find(|u| matches!(u, WorldUpdate::Chunk { pos, .. } if *pos == own))
            .expect("the evicted, still-visible chunk must be re-sent");
        let WorldUpdate::Chunk { data, .. } = re;
        assert_eq!(data.len(), REGION_BLOCKS, "re-send must be a full region");
        // A chunk evicted far from the viewpoint (out of the stream radius)
        // must NOT be re-sent — it is invisible to this viewer for now.
        let far = rustcraft_world::ChunkPos::new(
            own.x + VIEW_RADIUS + 5,
            0,
            own.z + VIEW_RADIUS + 5,
        );
        // Generate it first so it is a real (not unknown) chunk, then
        // pretend the client evicted it.
        s.world_mut().generate(far);
        st.note_evicted(&[far]);
        st.tick(s.world_mut(), p);
        let updates = st.take();
        assert!(
            !updates
                .iter()
                .any(|u| matches!(u, WorldUpdate::Chunk { pos, .. } if *pos == far)),
            "an evicted chunk outside the view must not be re-sent"
        );
    }

    #[test]
    fn fog_chunk_distance_invariant() {
        // The client drops chunks whose 3D Chebyshev chunk-cell distance is
        // >= FOG_CHUNK_DIST. That must be the smallest d whose nearest corner
        // is already beyond FOG_END (fully fogged => invisible => safe to
        // drop without a re-send). Nearest corner at distance d is (d-1)*16
        // blocks away.
        use rustcraft_world::camera::FOG_END;
        let fog = FOG_END as i32;
        let derived = fog / 16 + 2; // what the client computes
        let smallest = (1..=64)
            .find(|&d| (d - 1) * 16 > fog)
            .expect("some distance is fully fogged");
        assert_eq!(
            derived, smallest,
            "FOG_CHUNK_DIST ({derived}) must equal the first fully-fogged distance ({smallest})"
        );
    }

    #[test]
    fn player_falls_to_ground() {
        let mut s = Server::new(1337);
        let (sx, sz) = Server::find_spawn(&s.world, 8, 8, 16);
        let h = s.world.height_at(sx, sz);
        tick_n(&mut s, 180); // 3s of simulation
        let p = s.player_state();
        assert!(p.on_ground, "player should be standing on the ground");
        // Feet rest on top of the topmost solid block (at y == h).
        assert_eq!(p.pos.y as i32, h + 1, "feet should rest on the terrain surface");
        // No more falling: one more tick keeps the position stable.
        let y = p.pos.y;
        tick_n(&mut s, 30);
        assert!((s.player_state().pos.y - y).abs() < 1e-3);
    }

    #[test]
    fn player_spawns_on_dry_grass() {
        for seed in [1337u64, 42, 7, 999] {
            let mut s = Server::new(seed);
            tick_n(&mut s, 60);
            let p = s.player_state();
            let ground = BlockPos::new(
                p.pos.x.floor() as i32,
                p.pos.y as i32 - 1,
                p.pos.z.floor() as i32,
            );
            assert_eq!(
                s.world.block_at(ground),
                Block::Grass,
                "seed {seed}: spawn ground should be grass"
            );
            let body = BlockPos::new(
                p.pos.x.floor() as i32,
                (p.pos.y + 0.5).floor() as i32,
                p.pos.z.floor() as i32,
            );
            assert!(
                !s.world.block_at(body).is_water(),
                "seed {seed}: spawn should not be underwater"
            );
        }
    }

    #[test]
    fn swimming_sinks_slowly_and_rises_with_space() {
        let mut s = Server::new(1337);
        // Find a lake column (at least 3 blocks deep) near the origin.
        let (lx, lz) = (0..4096)
            .find_map(|i| {
                let (x, z) = (i % 64 - 32, i / 64 - 32);
                (s.world.height_at(x, z) < rustcraft_world::SEA_LEVEL - 2).then_some((x, z))
            })
            .expect("expected a lake near the origin");
        let bed = s.world.height_at(lx, lz);
        // Drop the player just above the water surface (feet at SEA+2).
        let sea = rustcraft_world::SEA_LEVEL;
        s.agents[0].pos = Vec3::new(lx as f32 + 0.5, (sea + 2) as f32, lz as f32 + 0.5);
        s.agents[0].vel = Vec3::new(0.0, 0.0, 0.0);
        tick_n(&mut s, 30); // 0.5s: enter the water and sink (capped fall)
        let p = s.player_state();
        assert!(
            p.pos.y < (sea + 1) as f32,
            "player should be below the surface (y={})",
            p.pos.y
        );
        assert!(
            p.pos.y > bed as f32 + 1.0,
            "player should still be swimming, not on the lakebed (y={} bed={})",
            p.pos.y,
            bed
        );
        assert!(s.agents[0].in_water(&mut s.world));
        // Hold Space: swim back up to the surface.
        let mut input = Input::default();
        input.keys.insert(Key::Space);
        s.set_input(input);
        let mut peak = f32::MIN;
        for _ in 0..120 {
            tick_n(&mut s, 1);
            peak = peak.max(s.player_state().pos.y);
        }
        assert!(
            peak > sea as f32 + 0.5,
            "player should have swum up to the surface (peak {})",
            peak
        );
    }

    #[test]
    fn player_moves_with_input() {
        let mut s = Server::new(1337);
        tick_n(&mut s, 120); // settle
        let start = s.player_state().pos;
        let mut input = Input::default();
        input.keys.insert(Key::W);
        s.set_input(input);
        tick_n(&mut s, 60); // 1s walking
        let end = s.player_state().pos;
        let dist = ((end.x - start.x).powi(2) + (end.z - start.z).powi(2)).sqrt();
        // WALK_SPEED is 4.5 blocks/s; a second of walking on open ground
        // should cover roughly that. The upper bound catches the old bug
        // (missing `* dt` moved ~270 blocks/s).
        assert!(dist > 1.0, "player should have walked, moved {dist}");
        assert!(dist < 10.0, "player moved {dist} in 1s — too fast");
    }

    #[test]
    fn jump_reaches_reasonable_height_and_lands() {
        let mut s = Server::new(1337);
        tick_n(&mut s, 120); // settle on the ground
        let ground_y = s.player_state().pos.y;
        // Press jump.
        let mut input = Input::default();
        input.keys.insert(Key::Space);
        s.set_input(input);
        tick_n(&mut s, 1); // the edge-detected jump tick
        s.set_input(Input::default()); // release
        let mut peak = ground_y;
        let mut landed_ticks = 0u32;
        for _ in 0..120 {
            tick_n(&mut s, 1);
            let p = s.player_state();
            peak = peak.max(p.pos.y);
            if p.on_ground && (p.pos.y - ground_y).abs() < 0.01 {
                landed_ticks += 1;
            }
        }
        // v^2/(2g) = 9^2/(2*28) ≈ 1.45 blocks apex.
        let rise = peak - ground_y;
        assert!(rise > 0.5, "jump too weak: rose {rise}");
        assert!(rise < 3.0, "jump too high: rose {rise} (missing `* dt`?)");
        assert!(landed_ticks > 0, "player never landed after jumping");
    }

    #[test]
    fn fly_mode_hovers_moves_and_falls_back_down() {
        let mut s = Server::new(1337);
        tick_n(&mut s, 120); // settle on the ground
        let ground = s.player_state().pos;
        assert!(!s.player_state().fly);

        // Enter fly mode.
        s.push_action(Action::ToggleFly);
        tick_n(&mut s, 1);
        assert!(s.player_state().fly, "F should toggle fly on");

        // Hold W + Space for 1s: fly forward and up at FLY_BASE_SPEED.
        let mut input = Input::default();
        input.keys.insert(Key::W);
        input.keys.insert(Key::Space);
        s.set_input(input);
        tick_n(&mut s, 60);
        let p = s.player_state();
        let horiz = ((p.pos.x - ground.x).powi(2) + (p.pos.z - ground.z).powi(2)).sqrt() as f64;
        // Horizontal and vertical each get the full base speed (orthogonal
        // addition), so 1s of W+Space covers ~base in both.
        let expected = FLY_BASE_SPEED as f64;
        assert!(
            (horiz - expected).abs() < 2.0,
            "fly speed: moved {horiz} in 1s, expected ~{expected}"
        );
        let climbed = (p.pos.y - ground.y) as f64;
        assert!(
            (climbed - expected).abs() < 2.0,
            "should have climbed ~{expected}, climbed {climbed}"
        );

        // Release everything: hover in place (no gravity while flying).
        s.set_input(Input::default());
        let hover = s.player_state().pos;
        tick_n(&mut s, 60);
        let p = s.player_state();
        let drift = ((p.pos.x - hover.x).powi(2)
            + (p.pos.y - hover.y).powi(2)
            + (p.pos.z - hover.z).powi(2))
            .sqrt();
        assert!(drift < 1e-3, "flying player drifted {drift} with no input (gravity?)");

        // Leave fly mode: gravity returns and the player falls.
        s.push_action(Action::ToggleFly);
        tick_n(&mut s, 1);
        assert!(!s.player_state().fly, "F should toggle fly off");
        let y = s.player_state().pos.y;
        tick_n(&mut s, 30);
        assert!(s.player_state().pos.y < y - 1.0, "should fall after exiting fly mode");
    }

    #[test]
    fn fly_speed_adjusts_and_clamps() {
        let mut s = Server::new(1337);
        tick_n(&mut s, 120);
        let base = s.player_state().fly_speed;
        assert!((base - FLY_BASE_SPEED).abs() < 1e-3);

        // Ramp up: each FlyFaster multiplies by FLY_STEP.
        for _ in 0..10 {
            s.push_action(Action::FlyFaster);
            tick_n(&mut s, 1);
        }
        let fast = s.player_state().fly_speed;
        assert!(fast > base * 5.0, "speed should have ramped up, got {fast}");

        // Hard clamp at the max.
        for _ in 0..200 {
            s.push_action(Action::FlyFaster);
            tick_n(&mut s, 1);
        }
        let top = s.player_state().fly_speed;
        assert!((top - FLY_MAX_SPEED).abs() < 1e-3, "max clamp broken: {top}");

        // And back down to the min.
        for _ in 0..200 {
            s.push_action(Action::FlySlower);
            tick_n(&mut s, 1);
        }
        let bottom = s.player_state().fly_speed;
        assert!((bottom - FLY_MIN_SPEED).abs() < 1e-3, "min clamp broken: {bottom}");

        // At max speed, 1s of flight covers ~FLY_MAX_SPEED blocks.
        s.push_action(Action::ToggleFly);
        tick_n(&mut s, 1);
        for _ in 0..200 {
            s.push_action(Action::FlyFaster);
            tick_n(&mut s, 1);
        }
        let start = s.player_state().pos;
        let mut input = Input::default();
        input.keys.insert(Key::W);
        s.set_input(input);
        tick_n(&mut s, 60);
        let end = s.player_state().pos;
        let dist = ((end.x - start.x).powi(2) + (end.z - start.z).powi(2)).sqrt() as f64;
        assert!(
            (dist - FLY_MAX_SPEED as f64).abs() < 2.0,
            "max-speed flight covered {dist} in 1s, expected ~{FLY_MAX_SPEED}"
        );
    }

    #[test]
    fn streaming_sends_region_payloads() {
        let mut s = Server::new(42);
        let mut st = Streamer::new();
        for _ in 0..240 {
            // 4s: generation + streaming (the streamer is driven per-viewer).
            s.tick(1.0 / 60.0);
            let vp = s.player_state().pos;
            st.tick(s.world_mut(), vp);
        }
        let updates = st.take();
        assert!(!updates.is_empty(), "expected chunk updates");
        let mut saw_terrain = false;
        for u in &updates {
            let WorldUpdate::Chunk { pos, data } = u;
            assert_eq!(data.len(), REGION_BLOCKS, "region payload size");
            assert!(pos.y * 16 < WORLD_HEIGHT);
            // Regions may be legitimately all-air (high chunk columns on
            // low terrain); at least one must carry terrain.
            if data.iter().any(|&b| b != 0) {
                saw_terrain = true;
            }
        }
        assert!(saw_terrain, "expected terrain in at least one region");
        // Player's own chunk should have been sent.
        let stats = s.stats(st.sent_count());
        assert!(stats.chunks_sent >= 20, "sent {} chunks", stats.chunks_sent);
        assert!(stats.chunks_generated > stats.chunks_sent / 4);
    }

    #[test]
    fn edits_go_through_delta_layer() {
        let mut s = Server::new(7);
        // Pick a far-away column so its chunk is surely not generated yet.
        let (x, z) = (200, 200);
        let h = s.world.height_at(x, z);
        let target = BlockPos::new(x, h, z);
        let was = s.world.block_at(target);
        assert!(was.is_solid());
        // Force the chunk ungenerated: edit before any generation happens there.
        let generated_before = s.world.contains(&rustcraft_world::ChunkPos::of(target));
        if generated_before {
            return; // chunk already generated by spawn/stream; test the other path
        }
        let dirty = s.world.set_block(target, Block::Air);
        assert!(dirty.contains(&rustcraft_world::ChunkPos::of(target)));
        assert!(s.world.delta_count() >= 1, "delta should be stored");
        assert_eq!(s.world.block_at(target), Block::Air);
        // Regenerating the chunk must keep the delta applied.
        s.world.generate(rustcraft_world::ChunkPos::of(target));
        assert_eq!(s.world.block_at(target), Block::Air);
    }

    /// Look slightly down-forward: the ground is dense in that direction,
    /// so a target always exists (and something is behind the hole after
    /// a break).
    const TEST_PITCH: f32 = -0.7;

    #[test]
    fn break_and_place_update_world() {
        let mut s = Server::new(99);
        tick_n(&mut s, 120); // settle on ground
        let yaw = s.player_state().yaw;
        let d = rustcraft_world::camera::look_direction(yaw, TEST_PITCH);
        let dir = Vec3::new(d[0], d[1], d[2]);
        let (target, _) = s
            .world
            .raycast(&s.agents[0].eye(), &dir, 6.0)
            .expect("ground should be in view");
        assert!(target.y > 0);
        // Break exactly the aimed block.
        s.push_action(Action::Break { yaw, pitch: TEST_PITCH });
        tick_n(&mut s, 2);
        assert_eq!(s.world.block_at(target), Block::Air, "targeted block must break");
        // Place against the face the ray now hits (the block behind the
        // hole); the new block goes in the cell in front of it.
        let (_, prev) = s
            .world
            .raycast(&s.agents[0].eye(), &dir, 6.0)
            .expect("ground behind the hole should be in view");
        s.push_action(Action::Place { yaw, pitch: TEST_PITCH });
        tick_n(&mut s, 2);
        // Placement is skipped when the cell would intersect an agent.
        let blocked = s.agents().iter().any(|a| {
            let p = a.pos;
            (prev.x as f32) < p.x + 0.3
                && (prev.x as f32) + 1.0 >= p.x - 0.3
                && (prev.y as f32) < p.y + 1.8
                && (prev.y as f32) + 1.0 >= p.y
                && (prev.z as f32) < p.z + 0.3
                && (prev.z as f32) + 1.0 >= p.z - 0.3
        });
        assert!(
            blocked || s.world.block_at(prev) == Block::Stone,
            "block must be placed against the face"
        );
    }

    #[test]
    fn break_uses_click_time_aim_not_current_aim() {
        // Regression: mouse deltas that arrive after a click used to rotate
        // the server's aim before the queued break was raycast, so the wrong
        // block (off to the side) got broken while moving. Actions now carry
        // the aim from the moment of the click.
        let mut s = Server::new(1337);
        tick_n(&mut s, 180); // settle on ground
        let p = s.player_state();
        let (yaw0, pitch0) = (p.yaw, TEST_PITCH);
        let d0 = rustcraft_world::camera::look_direction(yaw0, pitch0);
        let dir0 = Vec3::new(d0[0], d0[1], d0[2]);
        let (hit0, _) = s
            .world
            .raycast(&s.agents[0].eye(), &dir0, 6.0)
            .expect("ground should be in view");
        // Simulate the mouse moving after the click: find a rotated aim with
        // a DIFFERENT target, then deliver a break stamped with the OLD aim.
        let mut found = None;
        for k in 1..=8 {
            let yaw1 = yaw0 + k as f32 * 0.8;
            let d1 = rustcraft_world::camera::look_direction(yaw1, pitch0);
            let dir1 = Vec3::new(d1[0], d1[1], d1[2]);
            if let Some((h1, _)) = s.world.raycast(&s.agents[0].eye(), &dir1, 6.0) {
                if h1 != hit0 {
                    found = Some((yaw1, h1));
                    break;
                }
            }
        }
        let (yaw1, hit1) = found.expect("need a second target to prove aim correctness");
        s.agents[0].yaw = yaw1;
        s.push_action(Action::Break { yaw: yaw0, pitch: pitch0 });
        tick_n(&mut s, 2);
        assert_eq!(s.world.block_at(hit0), Block::Air, "block at click-time aim must break");
        assert_ne!(
            s.world.block_at(hit1),
            Block::Air,
            "block at post-click aim must NOT break"
        );
    }

    #[test]
    fn player_state_reports_crosshair_target() {
        let mut s = Server::new(1337);
        tick_n(&mut s, 180); // settle on ground
        let p = s.player_state();
        // The reported target must match a fresh raycast from the same aim
        // (the player is idle, so the aim is unchanged).
        let dir = s.agents[0].look_direction();
        let expected = s
            .world
            .raycast(&s.agents[0].eye(), &dir, 6.0)
            .map(|(h, _)| h);
        assert_eq!(p.target, expected);
    }

    #[test]
    fn local_block_window_serves_player_physics() {
        let mut s = Server::new(5);
        tick_n(&mut s, 120);
        let cache = &s.agents[0].cache;
        // The ground under the player is inside the window.
        let c = cache.center();
        let ground = BlockPos::new(c.x, s.player_state().pos.y as i32 - 1, c.z);
        assert!(cache.contains(ground), "ground under the player must be in the window");
        // Steady-state physics is served from the window: only the very
        // first tick (before the first build) could fall back to the world.
        let st = cache.stats();
        assert!(st.lookups > 100, "expected plenty of physics lookups");
        assert!(
            st.hits as f64 > st.lookups as f64 * 0.98,
            "window should serve nearly all lookups, got {st:?}"
        );
    }

    #[test]
    fn npc_load_spawns_exact_count() {
        let mut s = Server::new(1337);
        tick_n(&mut s, 60); // let the streamer settle
        s.set_npc_load(50, 12.0);
        s.push_action(Action::NpcLoad);
        tick_n(&mut s, 2);
        assert_eq!(s.agents().len(), 51, "player + 50 NPCs");
        assert_eq!(s.stats(0).npcs, 50);
        // Re-spawning replaces the load: the count stays exact.
        s.set_npc_load(10, 8.0);
        s.push_action(Action::NpcLoad);
        tick_n(&mut s, 2);
        assert_eq!(s.agents().len(), 11, "re-load must replace, not append");
        // Clear removes everything.
        s.push_action(Action::NpcClear);
        tick_n(&mut s, 2);
        assert_eq!(s.agents().len(), 1, "only the player remains");
        assert_eq!(s.stats(0).npcs, 0);
    }

    #[test]
    fn npc_load_phyllotaxis_layout() {
        let mut s = Server::new(1337);
        tick_n(&mut s, 60);
        let spacing = 12.0f32;
        let count = 40u32;
        s.set_npc_load(count, spacing);
        s.push_action(Action::NpcLoad);
        tick_n(&mut s, 2);
        let p = s.player_state().pos;
        let agents = s.agents();
        // Each NPC i sits at radius ~ spacing*sqrt(i+1) around the player
        // (the find_spawn halo + column rounding allow a few blocks of slack).
        for (i, a) in agents.iter().skip(1).enumerate() {
            let expected = spacing * (i as f32 + 1.0).sqrt();
            let dist = ((a.pos.x - p.x).powi(2) + (a.pos.z - p.z).powi(2)).sqrt();
            assert!(
                (dist - expected).abs() < 6.0 + spacing * 0.2,
                "NPC {i} at {dist:.1} blocks from player, expected ~{expected:.1}"
            );
        }
        // The cloud spans the expected radius overall.
        let max_r = agents
            .iter()
            .skip(1)
            .map(|a| ((a.pos.x - p.x).powi(2) + (a.pos.z - p.z).powi(2)).sqrt())
            .fold(0.0f32, f32::max);
        let full = spacing * (count as f32).sqrt();
        assert!(
            (max_r - full).abs() < full * 0.3 + 8.0,
            "outermost NPC at {max_r:.1}, expected ~{full:.1}"
        );
        // No two NPCs sit unreasonably close together.
        let npcs: Vec<Vec3> = agents.iter().skip(1).map(|a| a.pos).collect();
        let mut min_pair = f32::MAX;
        for i in 0..npcs.len() {
            for j in (i + 1)..npcs.len() {
                let d = ((npcs[i].x - npcs[j].x).powi(2)
                    + (npcs[i].z - npcs[j].z).powi(2))
                    .sqrt();
                min_pair = min_pair.min(d);
            }
        }
        assert!(
            min_pair >= spacing * 0.3,
            "two NPCs only {min_pair:.1} blocks apart (spacing {spacing})"
        );
    }

    /// The load test's core property: with many NPCs, steady-state collision
    /// physics is answered by the per-agent local block window, not by the
    /// world's chunk buffers.
    #[test]
    fn npc_load_physics_served_from_local_window() {
        let mut s = Server::new(1337);
        tick_n(&mut s, 60);
        s.set_npc_load(64, 16.0);
        s.push_action(Action::NpcLoad);
        tick_n(&mut s, 121); // spawn tick + 2s of landing/window building
        s.reset_cache_stats(); // measure steady state only
        tick_n(&mut s, 600); // 10s of wandering
        let c = s.stats(0).cache;
        assert!(c.lookups > 100_000, "expected >100k lookups, got {}", c.lookups);
        let hit_rate = c.hits as f64 / c.lookups as f64;
        assert!(
            hit_rate > 0.995,
            "window hit rate {hit_rate:.4} — physics must run on the cache"
        );
        // Solid data in particular never comes from the chunk buffers in
        // steady state (only transient pre-build ticks could).
        assert!(
            (c.solid_misses as f64) < c.lookups as f64 * 0.0005,
            "solid fallbacks {} of {} lookups",
            c.solid_misses,
            c.lookups
        );
        // NPCs move, so their windows keep rebuilding (and the rebuilds are
        // what amortise the chunk-buffer access).
        assert!(c.rebuilds > 100, "expected steady rebuilds, got {}", c.rebuilds);
    }

    #[test]
    fn npc_load_dials_clamp_to_limits() {
        let mut s = Server::new(1337);
        for _ in 0..32 {
            s.push_action(Action::NpcCountUp);
            tick_n(&mut s, 1);
        }
        assert_eq!(s.npc_load_config().0, NPC_COUNT_MAX);
        for _ in 0..32 {
            s.push_action(Action::NpcCountDown);
            tick_n(&mut s, 1);
        }
        assert_eq!(s.npc_load_config().0, NPC_COUNT_MIN);
        for _ in 0..32 {
            s.push_action(Action::NpcSpacingUp);
            tick_n(&mut s, 1);
        }
        assert!((s.npc_load_config().1 - NPC_SPACING_MAX).abs() < 1e-4);
        for _ in 0..32 {
            s.push_action(Action::NpcSpacingDown);
            tick_n(&mut s, 1);
        }
        assert!((s.npc_load_config().1 - NPC_SPACING_MIN).abs() < 1e-4);
        // set_npc_load clamps the same ranges.
        s.set_npc_load(0, 1.0);
        assert_eq!(s.npc_load_config(), (NPC_COUNT_MIN, NPC_SPACING_MIN));
        s.set_npc_load(999_999, 9999.0);
        assert_eq!(s.npc_load_config(), (NPC_COUNT_MAX, NPC_SPACING_MAX));
    }

    /// Breaking the block under the player's feet must be seen by physics
    /// (the local window is invalidated on edits) — otherwise the cached
    /// solid would hold the player up.
    #[test]
    fn breaking_under_feet_invalidates_window() {
        let mut s = Server::new(1337);
        tick_n(&mut s, 180); // settle on ground
        let p0 = s.player_state();
        assert!(p0.on_ground, "player should be standing");
        // Aim straight down and break the block the player stands on.
        let yaw = p0.yaw;
        s.push_action(Action::Break { yaw, pitch: -1.55 });
        tick_n(&mut s, 2);
        let feet = BlockPos::new(p0.pos.x as i32, p0.pos.y as i32 - 1, p0.pos.z as i32);
        assert_eq!(s.world.block_at(feet), Block::Air, "block under feet must break");
        // The player falls one block and stands on the next layer down.
        tick_n(&mut s, 120);
        let p1 = s.player_state();
        assert!(p1.on_ground, "player should land in the hole");
        assert!(
            (p1.pos.y - (p0.pos.y - 1.0)).abs() < 0.05,
            "player should stand 1 block lower (y={} expected ~{})",
            p1.pos.y,
            p0.pos.y - 1.0
        );
    }

    #[test]
    fn npcs_stay_in_world() {
        let mut s = Server::new(3);
        tick_n(&mut s, 600); // 10s of wandering
        for a in s.agents() {
            assert!(a.pos.y > 0.0 && a.pos.y < (WORLD_HEIGHT - 2) as f32);
        }
    }

    #[test]
    fn add_remove_player_and_unique_ids() {
        let mut s = Server::new_world(1337);
        let a = s.add_player();
        let b = s.add_player();
        assert_ne!(a, b, "player ids must be unique");
        assert_eq!(s.player_ids(), vec![a, b]);
        s.remove_player(a);
        assert_eq!(s.player_ids(), vec![b], "removing a player drops only that player");
        // The remaining player still simulates (no dangling per-player state).
        s.tick(1.0 / 60.0);
        assert!(s.agent_state(b).is_player);
    }

    #[test]
    fn shared_world_players_see_each_other_and_edits() {
        let mut s = Server::new_world(1337);
        let a = s.add_player();
        let b = s.add_player();
        let mut st_a = Streamer::new();
        let mut st_b = Streamer::new();

        // Stream a few seconds so both viewers hold the spawn area.
        for _ in 0..180 {
            s.tick(1.0 / 60.0);
            let va = s.agent_state(a).pos;
            let vb = s.agent_state(b).pos;
            st_a.tick(s.world_mut(), va);
            st_b.tick(s.world_mut(), vb);
            let _ = st_a.take();
            let _ = st_b.take();
        }
        assert!(st_a.sent_count() >= 20, "viewer A streamed {} chunks", st_a.sent_count());
        assert!(st_b.sent_count() >= 20, "viewer B streamed {} chunks", st_b.sent_count());

        // Both players are live in the shared world (each viewer renders all).
        let agents = s.agents();
        let players: Vec<u32> = agents.iter().filter(|x| x.is_player).map(|x| x.id).collect();
        assert!(
            players.contains(&a) && players.contains(&b),
            "both players must be present: {players:?}"
        );

        // A breaks the block under its feet; the edit is a change to the
        // shared world. Viewer B already holds that chunk, so it must get a
        // region resend (this is what makes the edit visible to B).
        let pa = s.agent_state(a);
        let feet_block = BlockPos::new(pa.pos.x as i32, pa.pos.y as i32 - 1, pa.pos.z as i32);
        let feet_chunk = ChunkPos::of(feet_block);
        s.push_agent_action(a, Action::Break { yaw: pa.yaw, pitch: -1.55 });
        s.tick(1.0 / 60.0);
        let dirty = s.drain_dirty();
        st_b.apply_edits(s.world(), &dirty);
        let resent = st_b.take();
        assert!(
            resent
                .iter()
                .any(|u| matches!(u, WorldUpdate::Chunk { pos, .. } if *pos == feet_chunk)),
            "viewer B must receive the edited chunk's region (got {} resends)",
            resent.len()
        );
        // And the world actually lost the block (a single shared world).
        assert_eq!(
            s.world.block_at(feet_block),
            Block::Air,
            "A's break must land in the shared world"
        );
    }

    /// The event sink (dashboard log feed) must see world edits and fly
    /// toggles with the acting player's id; without a sink nothing panics.
    #[test]
    fn event_sink_reports_edits_and_fly() {
        let mut s = Server::new(1337);
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        s.set_event_sink(Some(Arc::new(move |m: &str| {
            let _ = tx.send(m.to_string());
        })));

        // Aim down-forward and break (the same trick as the edit tests: a
        // steep pitch always hits while standing on the ground).
        let p = s.player_state();
        s.push_action(Action::Break { yaw: p.yaw, pitch: -0.7 });
        s.push_action(Action::ToggleFly);
        for _ in 0..3 {
            s.tick(1.0 / 60.0);
        }

        // try_iter: the channel stays open (the sink owns the sender for
        // the life of the server), so a blocking iter() would hang.
        let mut events: Vec<String> = rx.try_iter().collect();
        // The built-in `new()` world spawns ambient NPCs... only if the
        // NpcLoad action was applied; here we just expect the two actions
        // above produced their events.
        assert!(
            events.iter().any(|e| e.starts_with("player 0 broke ") && e.contains(" at (")),
            "expected a break event, got {events:?}"
        );
        assert!(
            events.iter().any(|e| e == "player 0 switched to fly mode"),
            "expected a fly event, got {events:?}"
        );

        // Toggling fly back off is reported too.
        let _ = events.drain(..);
        s.push_action(Action::ToggleFly);
        s.tick(1.0 / 60.0);
        assert!(
            rx.try_iter().any(|e| e == "player 0 switched to walk mode"),
            "expected a walk-mode event"
        );
    }
}

/// Minimal 3D vector (avoids a glam dependency in the server).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    pub fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }

    pub fn scale(self, s: f32) -> Self {
        Self::new(self.x * s, self.y * s, self.z * s)
    }

    pub fn length(self) -> f32 {
        (self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }

    pub fn normalize(self) -> Self {
        let l = self.length();
        if l > 1e-6 {
            self.scale(1.0 / l)
        } else {
            Self::new(0.0, 0.0, 0.0)
        }
    }
}
