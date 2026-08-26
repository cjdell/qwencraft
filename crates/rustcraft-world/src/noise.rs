//! Deterministic, dependency-free value noise (2D/3D) with fBm.
//!
//! All functions are pure functions of their inputs so the same seed always
//! produces the same world on every platform (including wasm).

/// 64-bit hash mixing (splitmix64).
#[inline]
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Hash integer lattice coords + seed to a u64 in `[0, 2^32)`.
#[inline]
fn hash3(seed: u64, x: i32, y: i32, z: i32) -> u32 {
    let mut h = seed;
    h = h
        .wrapping_mul(0x9E37_79B9u64)
        .wrapping_add(((x as u32).rotate_left(13).wrapping_mul(0x85EBC_A6B)) as u64);
    h = h
        .wrapping_mul(0xC2B2_AE35u64)
        .wrapping_add(((y as u32).rotate_left(7).wrapping_mul(0x27D4_EB2F)) as u64);
    h = h
        .wrapping_mul(0x1656_67B1u64)
        .wrapping_add(((z as u32).wrapping_mul(0x9E37_79B9)) as u64);
    (splitmix64(h) >> 32) as u32
}

/// Hash 2D lattice coords + seed to u64 in `[0, 2^32)` (uses a fixed z lane).
#[inline]
fn hash2(seed: u64, x: i32, z: i32) -> u32 {
    hash3(seed ^ 0xA5A5_5A5A, x, 0, z)
}

#[inline]
fn lerp(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t) // smoothstep
}

/// Seeded noise source. Cheap to clone (one u64).
#[derive(Clone, Copy, Debug)]
pub struct Noise {
    pub seed: u64,
}

impl Noise {
    pub fn new(seed: u64) -> Self {
        Self { seed }
    }

    /// 2D value noise in `[-1, 1]`.
    pub fn noise2(&self, x: f32, z: f32) -> f32 {
        let xi = x.floor() as i32;
        let zi = z.floor() as i32;
        let xf = (x - x.floor()) as f32;
        let zf = (z - z.floor()) as f32;

        let v00 = hash2(self.seed, xi, zi) as f32 / u32::MAX as f32 * 2.0 - 1.0;
        let v10 = hash2(self.seed, xi + 1, zi) as f32 / u32::MAX as f32 * 2.0 - 1.0;
        let v01 = hash2(self.seed, xi, zi + 1) as f32 / u32::MAX as f32 * 2.0 - 1.0;
        let v11 = hash2(self.seed, xi + 1, zi + 1) as f32 / u32::MAX as f32 * 2.0 - 1.0;

        let u = lerp(xf);
        let v = lerp(zf);
        let a = v00 + (v10 - v00) * u;
        let b = v01 + (v11 - v01) * u;
        a + (b - a) * v
    }

    /// 3D value noise in `[-1, 1]`.
    pub fn noise3(&self, x: f32, y: f32, z: f32) -> f32 {
        let xi = x.floor() as i32;
        let yi = y.floor() as i32;
        let zi = z.floor() as i32;
        let xf = x - x.floor();
        let yf = y - y.floor();
        let zf = z - z.floor();

        let corners = [
            hash3(self.seed, xi, yi, zi),
            hash3(self.seed, xi + 1, yi, zi),
            hash3(self.seed, xi, yi + 1, zi),
            hash3(self.seed, xi + 1, yi + 1, zi),
            hash3(self.seed, xi, yi, zi + 1),
            hash3(self.seed, xi + 1, yi, zi + 1),
            hash3(self.seed, xi, yi + 1, zi + 1),
            hash3(self.seed, xi + 1, yi + 1, zi + 1),
        ]
        .map(|h| h as f32 / u32::MAX as f32 * 2.0 - 1.0);

        let u = lerp(xf);
        let v = lerp(yf);
        let w = lerp(zf);

        let c000 = corners[0];
        let c100 = corners[1];
        let c010 = corners[2];
        let c110 = corners[3];
        let c001 = corners[4];
        let c101 = corners[5];
        let c011 = corners[6];
        let c111 = corners[7];

        let x00 = c000 + (c100 - c000) * u;
        let x10 = c010 + (c110 - c010) * u;
        let x01 = c001 + (c101 - c001) * u;
        let x11 = c011 + (c111 - c011) * u;
        let y0 = x00 + (x10 - x00) * v;
        let y1 = x01 + (x11 - x01) * v;
        y0 + (y1 - y0) * w
    }

    /// Fractal Brownian motion of 2D noise, roughly in `[-1, 1]`.
    pub fn fbm2(&self, x: f32, z: f32, octaves: u32) -> f32 {
        let mut sum = 0.0f32;
        let mut amp = 1.0f32;
        let mut freq = 1.0f32;
        let mut norm = 0.0f32;
        for _ in 0..octaves {
            sum += self.noise2(x * freq, z * freq) * amp;
            norm += amp;
            amp *= 0.5;
            freq *= 2.03;
        }
        sum / norm
    }

    /// Fractal Brownian motion of 3D noise, roughly in `[-1, 1]`.
    pub fn fbm3(&self, x: f32, y: f32, z: f32, octaves: u32) -> f32 {
        let mut sum = 0.0f32;
        let mut amp = 1.0f32;
        let mut freq = 1.0f32;
        let mut norm = 0.0f32;
        for _ in 0..octaves {
            sum += self.noise3(x * freq, y * freq, z * freq) * amp;
            norm += amp;
            amp *= 0.5;
            freq *= 2.03;
        }
        sum / norm
    }

    /// Deterministic pseudo-random u32 for a 2D lattice cell (e.g. per-column
    /// variation such as trees later).
    pub fn rand2(&self, x: i32, z: i32) -> u32 {
        hash2(self.seed ^ 0x1234_5678, x, z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let a = Noise::new(42);
        let b = Noise::new(42);
        for i in 0..200 {
            let x = (i as f32) * 0.137;
            let z = (i as f32) * 0.791;
            assert_eq!(a.noise2(x, z), b.noise2(x, z));
            assert_eq!(a.noise3(x, x * 0.5, z), b.noise3(x, x * 0.5, z));
        }
    }

    #[test]
    fn range() {
        let n = Noise::new(7);
        for i in 0..500 {
            let x = (i as f32) * 0.311;
            let z = (i as f32) * -0.177;
            assert!(n.noise2(x, z).abs() <= 1.0);
            assert!(n.noise3(x, z, x * 0.7).abs() <= 1.0);
            assert!(n.fbm2(x * 0.01, z * 0.01, 4).abs() <= 1.0);
        }
    }

    #[test]
    fn seeds_differ() {
        let a = Noise::new(1);
        let b = Noise::new(2);
        let mut diff = 0;
        for i in 0..100 {
            if a.noise2(i as f32 * 0.1, i as f32 * 0.2) != b.noise2(i as f32 * 0.1, i as f32 * 0.2) {
                diff += 1;
            }
        }
        assert!(diff > 50, "seeds should produce different noise");
    }
}
