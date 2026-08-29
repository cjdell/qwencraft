// Layout (must stay in lockstep with `uniform_bytes` in
// qwencraft_world::camera):
//   view_proj: 0..64, cam: 64..80 (vec4, w unused), fog_start: 80,
//   fog_end: 84, time: 88..96 (vec2, x = seconds, y unused),
//   sky: 96..112 (vec4, w unused).
// All members use 16-byte-aligned types (mat4x4 / vec4 / vec2) so WGSL
// inserts no hidden padding: the struct is exactly 112 bytes.
struct Uniforms {
    view_proj: mat4x4<f32>,
    cam: vec4<f32>,
    fog_start: f32,
    fog_end: f32,
    time: vec2<f32>,
    sky: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) light: f32,
    @location(1) uv: vec2<f32>,
    @location(2) tex: f32,
    @location(3) world: vec3<f32>,
};

@vertex
fn vs_main(
    @location(0) position: vec3<f32>,
    @location(1) light: f32,
    @location(2) uv: vec2<f32>,
    @location(3) tex: f32,
) -> VsOut {
    var out: VsOut;
    out.pos = u.view_proj * vec4<f32>(position, 1.0);
    out.light = light;
    out.uv = uv;
    out.tex = tex;
    out.world = position;
    return out;
}

// Opaque terrain (and the highlight wireframe, which carries
// TEX_HIGHLIGHT through the same path).
//
// Note the vector clamp bounds: WGSL's clamp has no scalar broadcasting
// (unlike GLSL) — the bounds must be the same type as the value,
// and bare `0.0`/`1.0` are *abstract-float* literals that Dawn rejects
// mixed with concrete f32/vec3<f32> arguments.
@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let c = clamp(sample_tex(in.tex, in.uv, in.world, u.time.x) * in.light, vec3<f32>(0.0f), vec3<f32>(1.0f));
    let d = distance(u.cam.xyz, in.world);
    let t = clamp((d - u.fog_start) / (u.fog_end - u.fog_start), 0.0f, 1.0f);
    return vec4<f32>(mix(c, u.sky.xyz, t), 1.0);
}

// Translucent pass (water + glass): same sampling, per-texture alpha.
// Drawn after all opaque geometry with src-alpha blending and depth
// writes disabled.
@fragment
fn fs_water(in: VsOut) -> @location(0) vec4<f32> {
    let c = clamp(sample_tex(in.tex, in.uv, in.world, u.time.x) * in.light, vec3<f32>(0.0f), vec3<f32>(1.0f));
    let d = distance(u.cam.xyz, in.world);
    let t = clamp((d - u.fog_start) / (u.fog_end - u.fog_start), 0.0f, 1.0f);
    return vec4<f32>(mix(c, u.sky.xyz, t), tex_trans_alpha(in.tex));
}

// ---- agent spheres (baked per-vertex colour, not block textures) --------

struct VsOutAgent {
    @builtin(position) pos: vec4<f32>,
    @location(0) color: vec3<f32>,
    @location(1) world: vec3<f32>,
};

@vertex
fn vs_agent(
    @location(0) position: vec3<f32>,
    @location(1) color: vec3<f32>,
) -> VsOutAgent {
    var out: VsOutAgent;
    out.pos = u.view_proj * vec4<f32>(position, 1.0);
    out.color = color;
    out.world = position;
    return out;
}

@fragment
fn fs_agent(in: VsOutAgent) -> @location(0) vec4<f32> {
    let d = distance(u.cam.xyz, in.world);
    let t = clamp((d - u.fog_start) / (u.fog_end - u.fog_start), 0.0f, 1.0f);
    return vec4<f32>(mix(in.color, u.sky.xyz, t), 1.0);
}
