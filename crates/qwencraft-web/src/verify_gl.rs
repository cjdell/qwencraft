//! WebGL2 "shadow" renderer for headless pixel verification.
//!
//! Headless Chromium cannot composite a WebGPU canvas into screenshots and
//! rejects `buffer.map()` readbacks, so the WebGPU output pixels cannot be
//! inspected directly. WebGL2, however, works fine headless (SwiftShader)
//! and `readPixels()` works.
//!
//! In verify mode we re-render the *exact same scene* — the same CPU meshes
//! (built from the same streamed region payloads) with the same shared
//! view-projection math — into a WebGL2 context whose GLSL shader is a
//! literal translation of the WGSL pipeline (`qwencraft-client/src/shader.rs`
//! + `textures.rs`), then read the pixels back. The procedural block
//! textures are mirrored here in GLSL (one function per `TEX_*` id, same
//! math) so the pixel checks actually see the textured scene. The WGSL
//! itself is separately verified by the browser compiling the real WebGPU
//! pipeline at startup (a shader error fails renderer init).
//!
//! The shadow renderer runs the water texture at `time = 0` (still water)
//! so its pixels are deterministic across runs.

use std::collections::HashMap;

use wasm_bindgen::JsCast;

use qwencraft_world::camera::{view_projection, FOG_END, FOG_START, SKY};
use qwencraft_world::mesh::{build_chunk_mesh, highlight_vertices};
use qwencraft_world::{ChunkPos, REGION_BLOCKS};

const VS: &str = r#"#version 300 es
layout(location=0) in vec3 a_pos;
layout(location=1) in float a_light;
layout(location=2) in vec2 a_uv;
layout(location=3) in float a_tex;
uniform mat4 u_vp;
out float v_light;
out vec2 v_uv;
out float v_tex;
out vec3 v_world;
void main() {
    gl_Position = u_vp * vec4(a_pos, 1.0);
    v_light = a_light;
    v_uv = a_uv;
    v_tex = a_tex;
    v_world = a_pos;
}
"#;

/// The procedural texture library — a mechanical GLSL translation of
/// `qwencraft-client/src/textures.rs` (same function per `TEX_*` id, same
/// math: fract/mix/smoothstep/step/sin/atan/length/dot/floor/abs are
/// shared by WGSL and GLSL ES 3.00; the two-arg atan and the float
/// remainder live in the shared tex_atan2 / tex_fmod helpers, since
/// WGSL's atan is single-arg (Dawn rejects the two-arg form) and GLSL
/// ES 3.00's `%` is integer-only (float remainder is mod())). Kept in a
/// separate const so the opaque and translucent programs share it
/// verbatim.
const TEX_LIB: &str = r#"
float tex_hash1(float n) { return fract(sin(n) * 43758.5453123); }
float tex_block_rand(vec3 world, float salt) {
    vec3 c = floor(world);
    return tex_hash1(dot(c, vec3(127.1, 311.7, 74.7)) + salt * 57.31);
}
float tex_atan2(float y, float x) {
    if (x == 0.0 && y == 0.0) { return 0.0; }
    float a = atan(y / x);
    if (x < 0.0) { return a + (y < 0.0 ? -3.14159265 : 3.14159265); }
    return a;
}
float tex_fmod(float a, float b) { return mod(a, b); }
float tex_vnoise(vec2 p) {
    vec2 i = floor(p);
    vec2 f = fract(p);
    vec2 u = f * f * (3.0 - 2.0 * f);
    float a = tex_hash1(dot(i, vec2(127.1, 311.7)));
    float b = tex_hash1(dot(i + vec2(1.0, 0.0), vec2(127.1, 311.7)));
    float c = tex_hash1(dot(i + vec2(0.0, 1.0), vec2(127.1, 311.7)));
    float d = tex_hash1(dot(i + vec2(1.0, 1.0), vec2(127.1, 311.7)));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}
vec3 tex_grass_top(vec2 uv, vec3 world, float time) {
    float r = tex_block_rand(world, 1.0);
    float n = tex_vnoise(world.xz * 6.0 + r * 17.0);
    float fine = tex_vnoise(world.xz * 22.0 + r * 31.0);
    vec3 base = mix(vec3(0.27, 0.50, 0.20), vec3(0.40, 0.66, 0.27), n);
    return mix(base, vec3(0.47, 0.74, 0.34), smoothstep(0.72, 0.95, fine));
}
vec3 tex_dirt(vec2 uv, vec3 world, float time) {
    float n = tex_vnoise(uv * 6.0 + vec2(tex_block_rand(world, 3.0) * 11.0, tex_block_rand(world, 4.0) * 7.0));
    float n2 = tex_vnoise(uv * 18.0 + vec2(tex_block_rand(world, 5.0) * 23.0, tex_block_rand(world, 6.0) * 13.0));
    vec3 base = mix(vec3(0.47, 0.33, 0.21), vec3(0.60, 0.44, 0.30), n);
    return base * (0.90 + 0.20 * n2);
}
vec3 tex_grass_side(vec2 uv, vec3 world, float time) {
    vec3 dirt = tex_dirt(uv, world, time);
    float r = tex_block_rand(world, 7.0);
    float rag = tex_vnoise(vec2(uv.x * 5.0, r * 13.0));
    float h = 0.16 + 0.10 * rag;
    float rim = smoothstep(1.0 - h - 0.03, 1.0 - h, uv.y);
    float mottle = tex_vnoise(vec2(uv.x * 9.0, uv.y * 14.0 + r * 3.0));
    vec3 grass = vec3(0.30, 0.56, 0.22) * (0.85 + 0.30 * mottle);
    return mix(dirt, grass, rim);
}
vec3 tex_stone(vec2 uv, vec3 world, float time) {
    float r = tex_block_rand(world, 8.0);
    float n = tex_vnoise(uv * 4.0 + vec2(r * 13.0, r * 7.0));
    float n2 = tex_vnoise(uv * 11.0 + vec2(r * 29.0, r * 17.0));
    vec3 base = mix(vec3(0.45, 0.46, 0.48), vec3(0.58, 0.59, 0.62), n);
    float crack = 1.0 - smoothstep(0.02, 0.10, n2);
    return base * (1.0 - 0.35 * crack);
}
vec3 tex_sand(vec2 uv, vec3 world, float time) {
    float r = tex_block_rand(world, 9.0);
    float n = tex_vnoise(uv * 14.0 + vec2(r * 19.0, r * 11.0));
    vec3 base = mix(vec3(0.83, 0.77, 0.55), vec3(0.91, 0.86, 0.64), n);
    float speck = tex_vnoise(uv * 40.0 + vec2(r * 7.0, r * 5.0));
    return base * (0.94 + 0.12 * speck);
}
vec3 tex_water(vec2 uv, vec3 world, float time) {
    float w1 = sin(world.x * 4.7 + time * 1.9 + sin(world.z * 3.3 + time * 1.3) * 0.8);
    float w2 = sin(world.z * 5.3 - time * 1.7 + world.x * 2.1);
    float ripple = 0.5 + 0.5 * (w1 * 0.6 + w2 * 0.4);
    vec3 base = vec3(0.20, 0.40, 0.80);
    vec3 light = vec3(0.32, 0.55, 0.92);
    return mix(base, light, ripple * 0.7);
}
vec3 tex_log_side(vec2 uv, vec3 world, float time) {
    float r = tex_block_rand(world, 10.0);
    float wobble = tex_vnoise(vec2(uv.x * 3.0 + r * 7.0, uv.y * 4.0 + r * 3.0));
    float s = 0.5 + 0.5 * sin(uv.x * 28.0 + wobble * 5.0 + r * 20.0);
    float n = tex_vnoise(vec2(uv.x * 8.0 + r * 31.0, uv.y * 6.0 + r * 17.0));
    return mix(vec3(0.32, 0.22, 0.12), vec3(0.55, 0.40, 0.24), 0.25 + 0.55 * s * 0.5 + 0.25 * n);
}
vec3 tex_log_top(vec2 uv, vec3 world, float time) {
    float r = tex_block_rand(world, 11.0);
    float d = length(uv - vec2(0.5, 0.5)) * 2.0;
    float ring = 0.5 + 0.5 * sin(d * 20.0 + r * 9.0);
    vec3 base = mix(vec3(0.55, 0.42, 0.25), vec3(0.72, 0.58, 0.38), ring);
    float bark = 1.0 - smoothstep(0.86, 0.98, d);
    return mix(base, vec3(0.36, 0.26, 0.14), 1.0 - bark);
}
vec3 tex_leaves(vec2 uv, vec3 world, float time) {
    float r = tex_block_rand(world, 12.0);
    float n = tex_vnoise(uv * 7.0 + vec2(r * 15.0, r * 23.0));
    float n2 = tex_vnoise(uv * 16.0 + vec2(r * 41.0, r * 29.0));
    vec3 base = mix(vec3(0.13, 0.30, 0.10), vec3(0.24, 0.48, 0.16), n);
    float hole = 1.0 - smoothstep(0.15, 0.35, n2);
    return base * (1.0 - 0.55 * hole);
}
vec3 tex_snow_top(vec2 uv, vec3 world, float time) {
    float r = tex_block_rand(world, 13.0);
    float n = tex_vnoise(uv * 10.0 + vec2(r * 21.0, r * 19.0));
    return mix(vec3(0.88, 0.91, 0.96), vec3(0.97, 0.98, 1.0), n);
}
vec3 tex_snow_side(vec2 uv, vec3 world, float time) {
    vec3 dirt = tex_dirt(uv, world, time);
    float r = tex_block_rand(world, 14.0);
    float rag = tex_vnoise(vec2(uv.x * 5.0, r * 23.0));
    float h = 0.22 + 0.12 * rag;
    float rim = smoothstep(1.0 - h - 0.03, 1.0 - h, uv.y);
    vec3 snow = tex_snow_top(uv, world, time);
    return mix(dirt, snow, rim);
}
vec3 tex_flower_red(vec2 uv, vec3 world, float time) {
    vec2 p = uv - vec2(0.5, 0.5);
    float d = length(p);
    float petal = 0.5 + 0.5 * sin(tex_atan2(p.y, p.x) * 5.0 + tex_block_rand(world, 15.0) * 3.14159);
    vec3 c = mix(vec3(0.70, 0.14, 0.12), vec3(0.92, 0.26, 0.22), petal);
    float core = 1.0 - smoothstep(0.05, 0.14, d);
    return mix(c, vec3(0.95, 0.82, 0.30), core);
}
vec3 tex_flower_yellow(vec2 uv, vec3 world, float time) {
    vec2 p = uv - vec2(0.5, 0.5);
    float d = length(p);
    float petal = 0.5 + 0.5 * sin(tex_atan2(p.y, p.x) * 5.0 + tex_block_rand(world, 16.0) * 3.14159);
    vec3 c = mix(vec3(0.85, 0.70, 0.16), vec3(0.96, 0.84, 0.30), petal);
    float core = 1.0 - smoothstep(0.05, 0.14, d);
    return mix(c, vec3(0.55, 0.35, 0.10), core);
}
vec3 tex_planks(vec2 uv, vec3 world, float time) {
    float r = tex_block_rand(world, 17.0);
    float row = floor(uv.y * 4.0);
    float y = fract(uv.y * 4.0);
    float x = fract(uv.x * 2.0 + tex_fmod(row, 2.0) * 0.5 + r);
    float wobble = tex_vnoise(vec2(uv.x * 3.0 + row * 5.0, y * 6.0 + r * 9.0));
    float grain = 0.5 + 0.5 * sin((uv.x * 24.0 + row * 7.0 + r * 10.0) * 1.5708 + wobble * 3.0);
    float streak = tex_vnoise(vec2(uv.x * 5.0 + row * 11.0, y * 20.0 + r * 7.0));
    vec3 base = mix(vec3(0.62, 0.45, 0.26), vec3(0.76, 0.58, 0.36), grain * (0.7 + 0.3 * streak));
    float seam = min(smoothstep(0.0, 0.05, x), smoothstep(0.0, 0.08, y));
    return base * (0.72 + 0.28 * seam);
}
vec3 tex_cobble(vec2 uv, vec3 world, float time) {
    float r = tex_block_rand(world, 18.0);
    vec2 p = uv * 3.0;
    vec2 i = floor(p);
    vec2 f = fract(p);
    float h1 = tex_hash1(dot(i, vec2(127.1, 311.7)) + r * 13.0);
    float h2 = tex_hash1(dot(i + vec2(37.7, 91.3), vec2(127.1, 311.7)) + r * 7.0);
    vec2 o = vec2(0.3 + 0.4 * h1, 0.3 + 0.4 * h2);
    float d = length(f - o);
    float stone = 1.0 - smoothstep(0.18, 0.42, d);
    vec3 shade = mix(vec3(0.42, 0.43, 0.46), vec3(0.62, 0.63, 0.66), h2);
    float n = tex_vnoise(uv * 12.0 + vec2(r * 31.0, r * 19.0));
    vec3 stone_c = shade * (0.85 + 0.30 * n);
    return mix(vec3(0.22, 0.23, 0.25), stone_c, stone);
}
vec3 tex_brick(vec2 uv, vec3 world, float time) {
    float r = tex_block_rand(world, 19.0);
    float row = floor(uv.y * 4.0);
    float by = fract(uv.y * 4.0);
    float bx = fract(uv.x * 2.0 + tex_fmod(row, 2.0) * 0.5);
    float bi = floor(uv.x * 2.0 + tex_fmod(row, 2.0) * 0.5);
    float h = tex_hash1(dot(vec2(bi, row), vec2(127.1, 311.7)) + r * 17.0);
    vec3 brick = mix(vec3(0.55, 0.24, 0.18), vec3(0.68, 0.32, 0.25), h);
    vec3 mortar = vec3(0.75, 0.73, 0.68);
    float mask = max(1.0 - smoothstep(0.0, 0.06, bx), 1.0 - smoothstep(0.0, 0.10, by));
    return mix(brick, mortar, mask);
}
vec3 tex_glass(vec2 uv, vec3 world, float time) {
    float r = tex_block_rand(world, 20.0);
    vec3 base = vec3(0.78, 0.88, 0.94);
    float e = min(min(uv.x, 1.0 - uv.x), min(uv.y, 1.0 - uv.y));
    float frame = 1.0 - smoothstep(0.05, 0.10, e);
    vec3 c = mix(base, vec3(0.92, 0.97, 1.0), frame);
    float g = 1.0 - smoothstep(0.04, 0.16, abs(fract(uv.x * 0.8 + uv.y * 0.6 + r) - 0.5));
    return c + g * 0.15;
}
vec3 tex_tnt_side(vec2 uv, vec3 world, float time) {
    float r = tex_block_rand(world, 21.0);
    float n = tex_vnoise(uv * 8.0 + vec2(r * 11.0, r * 7.0));
    vec3 red = mix(vec3(0.72, 0.20, 0.14), vec3(0.88, 0.30, 0.20), n);
    float band = step(0.42, uv.y) * (1.0 - step(0.62, uv.y));
    float dash = step(0.5, fract(uv.x * 4.0 + r));
    vec3 label = mix(vec3(0.93, 0.90, 0.82), vec3(0.15, 0.13, 0.12), dash);
    return mix(red, label, band);
}
vec3 tex_tnt_top(vec2 uv, vec3 world, float time) {
    float r = tex_block_rand(world, 22.0);
    float n = tex_vnoise(uv * 6.0 + vec2(r * 13.0, r * 9.0));
    vec3 red = mix(vec3(0.75, 0.22, 0.15), vec3(0.88, 0.32, 0.22), n);
    float e = min(min(uv.x, 1.0 - uv.x), min(uv.y, 1.0 - uv.y));
    float rim = 1.0 - smoothstep(0.06, 0.12, e);
    return mix(red, vec3(0.93, 0.90, 0.82), rim);
}
vec3 tex_obsidian(vec2 uv, vec3 world, float time) {
    float r = tex_block_rand(world, 23.0);
    float n = tex_vnoise(uv * 5.0 + vec2(r * 19.0, r * 17.0));
    float n2 = tex_vnoise(uv * 17.0 + vec2(r * 29.0, r * 23.0));
    vec3 base = mix(vec3(0.07, 0.04, 0.10), vec3(0.16, 0.10, 0.22), n);
    float speck = smoothstep(0.75, 0.95, n2);
    return base + vec3(0.35, 0.25, 0.55) * speck * 0.5;
}
vec3 tex_highlight(vec2 uv, vec3 world, float time) {
    return vec3(0.03, 0.03, 0.03);
}
// The id is a smoothly interpolated varying (see the WGSL note):
// threshold dispatch, not equality.
vec3 sample_tex(float tex, vec2 uv, vec3 world, float time) {
    if (tex < 0.5) { return tex_grass_top(uv, world, time); }
    if (tex < 1.5) { return tex_grass_side(uv, world, time); }
    if (tex < 2.5) { return tex_dirt(uv, world, time); }
    if (tex < 3.5) { return tex_stone(uv, world, time); }
    if (tex < 4.5) { return tex_sand(uv, world, time); }
    if (tex < 5.5) { return tex_water(uv, world, time); }
    if (tex < 6.5) { return tex_log_side(uv, world, time); }
    if (tex < 7.5) { return tex_log_top(uv, world, time); }
    if (tex < 8.5) { return tex_leaves(uv, world, time); }
    if (tex < 9.5) { return tex_snow_top(uv, world, time); }
    if (tex < 10.5) { return tex_snow_side(uv, world, time); }
    if (tex < 11.5) { return tex_flower_red(uv, world, time); }
    if (tex < 12.5) { return tex_flower_yellow(uv, world, time); }
    if (tex < 13.5) { return tex_planks(uv, world, time); }
    if (tex < 14.5) { return tex_cobble(uv, world, time); }
    if (tex < 15.5) { return tex_brick(uv, world, time); }
    if (tex < 16.5) { return tex_glass(uv, world, time); }
    if (tex < 17.5) { return tex_tnt_side(uv, world, time); }
    if (tex < 18.5) { return tex_tnt_top(uv, world, time); }
    if (tex < 19.5) { return tex_obsidian(uv, world, time); }
    if (tex < 20.5) { return tex_highlight(uv, world, time); }
    return vec3(1.0, 0.0, 1.0);
}
float tex_trans_alpha(float tex) {
    if (tex > 15.5 && tex < 16.5) { return 0.30; }
    return 0.62;
}
"#;

const FS_HEAD: &str = r#"#version 300 es
precision highp float;
in float v_light;
in vec2 v_uv;
in float v_tex;
in vec3 v_world;
uniform vec3 u_cam;
uniform float u_fog_start;
uniform float u_fog_end;
uniform float u_time;
uniform vec3 u_sky;
out vec4 o;
"#;

const FS_MAIN: &str = r#"
void main() {
    vec3 c = clamp(sample_tex(v_tex, v_uv, v_world, u_time) * v_light, 0.0, 1.0);
    float d = distance(u_cam, v_world);
    float t = clamp((d - u_fog_start) / (u_fog_end - u_fog_start), 0.0, 1.0);
    o = vec4(mix(c, u_sky, t), 1.0);
}
"#;

const FS_W_MAIN: &str = r#"
void main() {
    vec3 c = clamp(sample_tex(v_tex, v_uv, v_world, u_time) * v_light, 0.0, 1.0);
    float d = distance(u_cam, v_world);
    float t = clamp((d - u_fog_start) / (u_fog_end - u_fog_start), 0.0, 1.0);
    o = vec4(mix(c, u_sky, t), tex_trans_alpha(v_tex));
}
"#;

/// Assemble a fragment shader: common header + the texture library + the
/// pass's `main` (`FS_MAIN` for opaque, `FS_W_MAIN` for translucent).
fn fs_source(main: &str) -> String {
    format!("{FS_HEAD}{TEX_LIB}{main}")
}

const W: u32 = 256;
const H: u32 = 144;

/// f32 slice as raw bytes (safe: f32 is a plain 4-byte type).
fn f32_slice(v: &[f32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 4) }
}

/// u32 slice as raw bytes (safe: u32 is a plain 4-byte type).
fn u32_slice(v: &[u32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 4) }
}

type Gl = web_sys::WebGl2RenderingContext;

/// Terrain vertex stride in bytes: [pos(3f), light(1f), uv(2f), tex(1f)]
/// — must match `qwencraft_world::mesh::VERT_STRIDE` (7 f32 = 28 bytes).
const VSTRIDE: i32 = 28;

/// Uniform locations for one program (opaque and water share names).
struct Uni {
    u_vp: Option<web_sys::WebGlUniformLocation>,
    u_cam: Option<web_sys::WebGlUniformLocation>,
    u_fog_start: Option<web_sys::WebGlUniformLocation>,
    u_fog_end: Option<web_sys::WebGlUniformLocation>,
    u_time: Option<web_sys::WebGlUniformLocation>,
    u_sky: Option<web_sys::WebGlUniformLocation>,
}

fn locations(gl: &Gl, program: &web_sys::WebGlProgram) -> Uni {
    Uni {
        u_vp: gl.get_uniform_location(program, "u_vp"),
        u_cam: gl.get_uniform_location(program, "u_cam"),
        u_fog_start: gl.get_uniform_location(program, "u_fog_start"),
        u_fog_end: gl.get_uniform_location(program, "u_fog_end"),
        u_time: gl.get_uniform_location(program, "u_time"),
        u_sky: gl.get_uniform_location(program, "u_sky"),
    }
}

pub struct GlVerifier {
    gl: Gl,
    program: web_sys::WebGlProgram,
    /// Translucent program (src-alpha blend, no depth writes).
    program_w: web_sys::WebGlProgram,
    uni: Uni,
    uni_w: Uni,
    vbo: web_sys::WebGlBuffer,
    ibo: web_sys::WebGlBuffer,
    w_vbo: web_sys::WebGlBuffer,
    w_ibo: web_sys::WebGlBuffer,
}

impl GlVerifier {
    pub fn new(doc: &web_sys::Document) -> Option<GlVerifier> {
        let canvas = doc.create_element("canvas").ok()?;
        let canvas: web_sys::HtmlCanvasElement = canvas.dyn_into().ok()?;
        canvas.set_width(W);
        canvas.set_height(H);
        let gl: Gl = canvas
            .get_context("webgl2")
            .ok()??
            .dyn_into()
            .ok()?;

        let compile = |ty: u32, src: &str| -> Option<web_sys::WebGlShader> {
            let s = gl.create_shader(ty)?;
            gl.shader_source(&s, src);
            gl.compile_shader(&s);
            if !gl.get_shader_parameter(&s, Gl::COMPILE_STATUS) {
                let log = gl.get_shader_info_log(&s).unwrap_or_default();
                web_sys::console::error_1(&wasm_bindgen::JsValue::from_str(&format!(
                    "GLSL compile error: {log}"
                )));
                return None;
            }
            Some(s)
        };
        let fs_src = fs_source(FS_MAIN);
        let fs_w_src = fs_source(FS_W_MAIN);
        let vs = compile(Gl::VERTEX_SHADER, VS)?;
        let fs = compile(Gl::FRAGMENT_SHADER, &fs_src)?;
        let fs_w = compile(Gl::FRAGMENT_SHADER, &fs_w_src)?;
        let program = gl.create_program()?;
        gl.attach_shader(&program, &vs);
        gl.attach_shader(&program, &fs);
        gl.link_program(&program);
        if !gl.get_program_parameter(&program, Gl::LINK_STATUS) {
            let log = gl.get_program_info_log(&program).unwrap_or_default();
            web_sys::console::error_1(&wasm_bindgen::JsValue::from_str(&format!(
                "GLSL link error: {log}"
            )));
            return None;
        }
        // Water program reuses the vertex shader object (GL allows sharing
        // compiled shaders between programs).
        let program_w = gl.create_program()?;
        gl.attach_shader(&program_w, &vs);
        gl.attach_shader(&program_w, &fs_w);
        gl.link_program(&program_w);
        if !gl.get_program_parameter(&program_w, Gl::LINK_STATUS) {
            let log = gl.get_program_info_log(&program_w).unwrap_or_default();
            web_sys::console::error_1(&wasm_bindgen::JsValue::from_str(&format!(
                "GLSL link error (water): {log}"
            )));
            return None;
        }

        Some(GlVerifier {
            uni: locations(&gl, &program),
            uni_w: locations(&gl, &program_w),
            vbo: gl.create_buffer()?,
            ibo: gl.create_buffer()?,
            w_vbo: gl.create_buffer()?,
            w_ibo: gl.create_buffer()?,
            program,
            program_w,
            gl,
        })
    }

    /// Re-render the scene from `regions` (26^3 payloads) with the
    /// first-person camera and return a 4x3 grid of region colour averages
    /// as `r,g,b; ` (12 entries, left-to-right, top of screen first).
    /// `highlight` is the block under the crosshair (wireframe box, as in
    /// the WebGPU path).
    ///
    /// WebGL's framebuffer origin is bottom-left, so display row `i` is
    /// sampled from GL rows `(2 - i) * 48 .. (2 - i) * 48 + 48`.
    pub fn readback(
        &self,
        regions: &HashMap<ChunkPos, Vec<u8>>,
        cam: [f32; 3],
        yaw: f32,
        pitch: f32,
        highlight: Option<(i32, i32, i32)>,
    ) -> Option<String> {
        // Rebuild the combined mesh on the CPU from the streamed payloads
        // (opaque + water, mirroring the WebGPU pool layout: water is
        // appended after the opaque part per chunk).
        let mut verts: Vec<f32> = Vec::new();
        let mut idxs: Vec<u32> = Vec::new();
        let mut w_verts: Vec<f32> = Vec::new();
        let mut w_idxs: Vec<u32> = Vec::new();
        let mut base: u32 = 0;
        let mut base_w: u32 = 0;
        for (pos, data) in regions {
            if data.len() != REGION_BLOCKS {
                continue;
            }
            let m = build_chunk_mesh((pos.x * 16, pos.y * 16, pos.z * 16), data);
            if m.is_empty() {
                continue;
            }
            for i in m.indices {
                idxs.push(i + base);
            }
            // Water indices are chunk-local; offset by the combined water
            // vertex base accumulated from earlier chunks.
            for i in m.water_indices {
                w_idxs.push(i + base_w);
            }
            base += m.vertices.len() as u32 / 7;
            base_w += m.water_vertices.len() as u32 / 7;
            verts.extend(m.vertices);
            w_verts.extend(m.water_vertices);
        }
        if verts.is_empty() && w_verts.is_empty() {
            return Some("(no geometry)".to_string());
        }

        let gl = &self.gl;
        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.vbo));
        gl.buffer_data_with_u8_array(Gl::ARRAY_BUFFER, f32_slice(&verts), Gl::STATIC_DRAW);
        gl.bind_buffer(Gl::ELEMENT_ARRAY_BUFFER, Some(&self.ibo));
        gl.buffer_data_with_u8_array(Gl::ELEMENT_ARRAY_BUFFER, u32_slice(&idxs), Gl::STATIC_DRAW);
        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.w_vbo));
        gl.buffer_data_with_u8_array(Gl::ARRAY_BUFFER, f32_slice(&w_verts), Gl::STATIC_DRAW);
        gl.bind_buffer(Gl::ELEMENT_ARRAY_BUFFER, Some(&self.w_ibo));
        gl.buffer_data_with_u8_array(Gl::ELEMENT_ARRAY_BUFFER, u32_slice(&w_idxs), Gl::STATIC_DRAW);

        let vp = view_projection(cam, yaw, pitch, W as f32 / H as f32, 1.15, 0.1, 300.0);
        let set_unis = |u: &Uni| {
            gl.uniform_matrix4fv_with_f32_array(u.u_vp.as_ref(), false, &vp);
            gl.uniform3f(u.u_cam.as_ref(), cam[0], cam[1], cam[2]);
            gl.uniform1f(u.u_fog_start.as_ref(), FOG_START);
            gl.uniform1f(u.u_fog_end.as_ref(), FOG_END);
            // Still water (deterministic pixels; see the module note).
            gl.uniform1f(u.u_time.as_ref(), 0.0);
            gl.uniform3f(u.u_sky.as_ref(), SKY[0], SKY[1], SKY[2]);
        };
        // The four terrain attributes (pos/light/uv/tex, 28-byte stride).
        let set_attribs = |vbo: &web_sys::WebGlBuffer| {
            gl.bind_buffer(Gl::ARRAY_BUFFER, Some(vbo));
            gl.vertex_attrib_pointer_with_i32(0, 3, Gl::FLOAT, false, VSTRIDE, 0);
            gl.vertex_attrib_pointer_with_i32(1, 1, Gl::FLOAT, false, VSTRIDE, 12);
            gl.vertex_attrib_pointer_with_i32(2, 2, Gl::FLOAT, false, VSTRIDE, 16);
            gl.vertex_attrib_pointer_with_i32(3, 1, Gl::FLOAT, false, VSTRIDE, 24);
            for l in 0..4 {
                gl.enable_vertex_attrib_array(l);
            }
        };

        gl.enable(Gl::DEPTH_TEST);
        gl.depth_func(Gl::LESS);
        gl.viewport(0, 0, W as i32, H as i32);
        gl.clear_color(SKY[0], SKY[1], SKY[2], 1.0);
        gl.clear(Gl::COLOR_BUFFER_BIT | Gl::DEPTH_BUFFER_BIT);

        // Opaque pass (terrain + flowers + glass is translucent, not here).
        gl.use_program(Some(&self.program));
        set_unis(&self.uni);
        set_attribs(&self.vbo);
        gl.bind_buffer(Gl::ELEMENT_ARRAY_BUFFER, Some(&self.ibo));
        gl.draw_elements_with_i32(Gl::TRIANGLES, idxs.len() as i32, Gl::UNSIGNED_INT, 0);

        // Water/glass pass (src-alpha blend, no depth writes) — mirrors the
        // WebGPU translucent pipeline.
        if !w_idxs.is_empty() {
            gl.use_program(Some(&self.program_w));
            set_unis(&self.uni_w);
            gl.enable(Gl::BLEND);
            gl.blend_func(Gl::SRC_ALPHA, Gl::ONE_MINUS_SRC_ALPHA);
            gl.depth_mask(false);
            set_attribs(&self.w_vbo);
            gl.bind_buffer(Gl::ELEMENT_ARRAY_BUFFER, Some(&self.w_ibo));
            gl.draw_elements_with_i32(Gl::TRIANGLES, w_idxs.len() as i32, Gl::UNSIGNED_INT, 0);
            gl.depth_mask(true);
            gl.disable(Gl::BLEND);
        }

        // Block highlight (wireframe) — mirrors the WebGPU line pass. The
        // highlight carries TEX_HIGHLIGHT through the normal textured path
        // (a constant dark colour), so it draws with the opaque program.
        // The water vbo is free now, so the 24 line vertices go in there.
        if let Some(t) = highlight {
            let v = highlight_vertices(t);
            gl.use_program(Some(&self.program));
            set_unis(&self.uni);
            gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.w_vbo));
            gl.buffer_data_with_u8_array(Gl::ARRAY_BUFFER, f32_slice(&v), Gl::STATIC_DRAW);
            set_attribs(&self.w_vbo);
            gl.draw_arrays(Gl::LINES, 0, 24);
        }

        let mut px = vec![0u8; (W * H * 4) as usize];
        gl.read_pixels_with_opt_u8_array(
            0,
            0,
            W as i32,
            H as i32,
            Gl::RGBA,
            Gl::UNSIGNED_BYTE,
            Some(&mut px),
        )
        .ok()?;

        // 4x3 grid of 64x48-pixel regions, top of screen first (GL rows
        // run bottom-up).
        let mut msg = String::new();
        for screen_row in 0..3 {
            for gx in 0..4 {
                let gl_y = (2 - screen_row) * 48;
                let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
                for y in gl_y..(gl_y + 48) {
                    for x in (gx * 64)..(gx * 64 + 64) {
                        let i = ((y * W + x) * 4) as usize;
                        r += px[i] as u32;
                        g += px[i + 1] as u32;
                        b += px[i + 2] as u32;
                        n += 1;
                    }
                }
                msg.push_str(&format!("{},{},{}; ", r / n, g / n, b / n));
            }
        }
        Some(msg)
    }

    /// Read the entire framebuffer (RGBA8, GL row order: row 0 = bottom of
    /// screen). Must be called while the last rendered scene is still in the
    /// default framebuffer.
    pub fn framebuffer(&self) -> Option<Vec<u8>> {
        let gl = &self.gl;
        let mut px = vec![0u8; (W * H * 4) as usize];
        gl.read_pixels_with_opt_u8_array(0, 0, W as i32, H as i32, Gl::RGBA, Gl::UNSIGNED_BYTE, Some(&mut px))
            .ok()?;
        Some(px)
    }
}

/// Minimal base64 encoder (standard alphabet, with padding) used to stream
/// the verify screenshot through the console log.
pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}
