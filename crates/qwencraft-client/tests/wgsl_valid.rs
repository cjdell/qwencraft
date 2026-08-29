//! Host-side validation of the renderer's WGSL module.
//!
//! The crate itself only compiles for wasm32, but this integration test
//! runs on the host (it links against nothing — it just re-reads the
//! `.wgsl` sources the renderer embeds) and type-checks the concatenated
//! module with naga, the same shader front-end family Dawn (browser
//! WebGPU) uses. That catches WGSL/GLSL drift the moment `cargo test`
//! runs — e.g. GLSL-only builtins (`mod()`), missing helpers, and
//! abstract-float literal mixing
//! (`clamp(vec3<f32>, 0.0, 1.0)` → needs `0.0f, 1.0f`) — instead of
//! three headless-browser round trips.
//!
//! Known gap: naga-rs is a little more lenient than Dawn in exactly one
//! documented case (it accepts a two-argument `atan(y, x)`, which Dawn
//! rejects — that's why the textures use the shared `tex_atan2` helper
//! anyway). The browser tests (verify.sh / remote_test.sh) remain the
//! final authority on what Dawn accepts.

const SHADER: &str = include_str!("../src/shader.wgsl");
const TEXTURES: &str = include_str!("../src/textures.wgsl");

#[test]
fn shader_module_is_valid_wgsl() {
    let source = format!("{SHADER}{TEXTURES}");
    let module = naga::front::wgsl::parse_str(&source).expect("WGSL must parse");
    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    );
    validator
        .validate(&module)
        .expect("WGSL must pass naga validation (same front-end family as Dawn)");
}

#[test]
fn every_texture_function_is_defined() {
    // The mesh (qwencraft_world::mesh) writes one texture id per face via
    // the `TEX_*` constants in qwencraft_world::block; `sample_tex` in
    // textures.wgsl must handle every id with a threshold branch, and each
    // branch must call a defined function. Keep this list in lockstep with
    // the TEX_* constants (block.rs) and the dispatch chain.
    let source = format!("{SHADER}{TEXTURES}");
    let mut defined = std::collections::HashSet::new();
    for line in source.lines() {
        if let Some(rest) = line.strip_prefix("fn ") {
            if let Some(name) = rest.split(['<', '(']).next() {
                defined.insert(name.to_string());
            }
        }
    }
    for name in [
        "tex_grass_top",
        "tex_grass_side",
        "tex_dirt",
        "tex_stone",
        "tex_sand",
        "tex_water",
        "tex_log_side",
        "tex_log_top",
        "tex_leaves",
        "tex_snow_top",
        "tex_snow_side",
        "tex_flower_red",
        "tex_flower_yellow",
        "tex_planks",
        "tex_cobble",
        "tex_brick",
        "tex_glass",
        "tex_tnt_side",
        "tex_tnt_top",
        "tex_obsidian",
        "tex_highlight",
        "sample_tex",
        "tex_trans_alpha",
    ] {
        assert!(defined.contains(name), "texture function `{name}` is missing from the WGSL");
    }
}
