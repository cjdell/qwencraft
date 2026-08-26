//! Shared rendering constants: the WGSL shader source and the view/projection
//! math. Lives here (instead of the wasm-only client crate) so host tests can
//! run the exact same pipeline through native wgpu.

/// The single pipeline's WGSL source.
///
/// Vertices carry world-space position + baked colour (voxel lighting and AO
/// are computed at mesh build time). The vertex stage applies the
/// view-projection matrix; the fragment stage applies distance fog towards
/// the sky colour.
pub const SHADER: &str = r#"
// Layout (must stay in lockstep with `uniform_bytes` in camera.rs):
//   view_proj: 0..64, cam: 64..80 (vec4, w unused), fog_start: 80,
//   fog_end: 84, pad: 88..96, sky: 96..112 (vec4, w unused).
// All members use 16-byte-aligned types (mat4x4 / vec4 / vec2) so WGSL
// inserts no hidden padding: the struct is exactly 112 bytes.
struct Uniforms {
    view_proj: mat4x4<f32>,
    cam: vec4<f32>,
    fog_start: f32,
    fog_end: f32,
    pad: vec2<f32>,
    sky: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) world: vec3<f32>,
};

@vertex
fn vs_main(@location(0) position: vec3<f32>, @location(1) color: vec3<f32>) -> VsOut {
    var out: VsOut;
    out.pos = u.view_proj * vec4<f32>(position, 1.0);
    out.color = color;
    out.world = position;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let d = distance(u.cam.xyz, in.world);
    let t = clamp((d - u.fog_start) / (u.fog_end - u.fog_start), 0.0, 1.0);
    let c = mix(in.color, u.sky.xyz, t);
    return vec4<f32>(c, 1.0);
}

// Water: same fog, constant translucency. Drawn after all opaque geometry
// with src-alpha blending and depth writes disabled.
@fragment
fn fs_water(in: VsOut) -> @location(0) vec4<f32> {
    let d = distance(u.cam.xyz, in.world);
    let t = clamp((d - u.fog_start) / (u.fog_end - u.fog_start), 0.0, 1.0);
    let c = mix(in.color, u.sky.xyz, t);
    return vec4<f32>(c, 0.62);
}
"#;

/// Sky / fog colour.
pub const SKY: [f32; 3] = [0.53, 0.78, 0.92];

pub const FOG_START: f32 = 70.0;
pub const FOG_END: f32 = 108.0;

/// Size of the uniform block in bytes. Layout: mat4 (0..64), cam vec4
/// (64..80), fog_start (80), fog_end (84), pad (88..96), sky vec4
/// (96..112). All 16-byte-aligned WGSL types — no hidden padding, exactly
/// 112 bytes (must match the WGSL `Uniforms` struct in `SHADER`).
pub const UNIFORM_SIZE: u64 = 112;

/// View direction for the server's yaw/pitch convention
/// (matches `Agent::look_direction`).
#[inline]
pub fn look_direction(yaw: f32, pitch: f32) -> [f32; 3] {
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    [-sy * cp, sp, -cy * cp]
}

/// View matrix (world -> camera) as a column-major `mat4x4`, for the
/// server's yaw/pitch convention.
fn view_matrix(cam: [f32; 3], yaw: f32, pitch: f32) -> [f32; 16] {
    let (sy, cy) = yaw.sin_cos();
    let f = look_direction(yaw, pitch); // forward
    let r = [cy, 0.0, -sy]; // right
    // up = cross(right, forward)
    let u = [
        r[1] * f[2] - r[2] * f[1],
        r[2] * f[0] - r[0] * f[2],
        r[0] * f[1] - r[1] * f[0],
    ];
    let dot = |a: [f32; 3]| a[0] * cam[0] + a[1] * cam[1] + a[2] * cam[2];
    // Column-major: column i is (m0i, m1i, m2i, m3i).
    [
        r[0], u[0], -f[0], 0.0, // col 0
        r[1], u[1], -f[1], 0.0, // col 1
        r[2], u[2], -f[2], 0.0, // col 2
        -dot(r), -dot(u), dot(f), 1.0, // col 3
    ]
}

/// Perspective projection with WebGPU clip-space Z in [0, 1],
/// column-major `mat4x4`.
fn projection_matrix(aspect: f32, fov_y: f32, near: f32, far: f32) -> [f32; 16] {
    let f = 1.0 / (fov_y * 0.5).tan();
    let a = f / aspect;
    // WebGPU clip space has depth in [0, 1] (not OpenGL's [-1, 1]):
    // z_ndc = z_cam/(near - far) + near/(near - far), so
    // M22 = far/(near - far), M32 = far * near/(near - far).
    [
        a, 0.0, 0.0, 0.0, // col 0
        0.0, f, 0.0, 0.0, // col 1
        0.0, 0.0, far / (near - far), -1.0, // col 2
        0.0, 0.0, (far * near) / (near - far), 0.0, // col 3
    ]
}

/// Multiply two column-major 4x4 matrices: `a * b`.
fn mul4(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut out = [0.0f32; 16];
    for c in 0..4 {
        for r in 0..4 {
            let mut s = 0.0f32;
            for k in 0..4 {
                s += a[k * 4 + r] * b[c * 4 + k];
            }
            out[c * 4 + r] = s;
        }
    }
    out
}

/// Full view-projection matrix for a first-person camera.
///
/// `fov_y` is the vertical field of view in radians.
pub fn view_projection(
    cam: [f32; 3],
    yaw: f32,
    pitch: f32,
    aspect: f32,
    fov_y: f32,
    near: f32,
    far: f32,
) -> [f32; 16] {
    mul4(&projection_matrix(aspect, fov_y, near, far), &view_matrix(cam, yaw, pitch))
}

/// Serialize the uniform block to little-endian bytes.
pub fn uniform_bytes(
    view_proj: &[f32; 16],
    cam: [f32; 3],
    fog_start: f32,
    fog_end: f32,
    sky: [f32; 3],
) -> [u8; UNIFORM_SIZE as usize] {
    let mut out = [0u8; UNIFORM_SIZE as usize];
    let mut o = 0usize;
    for v in view_proj {
        out[o..o + 4].copy_from_slice(&v.to_le_bytes());
        o += 4;
    }
    let mut push = |o: &mut usize, v: f32| {
        out[*o..*o + 4].copy_from_slice(&v.to_le_bytes());
        *o += 4;
    };
    for v in cam {
        push(&mut o, v);
    }
    push(&mut o, 0.0); // cam.w
    push(&mut o, fog_start);
    push(&mut o, fog_end);
    push(&mut o, 0.0); // pad x
    push(&mut o, 0.0); // pad y
    for v in sky {
        push(&mut o, v);
    }
    push(&mut o, 0.0); // sky.w
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_view_when_facing_minus_z() {
        // yaw=0, pitch=0: looking down -Z, right=+X, up=+Y.
        let v = view_matrix([0.0; 3], 0.0, 0.0);
        // Column-major: v[c*4+r].
        assert_eq!(v[0], 1.0); // col0.x = r.x
        assert_eq!(v[5], 1.0); // col1.y = u.y
        assert_eq!(v[10], 1.0); // col2.z = -f.z (f = -Z)
        assert_eq!(v[15], 1.0);
    }

    #[test]
    fn look_dir_matches_server_convention() {
        let f = look_direction(0.0, 0.0);
        assert!((f[0] - 0.0).abs() < 1e-6);
        assert!((f[1] - 0.0).abs() < 1e-6);
        assert!((f[2] - -1.0).abs() < 1e-6);
        // yaw = pi/2: looking down -X.
        let f = look_direction(std::f32::consts::FRAC_PI_2, 0.0);
        assert!((f[0] - -1.0).abs() < 1e-6);
        assert!((f[2] - 0.0).abs() < 1e-6);
    }

    #[test]
    fn projection_maps_near_far_to_zero_one() {
        let p = projection_matrix(1.6, 1.2, 0.1, 300.0);
        // z' = p[10] * z + p[14]; w' = -z. In NDC, z_ndc = z'/w'.
        // At z = -near (near plane, in front of camera): ndc z should be 0.
        let zc = -0.1f32;
        let znear = (p[10] * zc + p[14]) / -zc;
        let zfar = (p[10] * -300.0 + p[14]) / 300.0;
        assert!((znear - 0.0).abs() < 1e-4, "near {znear}");
        assert!((zfar - 1.0).abs() < 1e-3, "far {zfar}");
    }

    #[test]
    fn uniform_layout() {
        let vp = [0.5f32; 16];
        let b = uniform_bytes(&vp, [1.0, 2.0, 3.0], 70.0, 108.0, [0.5, 0.6, 0.7]);
        assert_eq!(b.len(), 112);
        // cam at 64
        assert_eq!(&b[64..68], &1.0f32.to_le_bytes());
        assert_eq!(&b[68..72], &2.0f32.to_le_bytes());
        assert_eq!(&b[72..76], &3.0f32.to_le_bytes());
        assert_eq!(&b[76..80], &0.0f32.to_le_bytes()); // cam.w
        // fog at 80 / 84
        assert_eq!(&b[80..84], &70.0f32.to_le_bytes());
        assert_eq!(&b[84..88], &108.0f32.to_le_bytes());
        // pad at 88..96
        assert!(b[88..96].iter().all(|&x| x == 0));
        // sky at 96
        assert_eq!(&b[96..100], &0.5f32.to_le_bytes());
        assert_eq!(&b[100..104], &0.6f32.to_le_bytes());
        assert_eq!(&b[104..108], &0.7f32.to_le_bytes());
        assert_eq!(&b[108..112], &0.0f32.to_le_bytes()); // sky.w
    }
}
