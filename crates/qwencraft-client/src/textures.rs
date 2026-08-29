//! Procedural block textures — one WGSL function per texture id.
//!
//! This is the "appearance" half of the block registry: `block.rs`
//! (qwencraft_world) ties each block to its face texture ids + physics +
//! CPU colours, and this file defines what those textures actually look
//! like. Adding a block = add its function(s) here (next to the others,
//! with the matching `TEX_*` id from `block.rs`) and mirror it in the
//! GLSL shadow renderer (`qwencraft-web/src/verify_gl.rs`).
//!
//! Contract of every `tex_*` function (it maps ONE block face):
//! - `uv` runs 0..1 across the face. `u` is horizontal, `v` vertical with
//!   1 at the TOP edge (top/bottom faces use u=x, v=z).
//! - **Vary in BOTH directions.** A face texture that is a function of
//!   `uv.x` alone (or holds a noise argument constant) is a 1D texture in
//!   disguise: it renders as flat stripes, and — because top/bottom faces
//!   map u=x, v=z while sides map u=z, v=y — the stripes run in different
//!   world directions on different faces. If a feature is genuinely
//!   1D-ish (a ragged rim line, a label band, wood grain ALONG a board),
//!   keep the 1D part for the edge/phase and make the *fill* 2D — see
//!   `tex_grass_side` / `tex_log_side` / `tex_planks` in textures.wgsl.
//! - `world` is the fragment's world position. A face lies on an integer
//!   plane, so `floor(world)` is constant across the whole face (up to a
//!   measure-zero boundary pixel) — that is how per-block variation works
//!   without any extra vertex data.
//! - `time` is wall-clock seconds (only the water texture uses it).
//! - Return value is linear RGB in 0..1 (the shader multiplies it by the
//!   baked light scalar and applies fog).
//!
//! Keep the functions in the small portable subset shared with GLSL ES
//! 3.00 (fract/mix/smoothstep/step/sin/cos/atan/length/dot/floor/abs —
//! no language-specific builtins) so the GLSL mirror in `verify_gl.rs`
//! stays a mechanical translation. Traps already hit: WGSL has NO
//! two-argument `atan(y, x)` (Dawn rejects it even though naga accepts
//! it), and float remainder is spelled `%` in WGSL but `mod()` in GLSL
//! (GLSL ES 3.00's `%` is integer-only) — so the 2-arg angle math goes
//! through the shared `tex_atan2` helper and float remainder through the
//! shared `tex_fmod` helper (each implemented with its language's native
//! operation). Also keep smoothstep edges ascending (edge0 < edge1):
//! descending edges are undefined in WGSL — write them as
//! `1.0 - smoothstep(e1, e0, x)` (pixel-identical reversed curve).

pub const TEXTURES: &str = include_str!("textures.wgsl");
