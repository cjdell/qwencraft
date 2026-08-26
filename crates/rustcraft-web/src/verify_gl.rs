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
//! literal translation of the WGSL pipeline, then read the pixels back.
//! This verifies the scene data, camera math, fog and colours end to end.
//! The WGSL itself is separately verified by the browser compiling the real
//! WebGPU pipeline at startup (a shader error fails renderer init).

use std::collections::HashMap;

use wasm_bindgen::JsCast;

use rustcraft_world::camera::{view_projection, FOG_END, FOG_START, SKY};
use rustcraft_world::mesh::build_chunk_mesh;
use rustcraft_world::{ChunkPos, REGION_BLOCKS};

const VS: &str = r#"#version 300 es
layout(location=0) in vec3 a_pos;
layout(location=1) in vec3 a_color;
uniform mat4 u_vp;
out vec3 v_color;
out vec3 v_world;
void main() {
    gl_Position = u_vp * vec4(a_pos, 1.0);
    v_color = a_color;
    v_world = a_pos;
}
"#;

const FS: &str = r#"#version 300 es
precision highp float;
in vec3 v_color;
in vec3 v_world;
uniform vec3 u_cam;
uniform float u_fog_start;
uniform float u_fog_end;
uniform vec3 u_sky;
out vec4 o;
void main() {
    float d = distance(u_cam, v_world);
    float t = clamp((d - u_fog_start) / (u_fog_end - u_fog_start), 0.0, 1.0);
    o = vec4(mix(v_color, u_sky, t), 1.0);
}
"#;

/// Water variant: constant translucency (mirrors `fs_water` in the WGSL).
const FS_W: &str = r#"#version 300 es
precision highp float;
in vec3 v_color;
in vec3 v_world;
uniform vec3 u_cam;
uniform float u_fog_start;
uniform float u_fog_end;
uniform vec3 u_sky;
out vec4 o;
void main() {
    float d = distance(u_cam, v_world);
    float t = clamp((d - u_fog_start) / (u_fog_end - u_fog_start), 0.0, 1.0);
    o = vec4(mix(v_color, u_sky, t), 0.62);
}
"#;

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

/// Uniform locations for one program (opaque and water share names).
struct Uni {
    u_vp: Option<web_sys::WebGlUniformLocation>,
    u_cam: Option<web_sys::WebGlUniformLocation>,
    u_fog_start: Option<web_sys::WebGlUniformLocation>,
    u_fog_end: Option<web_sys::WebGlUniformLocation>,
    u_sky: Option<web_sys::WebGlUniformLocation>,
}

fn locations(gl: &Gl, program: &web_sys::WebGlProgram) -> Uni {
    Uni {
        u_vp: gl.get_uniform_location(program, "u_vp"),
        u_cam: gl.get_uniform_location(program, "u_cam"),
        u_fog_start: gl.get_uniform_location(program, "u_fog_start"),
        u_fog_end: gl.get_uniform_location(program, "u_fog_end"),
        u_sky: gl.get_uniform_location(program, "u_sky"),
    }
}

pub struct GlVerifier {
    gl: Gl,
    program: web_sys::WebGlProgram,
    /// Translucent water program (src-alpha blend, no depth writes).
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
        let vs = compile(Gl::VERTEX_SHADER, VS)?;
        let fs = compile(Gl::FRAGMENT_SHADER, FS)?;
        let fs_w = compile(Gl::FRAGMENT_SHADER, FS_W)?;
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
    ///
    /// WebGL's framebuffer origin is bottom-left, so display row `i` is
    /// sampled from GL rows `(2 - i) * 48 .. (2 - i) * 48 + 48`.
    pub fn readback(
        &self,
        regions: &HashMap<ChunkPos, Vec<u8>>,
        cam: [f32; 3],
        yaw: f32,
        pitch: f32,
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
            base += m.vertices.len() as u32 / 6;
            base_w += m.water_vertices.len() as u32 / 6;
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
            gl.uniform3f(u.u_sky.as_ref(), SKY[0], SKY[1], SKY[2]);
        };

        gl.enable(Gl::DEPTH_TEST);
        gl.depth_func(Gl::LESS);
        gl.viewport(0, 0, W as i32, H as i32);
        gl.clear_color(SKY[0], SKY[1], SKY[2], 1.0);
        gl.clear(Gl::COLOR_BUFFER_BIT | Gl::DEPTH_BUFFER_BIT);

        // Opaque pass (terrain + flowers).
        gl.use_program(Some(&self.program));
        set_unis(&self.uni);
        gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.vbo));
        gl.vertex_attrib_pointer_with_i32(0, 3, Gl::FLOAT, false, 24, 0);
        gl.vertex_attrib_pointer_with_i32(1, 3, Gl::FLOAT, false, 24, 12);
        gl.enable_vertex_attrib_array(0);
        gl.enable_vertex_attrib_array(1);
        gl.bind_buffer(Gl::ELEMENT_ARRAY_BUFFER, Some(&self.ibo));
        gl.draw_elements_with_i32(Gl::TRIANGLES, idxs.len() as i32, Gl::UNSIGNED_INT, 0);

        // Water pass (src-alpha blend, no depth writes) — mirrors the
        // WebGPU water pipeline.
        if !w_idxs.is_empty() {
            gl.use_program(Some(&self.program_w));
            set_unis(&self.uni_w);
            gl.enable(Gl::BLEND);
            gl.blend_func(Gl::SRC_ALPHA, Gl::ONE_MINUS_SRC_ALPHA);
            gl.depth_mask(false);
            gl.bind_buffer(Gl::ARRAY_BUFFER, Some(&self.w_vbo));
            gl.vertex_attrib_pointer_with_i32(0, 3, Gl::FLOAT, false, 24, 0);
            gl.vertex_attrib_pointer_with_i32(1, 3, Gl::FLOAT, false, 24, 12);
            gl.bind_buffer(Gl::ELEMENT_ARRAY_BUFFER, Some(&self.w_ibo));
            gl.draw_elements_with_i32(Gl::TRIANGLES, w_idxs.len() as i32, Gl::UNSIGNED_INT, 0);
            gl.depth_mask(true);
            gl.disable(Gl::BLEND);
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
