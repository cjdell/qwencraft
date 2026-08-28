//! Client input state sent to the server each frame.

use crate::Vec3;

/// Keys relevant to movement/actions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    W,
    A,
    S,
    D,
    Space,
    ShiftLeft,
    Key0,
    /// Toggle fly mode (one-shot, see Action::ToggleFly).
    F,
    /// Fly speed up (one-shot, see Action::FlyFaster).
    E,
    /// Fly speed down (one-shot, see Action::FlySlower).
    Q,
    /// Spawn the configured NPC load (one-shot, see Action::NpcLoad).
    KeyN,
    /// Clear all NPCs (one-shot, see Action::NpcClear).
    KeyC,
    /// NPC load count up (one-shot, see Action::NpcCountUp).
    KeyI,
    /// NPC load count down (one-shot, see Action::NpcCountDown).
    KeyU,
    /// NPC load spacing down (one-shot, see Action::NpcSpacingDown).
    BracketLeft,
    /// NPC load spacing up (one-shot, see Action::NpcSpacingUp).
    BracketRight,
}

/// One-shot actions (block editing, fly mode), queued by the client on key/mouse events.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    /// Break the targeted block. `yaw`/`pitch` are the camera aim *at the
    /// moment of the click*; the server raycasts with exactly that aim (not
    /// its current one, which may already include mouse deltas that arrived
    /// after the click — that lag is what made edits land off-target while
    /// moving).
    Break { yaw: f32, pitch: f32 },
    /// Place a stone block against the targeted face (same aim semantics).
    Place { yaw: f32, pitch: f32 },
    /// Toggle the player's fly mode.
    ToggleFly,
    /// Increase the fly speed (x FLY_STEP, clamped).
    FlyFaster,
    /// Decrease the fly speed (/ FLY_STEP, clamped).
    FlySlower,
    /// Spawn the configured NPC load (count + spacing from the server's
    /// `npc_count` / `npc_spacing`, adjusted by the Npc* dials). Replaces
    /// the existing NPC set so the count is exact — a load-test facility.
    NpcLoad,
    /// Remove all NPCs (the player remains).
    NpcClear,
    /// Double the configured NPC count (clamped).
    NpcCountUp,
    /// Halve the configured NPC count (clamped).
    NpcCountDown,
    /// Double the configured NPC spacing (clamped).
    NpcSpacingUp,
    /// Halve the configured NPC spacing (clamped).
    NpcSpacingDown,
}

/// Current input snapshot (level-triggered keys + accumulated look deltas).
#[derive(Clone, Copy, Debug, Default)]
pub struct Input {
    pub keys: KeySet,
    /// Accumulated horizontal mouse movement (pixels) since last frame.
    pub mouse_dx: f32,
    /// Accumulated vertical mouse movement (pixels) since last frame.
    pub mouse_dy: f32,
}

/// Small fixed-size key set.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeySet {
    bits: u32,
}

impl KeySet {
    const fn bit(k: Key) -> u32 {
        1u32 << (match k {
            Key::W => 0,
            Key::A => 1,
            Key::S => 2,
            Key::D => 3,
            Key::Space => 4,
            Key::ShiftLeft => 5,
            Key::Key0 => 6,
            Key::F => 7,
            Key::E => 8,
            Key::Q => 9,
            Key::KeyN => 10,
            Key::KeyC => 11,
            Key::KeyI => 12,
            Key::KeyU => 13,
            Key::BracketLeft => 14,
            Key::BracketRight => 15,
        })
    }

    pub fn insert(&mut self, k: Key) {
        self.bits |= Self::bit(k);
    }

    pub fn remove(&mut self, k: Key) {
        self.bits &= !Self::bit(k);
    }

    pub fn contains(&self, k: Key) -> bool {
        self.bits & Self::bit(k) != 0
    }

    /// Raw key bitmask (wire format).
    pub fn bits(&self) -> u32 {
        self.bits
    }

    /// Construct from a raw key bitmask (wire format).
    pub fn from_bits(bits: u32) -> Self {
        Self { bits }
    }
}

impl Input {
    /// Movement direction in world space for the given yaw (radians).
    /// Yaw 0 looks towards -Z; positive yaw turns left.
    pub fn move_direction(&self, yaw: f32) -> Vec3 {
        let mut x = 0.0f32;
        let mut z = 0.0f32;
        let (sin, cos) = yaw.sin_cos();
        // Forward = (-sin, 0, -cos); Right = (cos, 0, -sin).
        if self.keys.contains(Key::W) {
            x += -sin;
            z += -cos;
        }
        if self.keys.contains(Key::S) {
            x -= -sin;
            z -= -cos;
        }
        if self.keys.contains(Key::D) {
            x += cos;
            z += -sin;
        }
        if self.keys.contains(Key::A) {
            x -= cos;
            z -= -sin;
        }
        Vec3::new(x, 0.0, z).normalize()
    }
}
