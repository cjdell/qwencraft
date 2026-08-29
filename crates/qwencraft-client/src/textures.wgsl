// ---- texture helpers (must match the GLSL mirror) ----------------------

fn tex_hash1(n: f32) -> f32 {
    return fract(sin(n) * 43758.5453123);
}

/// Per-block random in [0,1): constant across a face (see the module note
/// about `floor(world)`), different per block and per salt.
fn tex_block_rand(world: vec3<f32>, salt: f32) -> f32 {
    let c = floor(world);
    return tex_hash1(dot(c, vec3<f32>(127.1, 311.7, 74.7)) + salt * 57.31);
}

/// atan2(y, x). WGSL's `atan` is single-argument (the two-arg form that
/// GLSL has is rejected by Dawn), so the angle math goes through this
/// helper in both languages. The 0/0 guard keeps the flower's centre
/// pixel finite.
fn tex_atan2(y: f32, x: f32) -> f32 {
    if (x == 0.0 && y == 0.0) { return 0.0; }
    let a = atan(y / x);
    if (x < 0.0) {
        return a + select(3.14159265, -3.14159265, y < 0.0);
    }
    return a;
}

/// Float remainder. WGSL spells it `%`; GLSL ES 3.00 spells it `mod()`
/// (its `%` operator is integer-only), so the mirror implements this
/// with its language's native operation.
fn tex_fmod(a: f32, b: f32) -> f32 {
    return a % b;
}

/// 2D value noise (integer lattice, smoothstep interpolation) in [0,1].
fn tex_vnoise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = tex_hash1(dot(i, vec2<f32>(127.1, 311.7)));
    let b = tex_hash1(dot(i + vec2<f32>(1.0, 0.0), vec2<f32>(127.1, 311.7)));
    let c = tex_hash1(dot(i + vec2<f32>(0.0, 1.0), vec2<f32>(127.1, 311.7)));
    let d = tex_hash1(dot(i + vec2<f32>(1.0, 1.0), vec2<f32>(127.1, 311.7)));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

// ---- the textures (ids: block.rs TEX_*) ---------------------------------

// 0: grass top — mottled green with brighter tufts.
fn tex_grass_top(_uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let r = tex_block_rand(world, 1.0);
    let n = tex_vnoise(world.xz * 6.0 + r * 17.0);
    let fine = tex_vnoise(world.xz * 22.0 + r * 31.0);
    let base = mix(vec3<f32>(0.27, 0.50, 0.20), vec3<f32>(0.40, 0.66, 0.27), n);
    return mix(base, vec3<f32>(0.47, 0.74, 0.34), smoothstep(0.72, 0.95, fine));
}

// 2: dirt — brown blotches with fine speckle.
fn tex_dirt(uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let n = tex_vnoise(uv * 6.0 + vec2<f32>(tex_block_rand(world, 3.0) * 11.0, tex_block_rand(world, 4.0) * 7.0));
    let n2 = tex_vnoise(uv * 18.0 + vec2<f32>(tex_block_rand(world, 5.0) * 23.0, tex_block_rand(world, 6.0) * 13.0));
    let base = mix(vec3<f32>(0.47, 0.33, 0.21), vec3<f32>(0.60, 0.44, 0.30), n);
    return base * (0.90 + 0.20 * n2);
}

// 1: grass side — dirt with a ragged green overhang along the top edge.
fn tex_grass_side(uv: vec2<f32>, world: vec3<f32>, time: f32) -> vec3<f32> {
    let dirt = tex_dirt(uv, world, time);
    let r = tex_block_rand(world, 7.0);
    let rag = tex_vnoise(vec2<f32>(uv.x * 5.0, r * 13.0));
    let h = 0.16 + 0.10 * rag;
    let rim = smoothstep(1.0 - h - 0.03, 1.0 - h, uv.y);
    let grass = vec3<f32>(0.30, 0.56, 0.22) * (0.85 + 0.30 * tex_vnoise(vec2<f32>(uv.x * 9.0, r * 3.0)));
    return mix(dirt, grass, rim);
}

// 3: stone — grey mottling with a few dark cracks.
fn tex_stone(uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let r = tex_block_rand(world, 8.0);
    let n = tex_vnoise(uv * 4.0 + vec2<f32>(r * 13.0, r * 7.0));
    let n2 = tex_vnoise(uv * 11.0 + vec2<f32>(r * 29.0, r * 17.0));
    let base = mix(vec3<f32>(0.45, 0.46, 0.48), vec3<f32>(0.58, 0.59, 0.62), n);
    let crack = 1.0 - smoothstep(0.02, 0.10, n2);
    return base * (1.0 - 0.35 * crack);
}

// 4: sand — pale, fine speckle.
fn tex_sand(uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let r = tex_block_rand(world, 9.0);
    let n = tex_vnoise(uv * 14.0 + vec2<f32>(r * 19.0, r * 11.0));
    let base = mix(vec3<f32>(0.83, 0.77, 0.55), vec3<f32>(0.91, 0.86, 0.64), n);
    let speck = tex_vnoise(uv * 40.0 + vec2<f32>(r * 7.0, r * 5.0));
    return base * (0.94 + 0.12 * speck);
}

// 5: water — drifting ripples (the only texture that uses time).
fn tex_water(_uv: vec2<f32>, world: vec3<f32>, time: f32) -> vec3<f32> {
    let w1 = sin(world.x * 4.7 + time * 1.9 + sin(world.z * 3.3 + time * 1.3) * 0.8);
    let w2 = sin(world.z * 5.3 - time * 1.7 + world.x * 2.1);
    let ripple = 0.5 + 0.5 * (w1 * 0.6 + w2 * 0.4);
    let base = vec3<f32>(0.20, 0.40, 0.80);
    let light = vec3<f32>(0.32, 0.55, 0.92);
    return mix(base, light, ripple * 0.7);
}

// 6: log side — vertical bark stripes.
fn tex_log_side(uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let r = tex_block_rand(world, 10.0);
    let s = 0.5 + 0.5 * sin(uv.x * 28.0 + r * 20.0);
    let n = tex_vnoise(vec2<f32>(uv.x * 8.0 + r * 31.0, r * 17.0));
    return mix(vec3<f32>(0.32, 0.22, 0.12), vec3<f32>(0.55, 0.40, 0.24), 0.25 + 0.55 * s * 0.5 + 0.25 * n);
}

// 7: log top — growth rings with a bark edge.
fn tex_log_top(uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let r = tex_block_rand(world, 11.0);
    let d = length(uv - vec2<f32>(0.5, 0.5)) * 2.0;
    let ring = 0.5 + 0.5 * sin(d * 20.0 + r * 9.0);
    let base = mix(vec3<f32>(0.55, 0.42, 0.25), vec3<f32>(0.72, 0.58, 0.38), ring);
    let bark = 1.0 - smoothstep(0.86, 0.98, d);
    return mix(base, vec3<f32>(0.36, 0.26, 0.14), 1.0 - bark);
}

// 8: leaves — dark green with darker holes.
fn tex_leaves(uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let r = tex_block_rand(world, 12.0);
    let n = tex_vnoise(uv * 7.0 + vec2<f32>(r * 15.0, r * 23.0));
    let n2 = tex_vnoise(uv * 16.0 + vec2<f32>(r * 41.0, r * 29.0));
    let base = mix(vec3<f32>(0.13, 0.30, 0.10), vec3<f32>(0.24, 0.48, 0.16), n);
    let hole = 1.0 - smoothstep(0.15, 0.35, n2);
    return base * (1.0 - 0.55 * hole);
}

// 9: snow top — white with soft blue-grey noise.
fn tex_snow_top(uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let r = tex_block_rand(world, 13.0);
    let n = tex_vnoise(uv * 10.0 + vec2<f32>(r * 21.0, r * 19.0));
    return mix(vec3<f32>(0.88, 0.91, 0.96), vec3<f32>(0.97, 0.98, 1.0), n);
}

// 10: snow side — dirt with a snow band along the top edge.
fn tex_snow_side(uv: vec2<f32>, world: vec3<f32>, time: f32) -> vec3<f32> {
    let dirt = tex_dirt(uv, world, time);
    let r = tex_block_rand(world, 14.0);
    let rag = tex_vnoise(vec2<f32>(uv.x * 5.0, r * 23.0));
    let h = 0.22 + 0.12 * rag;
    let rim = smoothstep(1.0 - h - 0.03, 1.0 - h, uv.y);
    let snow = tex_snow_top(uv, world, time);
    return mix(dirt, snow, rim);
}

// 11: red flower — five petals around a yellow core.
fn tex_flower_red(uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let p = uv - vec2<f32>(0.5, 0.5);
    let d = length(p);
    let petal = 0.5 + 0.5 * sin(tex_atan2(p.y, p.x) * 5.0 + tex_block_rand(world, 15.0) * 3.14159);
    let c = mix(vec3<f32>(0.70, 0.14, 0.12), vec3<f32>(0.92, 0.26, 0.22), petal);
    let core = 1.0 - smoothstep(0.05, 0.14, d);
    return mix(c, vec3<f32>(0.95, 0.82, 0.30), core);
}

// 12: yellow flower — same shape, warm petals.
fn tex_flower_yellow(uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let p = uv - vec2<f32>(0.5, 0.5);
    let d = length(p);
    let petal = 0.5 + 0.5 * sin(tex_atan2(p.y, p.x) * 5.0 + tex_block_rand(world, 16.0) * 3.14159);
    let c = mix(vec3<f32>(0.85, 0.70, 0.16), vec3<f32>(0.96, 0.84, 0.30), petal);
    let core = 1.0 - smoothstep(0.05, 0.14, d);
    return mix(c, vec3<f32>(0.55, 0.35, 0.10), core);
}

// 13: planks — four rows of staggered boards with grain and seams.
fn tex_planks(uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let r = tex_block_rand(world, 17.0);
    let row = floor(uv.y * 4.0);
    let y = fract(uv.y * 4.0);
    let x = fract(uv.x * 2.0 + tex_fmod(row, 2.0) * 0.5 + r);
    let grain = 0.5 + 0.5 * sin((uv.x * 24.0 + row * 7.0 + r * 10.0) * 1.5708);
    let base = mix(vec3<f32>(0.62, 0.45, 0.26), vec3<f32>(0.76, 0.58, 0.36), grain);
    let seam = min(smoothstep(0.0, 0.05, x), smoothstep(0.0, 0.08, y));
    return base * (0.72 + 0.28 * seam);
}

// 14: cobblestone — rounded stones with dark mortar.
fn tex_cobble(uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let r = tex_block_rand(world, 18.0);
    let p = uv * 3.0;
    let i = floor(p);
    let f = fract(p);
    let h1 = tex_hash1(dot(i, vec2<f32>(127.1, 311.7)) + r * 13.0);
    let h2 = tex_hash1(dot(i + vec2<f32>(37.7, 91.3), vec2<f32>(127.1, 311.7)) + r * 7.0);
    let o = vec2<f32>(0.3 + 0.4 * h1, 0.3 + 0.4 * h2);
    let d = length(f - o);
    let stone = 1.0 - smoothstep(0.18, 0.42, d);
    let shade = mix(vec3<f32>(0.42, 0.43, 0.46), vec3<f32>(0.62, 0.63, 0.66), h2);
    let n = tex_vnoise(uv * 12.0 + vec2<f32>(r * 31.0, r * 19.0));
    let stone_c = shade * (0.85 + 0.30 * n);
    return mix(vec3<f32>(0.22, 0.23, 0.25), stone_c, stone);
}

// 15: brick — staggered brick rows with pale mortar.
fn tex_brick(uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let r = tex_block_rand(world, 19.0);
    let row = floor(uv.y * 4.0);
    let by = fract(uv.y * 4.0);
    let bx = fract(uv.x * 2.0 + tex_fmod(row, 2.0) * 0.5);
    let bi = floor(uv.x * 2.0 + tex_fmod(row, 2.0) * 0.5);
    let h = tex_hash1(dot(vec2<f32>(bi, row), vec2<f32>(127.1, 311.7)) + r * 17.0);
    let brick = mix(vec3<f32>(0.55, 0.24, 0.18), vec3<f32>(0.68, 0.32, 0.25), h);
    let mortar = vec3<f32>(0.75, 0.73, 0.68);
    let mask = max(1.0 - smoothstep(0.0, 0.06, bx), 1.0 - smoothstep(0.0, 0.10, by));
    return mix(brick, mortar, mask);
}

// 16: glass — pale pane, bright frame, soft diagonal glint.
// (Alpha is applied by the translucent pipeline: see `tex_trans_alpha`.)
fn tex_glass(uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let r = tex_block_rand(world, 20.0);
    let base = vec3<f32>(0.78, 0.88, 0.94);
    let e = min(min(uv.x, 1.0 - uv.x), min(uv.y, 1.0 - uv.y));
    let frame = 1.0 - smoothstep(0.05, 0.10, e);
    let c = mix(base, vec3<f32>(0.92, 0.97, 1.0), frame);
    let g = 1.0 - smoothstep(0.04, 0.16, abs(fract(uv.x * 0.8 + uv.y * 0.6 + r) - 0.5));
    return c + g * 0.15;
}

// 17: tnt side — red with the white label band and black dashes.
fn tex_tnt_side(uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let r = tex_block_rand(world, 21.0);
    let n = tex_vnoise(uv * 8.0 + vec2<f32>(r * 11.0, r * 7.0));
    let red = mix(vec3<f32>(0.72, 0.20, 0.14), vec3<f32>(0.88, 0.30, 0.20), n);
    let band = step(0.42, uv.y) * (1.0 - step(0.62, uv.y));
    let dash = step(0.5, fract(uv.x * 4.0 + r));
    let label = mix(vec3<f32>(0.93, 0.90, 0.82), vec3<f32>(0.15, 0.13, 0.12), dash);
    return mix(red, label, band);
}

// 18: tnt top — red with a pale rim.
fn tex_tnt_top(uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let r = tex_block_rand(world, 22.0);
    let n = tex_vnoise(uv * 6.0 + vec2<f32>(r * 13.0, r * 9.0));
    let red = mix(vec3<f32>(0.75, 0.22, 0.15), vec3<f32>(0.88, 0.32, 0.22), n);
    let e = min(min(uv.x, 1.0 - uv.x), min(uv.y, 1.0 - uv.y));
    let rim = 1.0 - smoothstep(0.06, 0.12, e);
    return mix(red, vec3<f32>(0.93, 0.90, 0.82), rim);
}

// 19: obsidian — near-black purple with glowing specks.
fn tex_obsidian(uv: vec2<f32>, world: vec3<f32>, _time: f32) -> vec3<f32> {
    let r = tex_block_rand(world, 23.0);
    let n = tex_vnoise(uv * 5.0 + vec2<f32>(r * 19.0, r * 17.0));
    let n2 = tex_vnoise(uv * 17.0 + vec2<f32>(r * 29.0, r * 23.0));
    let base = mix(vec3<f32>(0.07, 0.04, 0.10), vec3<f32>(0.16, 0.10, 0.22), n);
    let speck = smoothstep(0.75, 0.95, n2);
    return base + vec3<f32>(0.35, 0.25, 0.55) * speck * 0.5;
}

// 20: highlight — constant dark (wireframe block outline).
fn tex_highlight(_uv: vec2<f32>, _world: vec3<f32>, _time: f32) -> vec3<f32> {
    return vec3<f32>(0.03, 0.03, 0.03);
}

// ---- dispatch (must match the GLSL mirror) ------------------------------

/// Sample the texture with id `tex` at face uv `uv`, world position
/// `world`, time `time`. Unknown ids return debug magenta.
///
/// The id travels as a *smoothly interpolated* varying, so by the time the
/// fragment sees it it can be a ulp or two off the exact integer (the
/// barycentric-weighted sum of four equal values is not exactly that
/// value in f32). The dispatch therefore uses threshold comparisons, not
/// equality — an `if (tex == 6.0)` would miss a fraction of every face.
fn sample_tex(tex: f32, uv: vec2<f32>, world: vec3<f32>, time: f32) -> vec3<f32> {
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
    return vec3<f32>(1.0, 0.0, 1.0);
}

/// Alpha for the translucent pipeline (water and glass share it). Same
/// threshold rule as `sample_tex` (interpolated varying, not exact).
fn tex_trans_alpha(tex: f32) -> f32 {
    if (tex > 15.5 && tex < 16.5) { return 0.30; } // glass
    return 0.62;                                   // water
}
