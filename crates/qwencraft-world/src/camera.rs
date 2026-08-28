//! View/projection math + the GPU uniform-block layout.
//!
//! Pure (host-testable), and used beyond the renderer: the server's spawn
//! test projects world points through `view_projection`/`project_point` to
//! pin that new players appear on screen. The WGSL shader source itself
//! lives in the wasm-only client crate (`qwencraft-client/src/shader.rs`),
//! whose `Uniforms` struct must stay in lockstep with `uniform_bytes` here.

/// Sky / fog colour.
pub const SKY: [f32; 3] = [0.53, 0.78, 0.92];

pub const FOG_START: f32 = 70.0;
pub const FOG_END: f32 = 108.0;

/// Size of the uniform block in bytes. Layout: mat4 (0..64), cam vec4
/// (64..80), fog_start (80), fog_end (84), pad (88..96), sky vec4
/// (96..112). All 16-byte-aligned WGSL types — no hidden padding, exactly
/// 112 bytes (must match the WGSL `Uniforms` struct in the client's
/// `shader.rs`).
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

/// Project a world point through a column-major view-projection matrix to
/// screen coordinates, in pixels of a `w x h` viewport.
///
/// WebGPU clip space: NDC x right, y up, z in [0, 1]; screen y is flipped
/// (CSS grows downward). `None` when the point is behind the camera or
/// well outside the screen (±20% cull margin).
///
/// Screen-space DOM overlays (the other players' name tags) use this with
/// the renderer's exact view-projection inputs — it must stay in lockstep
/// with `view_projection` (same fov/near/far, same camera state), or the
/// tags drift off their spheres.
pub fn project_point(vp: &[f32; 16], p: [f32; 3], w: f32, h: f32) -> Option<(f32, f32)> {
    let x = vp[0] * p[0] + vp[4] * p[1] + vp[8] * p[2] + vp[12];
    let y = vp[1] * p[0] + vp[5] * p[1] + vp[9] * p[2] + vp[13];
    let cw = vp[3] * p[0] + vp[7] * p[1] + vp[11] * p[2] + vp[15];
    if cw <= 1e-4 {
        return None; // behind the camera
    }
    let (nx, ny) = (x / cw, y / cw);
    if nx.abs() > 1.2 || ny.abs() > 1.2 {
        return None; // well off screen
    }
    Some((
        (nx * 0.5 + 0.5) * w,
        (0.5 - ny * 0.5) * h,
    ))
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
    fn point_ahead_projects_to_screen_centre() {
        // Camera at the origin, looking down -Z (yaw=0, pitch=0). The
        // renderer's exact parameters (see `render_pass_into` in the
        // client crate).
        let m = view_projection([0.0, 0.0, 0.0], 0.0, 0.0, 16.0 / 9.0, 1.15, 0.1, 300.0);
        let (sx, sy) = project_point(&m, [0.0, 0.0, -10.0], 1280.0, 720.0)
            .expect("point 10 blocks ahead is on screen");
        assert!((sx - 640.0).abs() < 1.0, "sx {sx}");
        assert!((sy - 360.0).abs() < 1.0, "sy {sy}");
    }

    #[test]
    fn right_is_right_and_up_is_up() {
        // The classic name-tag bugs (tags "moving the other way") come
        // from a flipped axis or a transposed matrix — pin all three.
        let m = view_projection([0.0, 0.0, 0.0], 0.0, 0.0, 16.0 / 9.0, 1.15, 0.1, 300.0);
        let (cx, cy) = project_point(&m, [0.0, 0.0, -10.0], 1280.0, 720.0).unwrap();
        // +X is the camera's right (yaw=0, right = [1,0,0]). (45° off the
        // view axis — inside the ±49° horizontal half-fov.)
        let (rx, ry) = project_point(&m, [10.0, 0.0, -10.0], 1280.0, 720.0).unwrap();
        assert!(rx > cx + 100.0, "right point must land right of centre: {rx} vs {cx}");
        assert!((ry - cy).abs() < 20.0, "side point stays at the same height: {ry} vs {cy}");
        // +Y is up on screen (smaller CSS y). (26.6° elevation — inside
        // the ±32.9° vertical half-fov.)
        let (ux, uy) = project_point(&m, [0.0, 5.0, -10.0], 1280.0, 720.0).unwrap();
        assert!(uy < cy - 50.0, "up point must land above centre: {uy} vs {cy}");
        assert!((ux - cx).abs() < 20.0, "up point stays centred horizontally: {ux} vs {cx}");
        // A nearer point projects further from centre than a far one.
        let (nx, _ny) = project_point(&m, [5.0, 0.0, -5.0], 1280.0, 720.0).unwrap();
        let (fx, _fy) = project_point(&m, [5.0, 0.0, -10.0], 1280.0, 720.0).unwrap();
        assert!(nx > fx, "nearer point projects further from centre: {nx} vs {fx}");
    }

    #[test]
    fn behind_the_camera_is_culled() {
        let m = view_projection([0.0, 0.0, 0.0], 0.0, 0.0, 16.0 / 9.0, 1.15, 0.1, 300.0);
        assert!(project_point(&m, [0.0, 0.0, 10.0], 1280.0, 720.0).is_none());
    }

    #[test]
    fn yaw_rotation_moves_the_scene_consistently() {
        // Turn the camera 45° left (yaw = +pi/4 looks down -X-Z): +X is
        // now 135° off the view axis (BEHIND the camera), and -Z is 45° to
        // its right — the scene must move the same way the camera turns.
        let m = view_projection(
            [0.0, 0.0, 0.0],
            std::f32::consts::FRAC_PI_4,
            0.0,
            16.0 / 9.0,
            1.15,
            0.1,
            300.0,
        );
        assert!(
            project_point(&m, [10.0, 0.0, 0.0], 1280.0, 720.0).is_none(),
            "+X is behind the camera at yaw +45°"
        );
        let (sx, _sy) = project_point(&m, [0.0, 0.0, -10.0], 1280.0, 720.0)
            .expect("-Z is now to the right (inside the ±49° half-fov)");
        assert!(sx > 640.0, "sx {sx}");
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
