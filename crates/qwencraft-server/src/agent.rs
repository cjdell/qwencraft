//! Agents: the player and NPCs, with simple AABB physics against the world.
//!
//! Physics queries go through each agent's [`LocalBlockCache`] first (a dense
//! local block window around the agent — steady-state lookups never touch the
//! world's chunk buffers), falling back to the world for cells outside the
//! window.

use qwencraft_world::BlockPos;

use crate::local_block_cache::LocalBlockCache;
use crate::world::World;
use crate::{Input, Vec3};

/// What an agent is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentKind {
    Player,
    Npc,
}

/// Physics constants.
pub const GRAVITY: f32 = -28.0;
pub const JUMP_VEL: f32 = 9.0;
pub const WALK_SPEED: f32 = 4.5;
pub const SPRINT_SPEED: f32 = 6.0;
/// Fly mode: base speed (blocks/s), speed range, and the multiplier applied
/// per FlyFaster/FlySlower action.
pub const FLY_BASE_SPEED: f32 = 20.0;
pub const FLY_MIN_SPEED: f32 = 5.0;
pub const FLY_MAX_SPEED: f32 = 500.0;
pub const FLY_STEP: f32 = 1.5;
/// Swim: horizontal speed factor vs walking, fall cap, and the vertical
/// speed while holding Space to rise.
pub const SWIM_SPEED_FACTOR: f32 = 0.6;
pub const SWIM_MAX_FALL: f32 = -3.5;
pub const SWIM_UP_SPEED: f32 = 4.2;
pub const HALF_W: f32 = 0.3;
pub const HEIGHT: f32 = 1.8;
pub const EYE_HEIGHT: f32 = 1.62;
const MAX_FALL: f32 = -40.0;
const EPS: f32 = 1e-4;
const MOUSE_SENS: f32 = 0.0024;

/// Snapshot of an agent for the client (rendering).
#[derive(Clone, Debug, PartialEq)]
pub struct AgentState {
    pub id: u32,
    pub is_player: bool,
    /// Display name (players: the name they chose, NPCs: empty).
    pub name: String,
    /// Feet position (world space).
    pub pos: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub on_ground: bool,
    /// Fly mode active (player only).
    pub fly: bool,
    /// Fly speed in blocks/s (player only).
    pub fly_speed: f32,
    /// Sphere colour (solid colour for now).
    pub color: [u8; 3],
    /// Sphere centre = pos + (0, radius, 0); radius below.
    pub radius: f32,
    /// The block under the player's crosshair (client draws a wireframe
    /// highlight around it); None when nothing is within break range.
    pub target: Option<BlockPos>,
}

impl Default for AgentState {
    /// A stand-in player state: used by the remote client before the first
    /// `PlayerState` message arrives.
    fn default() -> Self {
        AgentState {
            id: 0,
            is_player: true,
            name: "Player".to_string(),
            pos: crate::Vec3::new(0.0, 0.0, 0.0),
            yaw: 0.7,
            pitch: -0.15,
            on_ground: false,
            fly: false,
            fly_speed: FLY_BASE_SPEED,
            color: [255, 255, 255],
            radius: 0.42,
            target: None,
        }
    }
}

const NPC_COLORS: [[u8; 3]; 8] = [
    [214, 96, 96],
    [96, 160, 214],
    [214, 190, 96],
    [150, 214, 96],
    [190, 96, 214],
    [96, 214, 200],
    [214, 140, 90],
    [140, 120, 210],
];

/// A simulated agent.
pub struct Agent {
    pub id: u32,
    pub kind: AgentKind,
    /// Display name (players: the name they chose, NPCs: empty).
    pub name: String,
    /// Feet position.
    pub pos: Vec3,
    pub vel: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub on_ground: bool,
    /// Fly mode (player only): no gravity, no collision; Space/Shift are
    /// vertical thrust, WASD flies horizontally at `fly_speed`.
    pub fly: bool,
    /// Fly speed in blocks/s (player only).
    pub fly_speed: f32,
    /// Dense local block window around the agent (physics source).
    pub cache: LocalBlockCache,
    // NPC wander state
    npc_dir: f32,
    npc_timer: f64,
    rng: u64,
    pub color: [u8; 3],
}

impl Agent {
    pub fn player(id: u32, pos: Vec3, name: &str, color: [u8; 3]) -> Self {
        Agent {
            id,
            kind: AgentKind::Player,
            name: name.to_string(),
            pos,
            vel: Vec3::default(),
            yaw: 0.7,
            pitch: -0.15,
            on_ground: false,
            fly: false,
            fly_speed: FLY_BASE_SPEED,
            cache: LocalBlockCache::new(),
            npc_dir: 0.0,
            npc_timer: 0.0,
            rng: 0x9E37_79B9u64 ^ (id as u64 * 0x1000_0000_01B3),
            color,
        }
    }

    pub fn npc(id: u32, pos: Vec3) -> Self {
        Agent {
            id,
            kind: AgentKind::Npc,
            name: String::new(),
            pos,
            vel: Vec3::default(),
            yaw: 0.0,
            pitch: 0.0,
            on_ground: false,
            fly: false,
            fly_speed: FLY_BASE_SPEED,
            cache: LocalBlockCache::new(),
            npc_dir: (id as f32) * 1.7,
            npc_timer: 0.5 + (id as f64) * 0.3,
            rng: 0xC0FFEEu64 ^ (id as u64 * 0x9E37_79B9),
            color: NPC_COLORS[(id as usize) % NPC_COLORS.len()],
        }
    }

    fn next_rand(&mut self) -> f32 {
        // xorshift64 -> [0,1)
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x >> 40) as f32 / (1u32 << 24) as f32
    }

    pub fn state(&self) -> AgentState {
        AgentState {
            id: self.id,
            is_player: self.kind == AgentKind::Player,
            name: self.name.clone(),
            pos: self.pos,
            yaw: self.yaw,
            pitch: self.pitch,
            on_ground: self.on_ground,
            fly: self.fly,
            fly_speed: self.fly_speed,
            color: self.color,
            radius: 0.42,
            target: None,
        }
    }

    /// Toggle fly mode. Velocity is zeroed so re-entering/leaving fly does
    /// not inherit a stale horizontal velocity.
    pub fn toggle_fly(&mut self) {
        self.fly = !self.fly;
        self.vel = Vec3::default();
        if self.fly {
            self.on_ground = false;
        }
    }

    /// Scale the fly speed by `factor`, clamped to [FLY_MIN_SPEED, FLY_MAX_SPEED].
    pub fn adjust_fly_speed(&mut self, factor: f32) {
        self.fly_speed = (self.fly_speed * factor).clamp(FLY_MIN_SPEED, FLY_MAX_SPEED);
    }

    pub fn eye(&self) -> Vec3 {
        Vec3::new(self.pos.x, self.pos.y + EYE_HEIGHT, self.pos.z)
    }

    /// Camera/view direction from yaw & pitch.
    pub fn look_direction(&self) -> Vec3 {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        Vec3::new(-sy * cp, sp, -cy * cp)
    }

    /// Player movement: apply input, integrate, collide.
    pub fn step(&mut self, dt: f32, world: &mut World, move_dir: Vec3, jump: bool, input: &Input) {
        // Look.
        self.yaw -= input.mouse_dx * MOUSE_SENS;
        self.pitch -= input.mouse_dy * MOUSE_SENS;
        self.pitch = self.pitch.clamp(-1.55, 1.55);

        if self.fly {
            // Fly: no gravity, no collision (collision would tunnel at
            // high speed). Space thrusts up, Shift down, WASD flies at
            // `fly_speed` blocks/s.
            let mut v = move_dir.scale(self.fly_speed);
            if input.keys.contains(crate::Key::Space) {
                v.y += self.fly_speed;
            }
            if input.keys.contains(crate::Key::ShiftLeft) {
                v.y -= self.fly_speed;
            }
            self.pos.x += v.x * dt;
            self.pos.y += v.y * dt;
            self.pos.z += v.z * dt;
            self.vel = v;
            self.on_ground = false;
            return;
        }

        let speed = if input.keys.contains(crate::Key::ShiftLeft) {
            SPRINT_SPEED
        } else {
            WALK_SPEED
        };
        let dir = move_dir.scale(speed);
        if self.in_water(world) {
            // Swimming: slow horizontal movement, reduced gravity, capped
            // fall (a splash, not a plunge), hold Space to rise.
            let dir = dir.scale(SWIM_SPEED_FACTOR);
            self.vel.x = dir.x;
            self.vel.z = dir.z;
            self.vel.y += GRAVITY * 0.3 * dt;
            self.vel.y = self.vel.y.max(SWIM_MAX_FALL);
            if input.keys.contains(crate::Key::Space) {
                self.vel.y = SWIM_UP_SPEED;
            }
            self.move_axis(world, 0, dt);
            self.move_axis(world, 2, dt);
            self.on_ground = false;
            self.move_axis(world, 1, dt);
        } else {
            self.physics_step(dt, world, dir, jump);
        }
        self.cache.update(world, self.pos);
    }

    /// True when the agent's body centre is in water.
    pub fn in_water(&mut self, world: &mut World) -> bool {
        let body = BlockPos::new(
            self.pos.x.floor() as i32,
            (self.pos.y + 0.5).floor() as i32,
            self.pos.z.floor() as i32,
        );
        self.cache.lookup(body, world).is_water()
    }

    /// NPC wandering: pick directions, walk, occasionally pause.
    pub fn step_npc(&mut self, dt: f32, world: &mut World, _time: f64) {
        self.npc_timer -= dt as f64;
        if self.npc_timer <= 0.0 {
            let r = self.next_rand();
            self.npc_timer = 0.8 + r as f64 * 2.4;
            if r < 0.22 {
                self.npc_dir = f32::NAN; // idle marker
            } else {
                self.npc_dir = self.next_rand() * std::f32::consts::TAU;
            }
        }
        let dir = if self.npc_dir.is_nan() {
            Vec3::new(0.0, 0.0, 0.0)
        } else {
            let (s, c) = self.npc_dir.sin_cos();
            Vec3::new(-s, 0.0, -c).scale(WALK_SPEED * 0.6)
        };
        self.yaw = if self.npc_dir.is_nan() {
            self.yaw
        } else {
            self.npc_dir
        };
        if self.in_water(world) {
            // NPCs swim to the surface and keep moving.
            let dir = dir.scale(SWIM_SPEED_FACTOR);
            self.vel.x = dir.x;
            self.vel.z = dir.z;
            self.vel.y = SWIM_UP_SPEED * 0.7;
            self.move_axis(world, 0, dt);
            self.move_axis(world, 2, dt);
            self.on_ground = false;
            self.move_axis(world, 1, dt);
        } else {
            self.physics_step(dt, world, dir, false);
        }
        self.cache.update(world, self.pos);
    }

    /// Shared gravity + collision integration.
    pub fn physics_step(&mut self, dt: f32, world: &mut World, dir: Vec3, jump: bool) {
        self.vel.x = dir.x;
        self.vel.z = dir.z;
        self.vel.y += GRAVITY * dt;
        self.vel.y = self.vel.y.max(MAX_FALL);
        if jump && self.on_ground {
            self.vel.y = JUMP_VEL;
        }

        self.move_axis(world, 0, dt);
        self.move_axis(world, 2, dt);
        self.on_ground = false;
        self.move_axis(world, 1, dt);
    }

    /// Move along one axis by `vel * dt`, resolving collisions against solid
    /// blocks. (Velocity is in blocks/second; the displacement per tick is
    /// velocity scaled by the tick's `dt`.)
    fn move_axis(&mut self, world: &mut World, axis: u8, dt: f32) {
        let d = match axis {
            0 => self.vel.x * dt,
            1 => self.vel.y * dt,
            _ => self.vel.z * dt,
        };
        let mut new = self.pos;
        match axis {
            0 => new.x += d,
            1 => new.y += d,
            _ => new.z += d,
        }

        if self.collides_at(world, new) {
            // Clamp to the collision boundary.
            self.clamp_axis(world, &mut new, axis, d);
            match axis {
                1 if d < 0.0 => self.on_ground = true,
                _ => {}
            }
            self.vel.y = if axis == 1 { 0.0 } else { self.vel.y };
            self.vel.x = if axis == 0 { 0.0 } else { self.vel.x };
            self.vel.z = if axis == 2 { 0.0 } else { self.vel.z };
        }
        self.pos = new;
    }

    /// True when the agent AABB at `pos` intersects any solid block.
    fn collides_at(&mut self, world: &mut World, pos: Vec3) -> bool {
        let (x0, y0, z0) = (pos.x - HALF_W, pos.y, pos.z - HALF_W);
        let (x1, y1, z1) = (pos.x + HALF_W, pos.y + HEIGHT, pos.z + HALF_W);

        let bx0 = x0.floor() as i32;
        let bx1 = (x1 - EPS).floor() as i32;
        let by0 = y0.floor() as i32;
        let by1 = (y1 - EPS).floor() as i32;
        let bz0 = z0.floor() as i32;
        let bz1 = (z1 - EPS).floor() as i32;

        for by in by0..=by1 {
            for bz in bz0..=bz1 {
                for bx in bx0..=bx1 {
                    if self.cache.lookup(BlockPos::new(bx, by, bz), world).is_solid() {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Push `pos` back out of collision along `axis`.
    fn clamp_axis(&mut self, world: &mut World, pos: &mut Vec3, axis: u8, d: f32) {
        let (x0, y0, z0) = (pos.x - HALF_W, pos.y, pos.z - HALF_W);
        let (x1, y1, z1) = (pos.x + HALF_W, pos.y + HEIGHT, pos.z + HALF_W);
        let bx0 = x0.floor() as i32;
        let bx1 = (x1 - EPS).floor() as i32;
        let by0 = y0.floor() as i32;
        let by1 = (y1 - EPS).floor() as i32;
        let bz0 = z0.floor() as i32;
        let bz1 = (z1 - EPS).floor() as i32;

        let mut best: Option<f32> = None;
        for by in by0..=by1 {
            for bz in bz0..=bz1 {
                for bx in bx0..=bx1 {
                    if !self.cache.lookup(BlockPos::new(bx, by, bz), world).is_solid() {
                        continue;
                    }
                    // Block faces that block this axis' movement.
                    let candidate = match axis {
                        0 if d > 0.0 => bx as f32 - HALF_W - EPS,
                        0 if d < 0.0 => (bx + 1) as f32 + HALF_W + EPS,
                        1 if d > 0.0 => by as f32 - HEIGHT - EPS,
                        1 if d < 0.0 => (by + 1) as f32 + EPS,
                        2 if d > 0.0 => bz as f32 - HALF_W - EPS,
                        2 if d < 0.0 => (bz + 1) as f32 + HALF_W + EPS,
                        _ => continue,
                    };
                    best = Some(match best {
                        Some(b) if d > 0.0 => b.min(candidate),
                        Some(b) if d < 0.0 => b.max(candidate),
                        _ => candidate,
                    });
                }
            }
        }
        if let Some(v) = best {
            match axis {
                0 => pos.x = v,
                1 => pos.y = v,
                _ => pos.z = v,
            }
        }
    }
}


