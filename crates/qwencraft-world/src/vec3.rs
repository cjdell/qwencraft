//! Minimal 3D vector (avoids a glam dependency across all crates).
//!
//! Lives in the pure world crate because it is shared by every other
//! crate: the server's physics (`Agent` positions/velocities), the wire
//! protocol (`AgentState`), and the renderers that consume snapshots.

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
