//! The single pipeline's WGSL source.
//!
//! Vertices carry world-space position + baked colour (voxel lighting and AO
//! are computed at mesh build time). The vertex stage applies the
//! view-projection matrix; the fragment stage applies distance fog towards
//! the sky colour.
//!
//! Lives in the renderer (the only production consumer of WGSL here) — the
//! matching uniform-block serialization is `qwencraft_world::camera::uniform_bytes`;
//! the two must stay in lockstep (see the comment in `SHADER`).

pub const SHADER: &str = r#"
// Layout (must stay in lockstep with `uniform_bytes` in
// qwencraft_world::camera):
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
