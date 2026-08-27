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
pub use world::{World, WorldUpdate};

use crate::world::Edit;
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

/// The game server. One instance per player (single-player for now).
pub struct Server {
    seed: u64,
    world: World,
    agents: Vec<Agent>,
    input: Input,
    prev_input: Input,
    actions: Vec<Action>,
    acc: f64,
    time: f64,
    /// Chunks whose region has already been sent to the client.
    sent: std::collections::HashSet<ChunkPos>,
    /// Chunks queued for (re)sending, nearest first.
    updates: Vec<WorldUpdate>,
    /// Edits pending delivery to the client (delivered via region resends).
    _pending_edits: Vec<Edit>,
    /// The block under the player's crosshair (recomputed every tick);
    /// the client draws a wireframe highlight around it.
    target: Option<BlockPos>,
    /// Configured NPC load (applied by Action::NpcLoad): how many NPCs to
    /// spawn and how far apart the spiral arms sit. Load-test facility.
    npc_count: u32,
    npc_spacing: f32,
}

impl Server {
    /// Create a server for a world with the given seed.
    pub fn new(seed: u64) -> Self {
        let world = World::new(seed);
        // Spawn near the origin on dry, tree-free grass (skip water, snow
        // and tree columns so nobody starts underwater or in a trunk).
        let (sx, sz) = Self::find_spawn(&world, 8, 8, 16);
        let surface = world.height_at(sx, sz);

        let mut agents = Vec::new();
        agents.push(Agent::player(0, Vec3::new(sx as f32 + 0.5, (surface + 2) as f32, sz as f32 + 0.5)));
        // A couple of wandering NPC agents (also on dry, tree-free ground).
        for (i, (dx, dz)) in [(3, -4), (-5, 3), (2, 6)].into_iter().enumerate() {
            let (x, z) = Self::find_spawn(&world, 8 + dx, 8 + dz, 6);
            let h = world.height_at(x, z);
            agents.push(Agent::npc(
                (i + 1) as u32,
                Vec3::new(x as f32 + 0.5, (h + 1) as f32, z as f32 + 0.5),
            ));
        }

        Server {
            seed,
            world,
            agents,
            input: Input::default(),
            prev_input: Input::default(),
            actions: Vec::new(),
            acc: 0.0,
            time: 0.0,
            sent: std::collections::HashSet::new(),
            updates: Vec::new(),
            _pending_edits: Vec::new(),
            target: None,
            npc_count: NPC_COUNT_DEFAULT,
            npc_spacing: NPC_SPACING_DEFAULT,
        }
    }

    pub fn seed(&self) -> u64 {
        self.seed
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

    /// Queue a one-shot action (break/place) to be applied on the next tick.
    pub fn push_action(&mut self, a: Action) {
        self.actions.push(a);
    }

    /// Replace the current input state (keys + accumulated mouse deltas).
    pub fn set_input(&mut self, input: Input) {
        self.input = input;
    }

    /// Advance simulation by real time `dt` seconds (clamped) using fixed steps.
    pub fn tick(&mut self, dt: f64) {
        self.acc += dt.min(0.25);
        while self.acc >= TICK_DT {
            self.acc -= TICK_DT;
            self.step(TICK_DT as f32);
        }
        self.stream();
    }

    fn step(&mut self, dt: f32) {
        // Jump edge detection.
        let jump = self.input.keys.contains(Key::Space) && !self.prev_input.keys.contains(Key::Space);

        // Player.
        {
            let player = &mut self.agents[0];
            let move_dir = self.input.move_direction(player.yaw);
            player.step(dt, &mut self.world, move_dir, jump, &self.input);
        }
        // Consume queued actions (fly mode, break/place) against the world.
        let actions = std::mem::take(&mut self.actions);
        for action in actions {
            match action {
                Action::ToggleFly => self.agents[0].toggle_fly(),
                Action::FlyFaster => self.agents[0].adjust_fly_speed(FLY_STEP),
                Action::FlySlower => self.agents[0].adjust_fly_speed(1.0 / FLY_STEP),
                Action::NpcLoad => self.spawn_npcs(),
                Action::NpcClear => self.clear_npcs(),
                Action::NpcCountUp => self.adjust_npc_count(true),
                Action::NpcCountDown => self.adjust_npc_count(false),
                Action::NpcSpacingUp => self.adjust_npc_spacing(true),
                Action::NpcSpacingDown => self.adjust_npc_spacing(false),
                Action::Break { .. } | Action::Place { .. } => {
                    self.apply_player_action(action)
                }
            }
        }

        // Recompute the crosshair target for the client's block highlight
        // (same raycast + range as break/place, from the post-step aim —
        // the same aim the client is about to render).
        self.target = {
            let p = &self.agents[0];
            self.world.raycast(&p.eye(), &p.look_direction(), 6.0).map(|(hit, _)| hit)
        };

        // NPCs.
        for npc in self.agents.iter_mut().skip(1) {
            npc.step_npc(dt, &mut self.world, self.time);
        }

        self.prev_input = self.input;
        self.input.mouse_dx = 0.0;
        self.input.mouse_dy = 0.0;
        self.time += TICK_DT;
    }

    fn apply_player_action(&mut self, action: Action) {
        // Raycast with the aim stamped into the action (the camera at click
        // time), so post-click mouse movement can't move the target.
        let (eye, yaw, pitch) = match action {
            Action::Break { yaw, pitch } | Action::Place { yaw, pitch } => {
                let p = &self.agents[0];
                (p.eye(), yaw, pitch)
            }
            _ => unreachable!("only Break/Place reach apply_player_action"),
        };
        let d = rustcraft_world::camera::look_direction(yaw, pitch);
        let dir = Vec3::new(d[0], d[1], d[2]);
        match self.world.raycast(&eye, &dir, 6.0) {
            Some((hit, prev)) => match action {
                Action::Break { .. } => {
                    if hit.y > 0 {
                        // World edits go through the delta layer.
                        let dirty = self.world.set_block(hit, Block::Air);
                        self.invalidate_caches_at(hit);
                        self.queue_resends(&dirty);
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
                        self.queue_resends(&dirty);
                    }
                }
                // Fly actions never reach here (handled in step()).
                _ => unreachable!("only Break/Place reach apply_player_action"),
            },
            None => {}
        }
    }

    /// Stream chunk regions to the client.
    ///
    /// 1) Proactively generate chunks within VIEW_RADIUS+1 (nearest first) so
    ///    that streaming context (a chunk's region spans its 3x3x3 chunk
    ///    neighbourhood) is available without waiting for sends.
    /// 2) Emit region payloads for the nearest chunks that are ready and
    ///    have not been sent yet.
    fn stream(&mut self) {
        let p = self.agents[0].pos;
        let pc = ChunkPos::of(BlockPos::new(p.x as i32, 0, p.z as i32));

        // 1) Generation pass: ungenerated terrain chunks, nearest first.
        let mut todo: Vec<(i64, ChunkPos)> = Vec::new();
        for dx in -(VIEW_RADIUS + 1)..=VIEW_RADIUS + 1 {
            for dz in -(VIEW_RADIUS + 1)..=VIEW_RADIUS + 1 {
                if dx * dx + dz * dz > (VIEW_RADIUS + 1) * (VIEW_RADIUS + 1) {
                    continue;
                }
                for cy in 0..(WORLD_HEIGHT / rustcraft_world::CHUNK) {
                    let c = ChunkPos::new(pc.x + dx, cy, pc.z + dz);
                    if c.guaranteed_air() || self.world.contains(&c) {
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
            self.world.generate(c);
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
                    if self.region_ready(c) {
                        let d2 = (dx * dx + dz * dz) as i64 + (cy as i64).abs() * 2;
                        candidates.push((d2, c));
                    }
                }
            }
        }
        candidates.sort_by_key(|(d, _)| *d);
        candidates.truncate(STREAM_PER_TICK);
        for (_d, c) in candidates {
            let region = self.world.region(c);
            self.sent.insert(c);
            self.updates.push(WorldUpdate::Chunk { pos: c, data: region });
        }
    }

    /// A chunk's streamed region covers its 3x3x3 chunk neighbourhood
    /// (16 + 2*5 blocks). All of that must be known (generated or air).
    fn region_ready(&self, c: ChunkPos) -> bool {
        for k in self.context_chunks(c) {
            if k.guaranteed_air() {
                continue;
            }
            if k.y * rustcraft_world::CHUNK < 0
                || k.y * rustcraft_world::CHUNK >= WORLD_HEIGHT
            {
                continue; // outside world = air
            }
            if !self.world.contains(&k) {
                return false;
            }
        }
        true
    }

    /// The 3x3x3 chunk neighbourhood overlapping a chunk's region payload.
    fn context_chunks(&self, c: ChunkPos) -> [ChunkPos; 27] {
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

    /// Queue region resends for chunks whose data changed (edits).
    fn queue_resends(&mut self, dirty: &[ChunkPos]) {
        for c in dirty {
            if self.sent.contains(c) {
                let region = self.world.region(*c);
                self.updates.push(WorldUpdate::Chunk { pos: *c, data: region });
            }
        }
    }

    /// Drain world updates produced since the last call.
    pub fn take_world_updates(&mut self) -> Vec<WorldUpdate> {
        std::mem::take(&mut self.updates)
    }

    /// Re-send an already-generated chunk on client request (the client's
    /// terrain buffer pool may evict a chunk; the server keeps it). None if
    /// the chunk was never generated.
    pub fn resend_chunk(&mut self, pos: ChunkPos) -> Option<Vec<u8>> {
        if self.world.contains(&pos) {
            Some(self.world.region(pos))
        } else {
            None
        }
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
    pub fn spawn_npcs(&mut self) {
        let (count, spacing) = self.npc_load_config();
        let p = self.agents[0].pos;
        self.agents.truncate(1); // keep the player; replace the NPC set
        for i in 0..count {
            let angle = (i as f32) * GOLDEN_ANGLE;
            let radius = spacing * (i as f32 + 1.0).sqrt();
            let tx = (p.x + radius * angle.sin()) as i32;
            let tz = (p.z + radius * angle.cos()) as i32;
            let (x, z) = Self::find_spawn(&self.world, tx, tz, 2);
            let h = self.world.height_at(x, z);
            self.agents.push(Agent::npc(
                1 + i as u32,
                Vec3::new(x as f32 + 0.5, (h + 1) as f32, z as f32 + 0.5),
            ));
        }
        self.reset_cache_stats();
    }

    /// Remove all NPCs (the player remains).
    pub fn clear_npcs(&mut self) {
        self.agents.truncate(1);
        self.reset_cache_stats();
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

    /// Snapshot of all agent states (player first).
    pub fn agents(&self) -> Vec<AgentState> {
        self.agents.iter().map(|a| a.state()).collect()
    }

    /// Player state (camera source of truth).
    pub fn player_state(&self) -> AgentState {
        let mut s = self.agents[0].state();
        s.target = self.target;
        s
    }

    /// Simple stats for the HUD / debugging (cache counters aggregated over
    /// all agents; they reset when the NPC load changes).
    pub fn stats(&self) -> ServerStats {
        let mut cache = CacheStats::default();
        for a in &self.agents {
            cache.add(a.cache.stats());
        }
        ServerStats {
            chunks_generated: self.world.chunks_generated(),
            chunks_sent: self.sent.len(),
            deltas: self.world.delta_count(),
            agents: self.agents.len(),
            npcs: self.agents.len().saturating_sub(1),
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustcraft_world::{Block, BlockPos, REGION_BLOCKS, WORLD_HEIGHT};

    fn tick_n(s: &mut Server, n: u32) {
        for _ in 0..n {
            s.tick(1.0 / 60.0);
        }
    }

    #[test]
    fn resend_chunk_round_trips_generated_chunks() {
        let mut s = Server::new(1337);
        tick_n(&mut s, 30); // let the streamer generate the spawn area
        let p = s.player_state().pos;
        let pos = rustcraft_world::ChunkPos::of(rustcraft_world::BlockPos::new(
            p.x as i32,
            p.y as i32,
            p.z as i32,
        ));
        let data = s
            .resend_chunk(pos)
            .expect("the player's own chunk must have been generated");
        assert_eq!(data.len(), REGION_BLOCKS, "re-send must be a full region");
        // A chunk that was never generated (far away) is None, not an error.
        let far = rustcraft_world::ChunkPos::new(1000, 0, 1000);
        assert!(s.resend_chunk(far).is_none(), "unknown chunk must be None");
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
        tick_n(&mut s, 240); // 4s: generation + streaming
        let updates = s.take_world_updates();
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
        let stats = s.stats();
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
        assert_eq!(s.stats().npcs, 50);
        // Re-spawning replaces the load: the count stays exact.
        s.set_npc_load(10, 8.0);
        s.push_action(Action::NpcLoad);
        tick_n(&mut s, 2);
        assert_eq!(s.agents().len(), 11, "re-load must replace, not append");
        // Clear removes everything.
        s.push_action(Action::NpcClear);
        tick_n(&mut s, 2);
        assert_eq!(s.agents().len(), 1, "only the player remains");
        assert_eq!(s.stats().npcs, 0);
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
        let c = s.stats().cache;
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
