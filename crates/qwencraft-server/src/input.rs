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
    /// Place the selected `block` against the targeted face (same aim
    /// semantics). The server validates the id — unknown ids are ignored
    /// (a stale client can't corrupt the world).
    Place { yaw: f32, pitch: f32, block: u8 },
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
    /// Touch joystick: right(+)/left(-) component, magnitude ≤ 1.
    /// (0, 0) = no analog input (keyboard/mouse clients use the key bits).
    pub analog_x: f32,
    /// Touch joystick: forward(+)/back(-) component, magnitude ≤ 1.
    pub analog_y: f32,
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
    ///
    /// Analog-first: a non-zero joystick vector (the mobile move pad) moves
    /// at exactly that magnitude — the stick's distance from centre is the
    /// throttle (0..=1 × walk speed, NOT renormalised, unlike the key path
    /// where a diagonal costs no speed). A zero vector falls back to the
    /// WASD key bits (binary, normalised, 8-way).
    pub fn move_direction(&self, yaw: f32) -> Vec3 {
        let (ax, ay) = (self.analog_x, self.analog_y);
        let mag = (ax * ax + ay * ay).sqrt();
        if mag > 0.05 {
            let throttle = mag.min(1.0);
            let inv = 1.0 / mag;
            let right = ax * inv;
            let fwd = ay * inv;
            let (sin, cos) = yaw.sin_cos();
            // Forward = (-sin, 0, -cos); Right = (cos, 0, -sin).
            return Vec3::new(
                (fwd * -sin + right * cos) * throttle,
                0.0,
                (fwd * -cos + right * -sin) * throttle,
            );
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn analog(x: f32, y: f32) -> Input {
        let mut i = Input::default();
        i.analog_x = x;
        i.analog_y = y;
        i
    }

    #[test]
    fn analog_stick_moves_relative_to_yaw() {
        // Yaw 0 looks down -Z: full forward stick → (0, 0, -1).
        let d = analog(0.0, 1.0).move_direction(0.0);
        assert!((d.x - 0.0).abs() < 1e-4 && (d.z - -1.0).abs() < 1e-4);
        // Full right stick → +X.
        let d = analog(1.0, 0.0).move_direction(0.0);
        assert!((d.x - 1.0).abs() < 1e-4 && (d.z - 0.0).abs() < 1e-4);
        // Yaw π/2 turns left → looking down -X; forward stick → -X.
        let d = analog(0.0, 1.0).move_direction(std::f32::consts::FRAC_PI_2);
        assert!((d.x - -1.0).abs() < 1e-4 && (d.z - 0.0).abs() < 1e-4);
    }

    #[test]
    fn analog_magnitude_is_the_throttle() {
        // Half forward stick → half speed, NOT renormalised to full.
        let d = analog(0.0, 0.5).move_direction(0.0);
        assert!((d.z - -0.5).abs() < 1e-4, "half stick must halve speed: {d:?}");
        // Oversized vectors clamp to unit magnitude.
        let d = analog(3.0, 4.0).move_direction(0.0);
        let m = (d.x * d.x + d.z * d.z).sqrt();
        assert!((m - 1.0).abs() < 1e-4);
    }

    #[test]
    fn analog_deadzone_falls_back_to_keys() {
        // Centred-stick drift below the deadzone is "no input", and a zero
        // stick uses the WASD bits exactly as before.
        let d = analog(0.03, 0.0).move_direction(0.0);
        assert!(d.length() < 1e-4);
        let mut i = Input::default();
        i.keys.insert(Key::W);
        let d = i.move_direction(0.0);
        assert!((d.z - -1.0).abs() < 1e-4, "key fallback broken: {d:?}");
        // Analog takes priority over keys when both are present.
        let mut i = analog(0.0, 1.0);
        i.keys.insert(Key::W);
        i.keys.insert(Key::A);
        let d = i.move_direction(0.0);
        assert!((d.x - 0.0).abs() < 1e-4 && (d.z - -1.0).abs() < 1e-4);
    }
}
