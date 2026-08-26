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
