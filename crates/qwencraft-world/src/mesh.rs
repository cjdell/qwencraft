//! Mesh building: turn a 26^3 chunk region into renderable geometry.
//!
//! Per chunk we compute:
//! - voxel sky light (BFS flood fill from the sky, attenuating by 1 per step)
//! - per-vertex ambient occlusion from the three corner neighbour blocks
//! - face culling (only faces adjacent to non-solid are emitted)
//!
//! The GPU samples each block's procedural texture in the fragment stage
//! (the texture id is a per-vertex attribute, see `qwencraft_world::block`),
//! so what is baked on the CPU is: world position, a **light scalar**
//! (sky light * face shading * AO) that multiplies the texture, the face
//! UV (0..1 across the face) and the texture id. The shader also applies
//! distance fog.
//!
//! Vertex layout (7 floats, 28 bytes): `[x, y, z, light, u, v, tex]`.
//! `u` is horizontal across the face, `v` vertical with 1 at the top edge
//! (top/bottom faces use u=x, v=z); `tex` is a `TEX_*` id.

use crate::{Block, REGION, TEX_HIGHLIGHT};

const R: usize = REGION as usize; // 26
const MARGIN: i32 = 5;

/// Vertex stride in floats (see the module docs for the layout).
pub const VERT_STRIDE: usize = 7;

/// A built chunk mesh (positions + baked light + UV + texture id, u32 indices).
pub struct MeshData {
    /// Interleaved [x, y, z, light, u, v, tex] in world space (opaque
    /// terrain + flower decals).
    pub vertices: Vec<f32>,
    pub indices: Vec<u32>,
    /// Translucent geometry (water + glass): separate, drawn with a
    /// translucent pipeline after all opaque geometry.
    pub water_vertices: Vec<f32>,
    pub water_indices: Vec<u32>,
}

impl MeshData {
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty() && self.water_indices.is_empty()
    }

    #[allow(clippy::len_without_is_empty)]
    pub fn index_count(&self) -> u32 {
        self.indices.len() as u32
    }
}

#[inline]
fn idx(x: i32, y: i32, z: i32) -> usize {
    ((y as usize) * R + (z as usize)) * R + x as usize
}

/// Build the mesh for the chunk whose origin is `origin`, from its 26^3
/// region payload (`data`, region-local coordinates).
pub fn build_chunk_mesh(origin: (i32, i32, i32), data: &[u8]) -> MeshData {
    debug_assert_eq!(data.len(), R * R * R);

    let solid = |x: i32, y: i32, z: i32| -> bool {
        if x < 0 || y < 0 || z < 0 || x >= R as i32 || y >= R as i32 || z >= R as i32 {
            return false;
        }
        Block::from_u8(data[idx(x, y, z)]).is_solid()
    };

    // ---- Voxel sky light -------------------------------------------------
    let mut light = vec![0u8; R * R * R];
    // Direct sky: air columns open to the top of the region.
    for x in 0..R as i32 {
        for z in 0..R as i32 {
            let mut top_solid = -1i32;
            let mut y = R as i32 - 1;
            while y >= 0 {
                if solid(x, y, z) {
                    top_solid = y;
                    break;
                }
                y -= 1;
            }
            y = top_solid + 1;
            while y < R as i32 {
                if !solid(x, y, z) {
                    light[idx(x, y, z)] = 15;
                }
                y += 1;
            }
        }
    }
    // Propagation (BFS; sky light attenuates by 1 per block).
    let mut queue: std::vec::Vec<usize> = Vec::new();
    for i in 0..(R * R * R) {
        if light[i] == 15 {
            queue.push(i);
        }
    }
    let mut head = 0usize;
    while head < queue.len() {
        let i = queue[head];
        head += 1;
        let l = light[i] as i32;
        if l <= 1 {
            continue;
        }
        let x = (i % R) as i32;
        let z = ((i / R) % R) as i32;
        let y = (i / (R * R)) as i32;
        for n in [
            (x + 1, y, z),
            (x - 1, y, z),
            (x, y + 1, z),
            (x, y - 1, z),
            (x, y, z + 1),
            (x, y, z - 1),
        ] {
            if n.0 < 0 || n.1 < 0 || n.2 < 0 || n.0 >= R as i32 || n.1 >= R as i32 || n.2 >= R as i32 {
                continue;
            }
            if solid(n.0, n.1, n.2) {
                continue;
            }
            let ni = idx(n.0, n.1, n.2);
            let cand = (l - 1) as u8;
            if light[ni] < cand {
                light[ni] = cand;
                queue.push(ni);
            }
        }
    }

    // ---- Geometry ---------------------------------------------------------
    let mut vertices: Vec<f32> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut water_vertices: Vec<f32> = Vec::new();
    let mut water_indices: Vec<u32> = Vec::new();

    // Core 16^3 (inset by the margin).
    for cy in 0..16i32 {
        for cz in 0..16i32 {
            for cx in 0..16i32 {
                let rx = MARGIN + cx;
                let ry = MARGIN + cy;
                let rz = MARGIN + cz;
                let b = Block::from_u8(data[idx(rx, ry, rz)]);

                // Water: translucent, separate pass. Faces are only emitted
                // against *air* (never against water or solid), and the top
                // surface sits slightly below the cell top. The texture
                // (rippling with world position + time) is sampled on the GPU.
                if b == Block::Water {
                    let air = |x: i32, y: i32, z: i32| -> bool {
                        (0..R as i32).contains(&x)
                            && (0..R as i32).contains(&y)
                            && (0..R as i32).contains(&z)
                            && Block::from_u8(data[idx(x, y, z)]) == Block::Air
                    };
                    let wx = (origin.0 + rx - MARGIN) as f32;
                    let wy = (origin.1 + ry - MARGIN) as f32;
                    let wz = (origin.2 + rz - MARGIN) as f32;
                    let top_h = if air(rx, ry + 1, rz) { 0.875 } else { 1.0 };
                    let light_of = |li: usize, shade: f32| -> f32 {
                        (0.38 + 0.62 * (light[li] as f32 / 15.0)) * shade
                    };
                    // Top (only against air).
                    if air(rx, ry + 1, rz) {
                        let l = light_of(idx(rx, ry + 1, rz), 1.0);
                        push_quad(
                            &mut water_vertices,
                            &mut water_indices,
                            &[
                                (wx, wy + top_h, wz, 0.0, 0.0),
                                (wx, wy + top_h, wz + 1.0, 0.0, 1.0),
                                (wx + 1.0, wy + top_h, wz + 1.0, 1.0, 1.0),
                                (wx + 1.0, wy + top_h, wz, 1.0, 0.0),
                            ],
                            [l; 4],
                            b.tex_for_dir(0),
                        );
                    }
                    // Bottom (a cave under the lake).
                    if air(rx, ry - 1, rz) {
                        let l = light_of(idx(rx, ry - 1, rz), 0.55);
                        push_quad(
                            &mut water_vertices,
                            &mut water_indices,
                            &[
                                (wx, wy, wz + 1.0, 0.0, 1.0),
                                (wx, wy, wz, 0.0, 0.0),
                                (wx + 1.0, wy, wz, 1.0, 0.0),
                                (wx + 1.0, wy, wz + 1.0, 1.0, 1.0),
                            ],
                            [l; 4],
                            b.tex_for_dir(1),
                        );
                    }
                    // Sides (only against air; the top edge follows the
                    // water surface). Side UVs: u = z, v = y (0 at bottom).
                    for (nx, nz, face) in [(1i32, 0i32, 2u32), (-1, 0, 3), (0, 1, 4), (0, -1, 5)] {
                        if air(rx + nx, ry, rz + nz) {
                            let l = light_of(
                                idx(rx + nx, ry, rz + nz),
                                match face {
                                    2 | 3 => 0.82,
                                    _ => 0.7,
                                },
                            );
                            let corners: [(f32, f32, f32, f32, f32); 4] = match face {
                                2 => [
                                    (wx + 1.0, wy, wz + 1.0, 1.0, 0.0),
                                    (wx + 1.0, wy, wz, 0.0, 0.0),
                                    (wx + 1.0, wy + top_h, wz, 0.0, 1.0),
                                    (wx + 1.0, wy + top_h, wz + 1.0, 1.0, 1.0),
                                ],
                                3 => [
                                    (wx, wy, wz, 0.0, 0.0),
                                    (wx, wy, wz + 1.0, 1.0, 0.0),
                                    (wx, wy + top_h, wz + 1.0, 1.0, 1.0),
                                    (wx, wy + top_h, wz, 0.0, 1.0),
                                ],
                                4 => [
                                    (wx, wy, wz + 1.0, 0.0, 0.0),
                                    (wx + 1.0, wy, wz + 1.0, 1.0, 0.0),
                                    (wx + 1.0, wy + top_h, wz + 1.0, 1.0, 1.0),
                                    (wx, wy + top_h, wz + 1.0, 0.0, 1.0),
                                ],
                                _ => [
                                    (wx, wy + top_h, wz, 0.0, 1.0),
                                    (wx + 1.0, wy + top_h, wz, 1.0, 1.0),
                                    (wx + 1.0, wy, wz, 1.0, 0.0),
                                    (wx, wy, wz, 0.0, 0.0),
                                ],
                            };
                            push_quad(
                                &mut water_vertices,
                                &mut water_indices,
                                &corners,
                                [l; 4],
                                b.tex_for_dir(2),
                            );
                        }
                    }
                    continue;
                }

                // Flowers: opaque plus-shaped decals on top of the cell.
                // Each bar of the plus samples a strip through the middle of
                // the flower texture (the two bars overlap at the centre;
                // identical pixels, so the double draw is invisible).
                if b.is_flower() {
                    let wx = (origin.0 + rx - MARGIN) as f32;
                    let wy = (origin.1 + ry - MARGIN) as f32 + 0.03;
                    let wz = (origin.2 + rz - MARGIN) as f32;
                    let l = 0.38 + 0.62 * (light[idx(rx, ry, rz)] as f32 / 15.0);
                    push_quad(
                        &mut vertices,
                        &mut indices,
                        &[
                            (wx + 0.19, wy, wz + 0.56, 0.0, 0.58),
                            (wx + 0.81, wy, wz + 0.56, 1.0, 0.58),
                            (wx + 0.81, wy, wz + 0.44, 1.0, 0.42),
                            (wx + 0.19, wy, wz + 0.44, 0.0, 0.42),
                        ],
                        [l; 4],
                        b.tex_for_dir(0),
                    );
                    push_quad(
                        &mut vertices,
                        &mut indices,
                        &[
                            (wx + 0.44, wy, wz + 0.19, 0.42, 0.0),
                            (wx + 0.44, wy, wz + 0.81, 0.42, 1.0),
                            (wx + 0.56, wy, wz + 0.81, 0.58, 1.0),
                            (wx + 0.56, wy, wz + 0.19, 0.58, 0.0),
                        ],
                        [l; 4],
                        b.tex_for_dir(0),
                    );
                    continue;
                }

                if !b.is_solid() {
                    continue;
                }

                // Glass is solid but translucent: its faces go to the
                // translucent sub-mesh (drawn after all opaque geometry with
                // blending and no depth writes), like water.
                let (out_v, out_i) = if b.is_translucent() {
                    (&mut water_vertices, &mut water_indices)
                } else {
                    (&mut vertices, &mut indices)
                };

                for face in 0..6 {
                    // face: 0:+Y 1:-Y 2:+X 3:-X 4:+Z 5:-Z
                    let (nx, ny, nz) = match face {
                        0 => (0i32, 1i32, 0i32),
                        1 => (0, -1, 0),
                        2 => (1, 0, 0),
                        3 => (-1, 0, 0),
                        4 => (0, 0, 1),
                        _ => (0, 0, -1),
                    };
                    let fx = rx + nx;
                    let fy = ry + ny;
                    let fz = rz + nz;
                    if solid(fx, fy, fz) {
                        continue; // hidden face
                    }

                    let tex = b.tex_for_dir(face as u8);
                    let face_shade = match face {
                        0 => 1.0,
                        1 => 0.55,
                        2 | 3 => 0.82,
                        _ => 0.7,
                    };
                    let l = light[idx(fx, fy, fz)] as f32 / 15.0;
                    let light_f = 0.38 + 0.62 * l;

                                    let corners: [(i32, i32, i32); 4] = match face {
                        0 => [(0, 1, 0), (0, 1, 1), (1, 1, 1), (1, 1, 0)],
                        1 => [(0, 0, 1), (0, 0, 0), (1, 0, 0), (1, 0, 1)],
                        2 => [(1, 0, 1), (1, 0, 0), (1, 1, 0), (1, 1, 1)],
                        3 => [(0, 0, 0), (0, 0, 1), (0, 1, 1), (0, 1, 0)],
                        4 => [(0, 0, 1), (1, 0, 1), (1, 1, 1), (0, 1, 1)],
                        _ => [(0, 1, 0), (1, 1, 0), (1, 0, 0), (0, 0, 0)],
                    };

                    // Ambient occlusion per corner: the 3 blocks around the
                    // corner in the face plane. Baked as a light scalar that
                    // multiplies the (GPU-sampled) texture colour.
                    let mut lights = [0.0f32; 4];
                    let mut out_corners: [(f32, f32, f32, f32, f32); 4] = [(0.0, 0.0, 0.0, 0.0, 0.0); 4];
                    for (i, corner) in corners.iter().enumerate() {
                        let signs = (corner.0 * 2 - 1, corner.1 * 2 - 1, corner.2 * 2 - 1);
                        let (s1, s2, sc) = ao_corners(fx, fy, fz, signs, face, &solid);
                        let ao = if s1 && s2 { 0 } else { 3 - s1 as u8 - s2 as u8 - sc as u8 };
                        let ao_f = [0.45f32, 0.62, 0.82, 1.0][ao as usize];
                        lights[i] = (light_f * face_shade * ao_f).min(1.0);
                        // rx/ry/rz are region-local (core starts at MARGIN);
                        // convert to chunk-local before adding the origin.
                        let wx = origin.0 + rx - MARGIN + corner.0;
                        let wy = origin.1 + ry - MARGIN + corner.1;
                        let wz = origin.2 + rz - MARGIN + corner.2;
                        // Face UVs: top/bottom use u=x, v=z; sides use
                        // u=(the face's horizontal world axis), v=y (1 =
                        // top edge — the grass overhang rim and the TNT
                        // label band live there). ±X faces are horizontal
                        // in z, but ±Z faces are horizontal in X: using z
                        // there made u constant across the whole face (z is
                        // the face's normal) and every ±Z face rendered as
                        // a 1D texture in v — flat horizontal bands.
                        let (u, v) = match face {
                            0 | 1 => (corner.0 as f32, corner.2 as f32),
                            2 | 3 => (corner.2 as f32, corner.1 as f32),
                            _ => (corner.0 as f32, corner.1 as f32),
                        };
                        out_corners[i] = (wx as f32, wy as f32, wz as f32, u, v);
                    }
                    push_quad(out_v, out_i, &out_corners, lights, tex);
                }
            }
        }
    }

    MeshData {
        vertices,
        indices,
        water_vertices,
        water_indices,
    }
}

/// Wireframe cube (24 vertices, 12 line segments) around block `t`,
/// inflated by 0.001 so the lines sit just outside the solid faces (no
/// z-fighting). Vertex layout: [x, y, z, light, u, v, tex] (same as chunk
/// meshes) with light = 1 and `tex` = `TEX_HIGHLIGHT` (a constant dark
/// colour in the texture dispatch). Drawn with a line-list pipeline
/// (client) / `gl.LINES` (shadow renderer).
pub fn highlight_vertices(t: (i32, i32, i32)) -> Vec<f32> {
    let (x, y, z) = (t.0 as f32 + 0.5, t.1 as f32 + 0.5, t.2 as f32 + 0.5);
    let h = 0.501f32;
    let mut out = Vec::with_capacity(24 * VERT_STRIDE);
    let c = |dx: f32, dy: f32, dz: f32| (x + dx, y + dy, z + dz);
    let seg = |out: &mut Vec<f32>, a: (f32, f32, f32), b: (f32, f32, f32)| {
        for p in [a, b] {
            out.extend_from_slice(&[p.0, p.1, p.2, 1.0, 0.0, 0.0, TEX_HIGHLIGHT as f32]);
        }
    };
    // Bottom square.
    seg(&mut out, c(-h, -h, -h), c(h, -h, -h));
    seg(&mut out, c(h, -h, -h), c(h, -h, h));
    seg(&mut out, c(h, -h, h), c(-h, -h, h));
    seg(&mut out, c(-h, -h, h), c(-h, -h, -h));
    // Top square.
    seg(&mut out, c(-h, h, -h), c(h, h, -h));
    seg(&mut out, c(h, h, -h), c(h, h, h));
    seg(&mut out, c(h, h, h), c(-h, h, h));
    seg(&mut out, c(-h, h, h), c(-h, h, -h));
    // Verticals.
    seg(&mut out, c(-h, -h, -h), c(-h, h, -h));
    seg(&mut out, c(h, -h, -h), c(h, h, -h));
    seg(&mut out, c(h, -h, h), c(h, h, h));
    seg(&mut out, c(-h, -h, h), c(-h, h, h));
    out
}

/// One convex quad (CCW as seen from outside): per-corner position + face
/// UV, per-corner light scalar, one texture id for the whole face.
fn push_quad(
    verts: &mut Vec<f32>,
    idxs: &mut Vec<u32>,
    corners: &[(f32, f32, f32, f32, f32); 4],
    lights: [f32; 4],
    tex: u8,
) {
    let start = verts.len() as u32 / VERT_STRIDE as u32;
    for ((px, py, pz, u, v), l) in corners.iter().zip(lights.iter()) {
        verts.extend_from_slice(&[
            *px,
            *py,
            *pz,
            l.min(1.0),
            *u,
            *v,
            tex as f32,
        ]);
    }
    idxs.extend_from_slice(&[start, start + 1, start + 2, start, start + 2, start + 3]);
}

/// For a face corner, evaluate the two side blocks and the corner block in
/// the face plane. `signs` are the corner directions along the two tangent
/// axes (per world axis, each -1 or +1; 0 on the normal axis).
fn ao_corners(
    fx: i32,
    fy: i32,
    fz: i32,
    signs: (i32, i32, i32),
    face: u32,
    solid: &dyn Fn(i32, i32, i32) -> bool,
) -> (bool, bool, bool) {
    let (t1, t2) = match face {
        0 | 1 => ((1i32, 0, 0), (0, 0, 1)),
        2 | 3 => ((0, 1, 0), (0, 0, 1)),
        _ => ((1, 0, 0), (0, 1, 0)),
    };
    let axis_sign = |axis: (i32, i32, i32)| -> i32 {
        if axis.0 != 0 {
            signs.0
        } else if axis.1 != 0 {
            signs.1
        } else {
            signs.2
        }
    };
    let s1 = axis_sign(t1);
    let s2 = axis_sign(t2);
    let p1 = (fx + t1.0 * s1, fy + t1.1 * s1, fz + t1.2 * s1);
    let p2 = (fx + t2.0 * s2, fy + t2.1 * s2, fz + t2.2 * s2);
    let pc = (
        fx + t1.0 * s1 + t2.0 * s2,
        fy + t1.1 * s1 + t2.1 * s2,
        fz + t1.2 * s1 + t2.2 * s2,
    );
    (solid(p1.0, p1.1, p1.2), solid(p2.0, p2.1, p2.2), solid(pc.0, pc.1, pc.2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Block, TEX_FLOWER_RED, TEX_FLOWER_YELLOW, TEX_GLASS, TEX_GRASS_TOP, TEX_HIGHLIGHT,
        TEX_LOG_SIDE, TEX_LOG_TOP, TEX_STONE, CHUNK, CHUNK_BLOCKS, WorldGen,
    };

    /// Every face quad must span the full 0..1 range of BOTH uv axes:
    /// if u (or v) is constant across a quad's four corners, that face
    /// samples only a 1D slice of its texture. This bit the ±Z sides once:
    /// they used u=z — the face's own normal direction — so u was constant
    /// and every north/south face rendered as flat horizontal bands while
    /// the ±X faces looked fine ("stripy on some sides, not others").
    /// Flower decals are the exception: each bar of the plus samples a
    /// strip of the flower texture by design.
    #[test]
    fn every_face_quad_spans_both_uv_axes() {
        let gen = WorldGen::new(1337);
        let mut checked = 0usize;
        for (cx, cy, cz) in [(0i32, 0, 0), (1, 0, 0), (0, 0, 1), (0, -1, 0), (2, 0, -1)] {
            let region = region_for(&gen, cx, cy, cz);
            let mesh = build_chunk_mesh(
                (cx * CHUNK, cy * CHUNK, cz * CHUNK),
                &region,
            );
            for (verts, label) in [
                (&mesh.vertices, "opaque"),
                (&mesh.water_vertices, "water"),
            ] {
                assert_eq!(
                    verts.len() % (4 * VERT_STRIDE),
                    0,
                    "{label}: vertex count not a multiple of 4"
                );
                for q in verts.chunks(4 * VERT_STRIDE) {
                    let tex = q[6] as u8;
                    if tex == TEX_FLOWER_RED || tex == TEX_FLOWER_YELLOW {
                        continue; // decal bars sample a strip, not a face
                    }
                    let axis = |k: usize| -> (f32, f32) {
                        let mut lo = f32::INFINITY;
                        let mut hi = f32::NEG_INFINITY;
                        for i in 0..4usize {
                            let v = q[i * VERT_STRIDE + k];
                            lo = lo.min(v);
                            hi = hi.max(v);
                        }
                        (lo, hi)
                    };
                    let (u_lo, u_hi) = axis(4);
                    let (v_lo, v_hi) = axis(5);
                    assert!(
                        (u_hi - u_lo - 1.0).abs() < 1e-5,
                        "face with tex {tex} does not span u (lo {u_lo}, hi {u_hi})"
                    );
                    assert!(
                        (v_hi - v_lo - 1.0).abs() < 1e-5,
                        "face with tex {tex} does not span v (lo {v_lo}, hi {v_hi})"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 500, "expected plenty of face quads, only saw {checked}");
    }

    fn region_for(gen: &WorldGen, cx: i32, cy: i32, cz: i32) -> Vec<u8> {
        // Assemble a 26^3 region from the generator (3x3x3 chunk neighbourhood).
        let mut out = vec![0u8; R * R * R];
        let ox = cx * CHUNK - MARGIN;
        let oy = cy * CHUNK - MARGIN;
        let oz = cz * CHUNK - MARGIN;
        for y in 0..R as i32 {
            for z in 0..R as i32 {
                for x in 0..R as i32 {
                    let b = gen.block_at(ox + x, oy + y, oz + z);
                    out[idx(x, y, z)] = b.as_u8();
                }
            }
        }
        out
    }

    #[test]
    fn mesh_has_geometry_and_valid_indices() {
        let gen = WorldGen::new(1337);
        let (cx, cy, cz) = (0, 0, 0);
        let region = region_for(&gen, cx, cy, cz);
        let mesh = build_chunk_mesh((0, 0, 0), &region);
        assert!(!mesh.is_empty(), "terrain chunk should produce faces");
        assert_eq!(mesh.vertices.len() % VERT_STRIDE, 0);
        let vcount = mesh.vertices.len() as u32 / VERT_STRIDE as u32;
        assert!((mesh.indices.iter().copied().max().unwrap() as usize) < vcount as usize);
        // Per-vertex attributes: light in [0,1], UV in [0,1], tex a valid id.
        for v in mesh.vertices.chunks(VERT_STRIDE) {
            assert!((0.0..=1.0).contains(&v[3]), "light out of range: {v:?}");
            assert!((0.0..=1.0).contains(&v[4]), "uv.x out of range: {v:?}");
            assert!((0.0..=1.0).contains(&v[5]), "uv.y out of range: {v:?}");
            assert!(
                (v[6] as usize) <= TEX_HIGHLIGHT as usize,
                "unknown texture id: {v:?}"
            );
        }
        let _ = CHUNK_BLOCKS;
    }

    #[test]
    fn highlight_is_a_24_vertex_cube_around_the_block() {
        let v = highlight_vertices((3, 4, 5));
        assert_eq!(v.len(), 24 * VERT_STRIDE);
        // Every corner must hug the block (3..6) from just outside, with
        // full light and the highlight texture id.
        for c in v.chunks(VERT_STRIDE) {
            assert!((2.99..6.01).contains(&c[0]), "x {} out of range", c[0]);
            assert!((2.99..6.01).contains(&c[1]), "y {} out of range", c[1]);
            assert!((2.99..6.01).contains(&c[2]), "z {} out of range", c[2]);
            assert_eq!(c[3], 1.0, "highlight light must be full");
            assert_eq!(c[6], TEX_HIGHLIGHT as f32, "highlight tex id");
        }
    }

    #[test]
    fn light_reaches_under_overhangs_partially() {
        // Synthetic region: stone floor at y=8 (inside the meshable 16^3 core,
        // which is local 5..21) and a ceiling at y=12 covering half the core.
        // Faces must be emitted for the floor and the underside of the
        // ceiling; the mesh must not be empty.
        let mut region = vec![0u8; R * R * R];
        for x in 0..R as i32 {
            for z in 0..R as i32 {
                region[idx(x, 8, z)] = 3; // stone floor
            }
        }
        for x in 5..21i32 {
            for z in 5..21i32 {
                region[idx(x, 12, z)] = 3; // ceiling
            }
        }
        let mesh = build_chunk_mesh((0, 0, 0), &region);
        assert!(!mesh.is_empty());
    }

    #[test]
    fn surface_top_faces_carry_full_light_and_grass_texture() {
        // Regression test: chunk vertices must be placed at
        // `origin + chunk-local`, not offset by the region margin. A +MARGIN
        // offset shifts every chunk's geometry and leaves players inside
        // misaligned walls.
        let gen = WorldGen::new(1337);
        let h = gen.height(8, 8);
        let cy = ((h - 1) / 16).clamp(0, 3);
        let region = region_for(&gen, 0, cy, 0);
        let mesh = build_chunk_mesh((0, cy * 16, 0), &region);
        // The top face of the surface block at (8, h, 8) has corners at
        // y = h + 1, x in 8..10, z in 8..10.
        let mut found = 0u32;
        for v in mesh.vertices.chunks(VERT_STRIDE) {
            if v[1] == (h + 1) as f32 && (8.0..10.0).contains(&v[0]) && (8.0..10.0).contains(&v[2]) {
                found += 1;
                // Full sky light + top face + no AO => light exactly 1.0,
                // and the grass-top texture (the GPU samples it).
                assert!((v[3] - 1.0).abs() < 1e-6, "light={} at {v:?}", v[3]);
                assert_eq!(v[6], TEX_GRASS_TOP as f32, "tex at {v:?}");
            }
        }
        assert!(found >= 4, "expected the grass top face at spawn, found {found} corners");
    }

    /// A synthetic all-air region with one block of `b` at region-local
    /// (13, 13, 13) (inside the 16^3 core, 5..21).
    fn single_block_region(b: Block) -> Vec<u8> {
        let mut region = vec![0u8; R * R * R];
        region[idx(13, 13, 13)] = b.as_u8();
        region
    }

    /// Face count of a mesh (each face = 4 vertices = 6 indices).
    fn face_count(vertices: &[f32], indices: &[u32]) -> (u32, u32) {
        (indices.len() as u32 / 6, vertices.len() as u32 / VERT_STRIDE as u32 / 4)
    }

    #[test]
    fn glass_emits_faces_only_where_neighbour_is_not_solid() {
        // A lone glass block: all 6 faces, all in the TRANSLUCENT sub-mesh
        // (drawn after opaque with blending, no depth writes).
        let mesh = build_chunk_mesh((0, 0, 0), &single_block_region(Block::Glass));
        let (opaque_faces, _) = face_count(&mesh.vertices, &mesh.indices);
        let (trans_faces, _) = face_count(&mesh.water_vertices, &mesh.water_indices);
        assert_eq!(opaque_faces, 0, "glass has no opaque geometry");
        assert_eq!(trans_faces, 6, "lone glass block shows all 6 faces");
        assert_eq!(mesh.water_vertices.len() / VERT_STRIDE, 24);
        // Every translucent vertex carries the glass texture id.
        for v in mesh.water_vertices.chunks(VERT_STRIDE) {
            assert_eq!(v[6], TEX_GLASS as f32);
        }
        // Glass against stone: the shared face is culled (stone is solid).
        let mut region = single_block_region(Block::Glass);
        region[idx(14, 13, 13)] = Block::Stone.as_u8();
        let mesh = build_chunk_mesh((0, 0, 0), &region);
        let (trans_faces, _) = face_count(&mesh.water_vertices, &mesh.water_indices);
        assert_eq!(trans_faces, 5, "glass-stone face must be culled");
        // Glass against glass: same rule (Minecraft-style).
        let mut region = single_block_region(Block::Glass);
        region[idx(14, 13, 13)] = Block::Glass.as_u8();
        let mesh = build_chunk_mesh((0, 0, 0), &region);
        let (trans_faces, _) = face_count(&mesh.water_vertices, &mesh.water_indices);
        assert_eq!(trans_faces, 10, "glass-glass face must be culled");
    }

    #[test]
    fn flower_decals_sample_the_flower_texture() {
        let mesh = build_chunk_mesh((0, 0, 0), &single_block_region(Block::FlowerRed));
        // Two quads (the plus), opaque pass, flower texture, UVs inside the
        // two middle strips of the texture.
        assert_eq!(mesh.indices.len(), 12);
        assert!(mesh.water_indices.is_empty());
        for v in mesh.vertices.chunks(VERT_STRIDE) {
            assert_eq!(v[6], TEX_FLOWER_RED as f32);
            // Each bar samples a strip: one of the UV axes is full [0,1],
            // the other stays in the middle band [0.42, 0.58].
            let (u, w) = (v[4], v[5]);
            assert!((0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&w));
            assert!(
                (0.42..=0.58).contains(&u) || (0.42..=0.58).contains(&w),
                "one UV axis must stay in the flower strip: {v:?}"
            );
        }
    }

    #[test]
    fn solid_block_faces_carry_their_face_textures() {
        // Log: top/bottom are the ring texture, sides are the bark texture.
        let mesh = build_chunk_mesh((0, 0, 0), &single_block_region(Block::Log));
        let mut tops = 0;
        let mut sides = 0;
        for v in mesh.vertices.chunks(VERT_STRIDE) {
            match v[6] as u8 {
                TEX_LOG_TOP => tops += 1,
                TEX_LOG_SIDE => sides += 1,
                other => panic!("unexpected texture {other} on log vertex {v:?}"),
            }
        }
        // 2 ring faces * 4 verts + 4 bark faces * 4 verts.
        assert_eq!((tops, sides), (8, 16));
        // Stone: everything is the stone texture.
        let mesh = build_chunk_mesh((0, 0, 0), &single_block_region(Block::Stone));
        for v in mesh.vertices.chunks(VERT_STRIDE) {
            assert_eq!(v[6], TEX_STONE as f32);
        }
    }

    #[test]
    fn water_stays_in_the_translucent_pass() {
        let mesh = build_chunk_mesh((0, 0, 0), &single_block_region(Block::Water));
        assert!(mesh.vertices.is_empty(), "water has no opaque geometry");
        assert!(!mesh.water_vertices.is_empty(), "water must be translucent geometry");
        for v in mesh.water_vertices.chunks(VERT_STRIDE) {
            assert_eq!(v[6], crate::TEX_WATER as f32);
        }
    }

}
