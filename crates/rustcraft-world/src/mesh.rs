//! Mesh building: turn a 26^3 chunk region into renderable geometry.
//!
//! Per chunk we compute:
//! - voxel sky light (BFS flood fill from the sky, attenuating by 1 per step)
//! - per-vertex ambient occlusion from the three corner neighbour blocks
//! - face culling (only faces adjacent to air are emitted)
//!
//! Vertex colours are fully baked on the CPU: block base colour * face
//! shading * light * AO. The GPU shader only applies distance fog.

use crate::{Block, REGION};

const R: usize = REGION as usize; // 26
const MARGIN: i32 = 5;

/// A built chunk mesh (positions + baked colours, u32 indices).
pub struct MeshData {
    /// Interleaved [x, y, z, r, g, b] in world space (opaque terrain +
    /// flower decals).
    pub vertices: Vec<f32>,
    pub indices: Vec<u32>,
    /// Water: separate geometry, drawn with a translucent pipeline after
    /// all opaque geometry.
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
                // surface sits slightly below the cell top.
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
                    let shaded = |li: usize, base: [f32; 3], shade: f32| -> [f32; 3] {
                        let f = 0.38 + 0.62 * (light[li] as f32 / 15.0);
                        [base[0] * f * shade, base[1] * f * shade, base[2] * f * shade]
                    };
                    // Top (only against air).
                    if air(rx, ry + 1, rz) {
                        let c = shaded(idx(rx, ry + 1, rz), b.color_top(), 1.0);
                        push_quad(
                            &mut water_vertices,
                            &mut water_indices,
                            &[
                                (wx, wy + top_h, wz),
                                (wx, wy + top_h, wz + 1.0),
                                (wx + 1.0, wy + top_h, wz + 1.0),
                                (wx + 1.0, wy + top_h, wz),
                            ],
                            c,
                        );
                    }
                    // Bottom (a cave under the lake).
                    if air(rx, ry - 1, rz) {
                        let c = shaded(idx(rx, ry - 1, rz), b.color_bottom(), 0.55);
                        push_quad(
                            &mut water_vertices,
                            &mut water_indices,
                            &[
                                (wx, wy, wz + 1.0),
                                (wx, wy, wz),
                                (wx + 1.0, wy, wz),
                                (wx + 1.0, wy, wz + 1.0),
                            ],
                            c,
                        );
                    }
                    // Sides (only against air; the top edge follows the
                    // water surface).
                    for (nx, nz, face) in [(1i32, 0i32, 2u32), (-1, 0, 3), (0, 1, 4), (0, -1, 5)] {
                        if air(rx + nx, ry, rz + nz) {
                            let c = shaded(
                                idx(rx + nx, ry, rz + nz),
                                b.color_side(),
                                match face {
                                    2 | 3 => 0.82,
                                    _ => 0.7,
                                },
                            );
                            let corners: [(f32, f32, f32); 4] = match face {
                                2 => [
                                    (wx + 1.0, wy, wz + 1.0),
                                    (wx + 1.0, wy, wz),
                                    (wx + 1.0, wy + top_h, wz),
                                    (wx + 1.0, wy + top_h, wz + 1.0),
                                ],
                                3 => [
                                    (wx, wy, wz),
                                    (wx, wy, wz + 1.0),
                                    (wx, wy + top_h, wz + 1.0),
                                    (wx, wy + top_h, wz),
                                ],
                                4 => [
                                    (wx, wy, wz + 1.0),
                                    (wx + 1.0, wy, wz + 1.0),
                                    (wx + 1.0, wy + top_h, wz + 1.0),
                                    (wx, wy + top_h, wz + 1.0),
                                ],
                                _ => [
                                    (wx, wy + top_h, wz),
                                    (wx + 1.0, wy + top_h, wz),
                                    (wx + 1.0, wy, wz),
                                    (wx, wy, wz),
                                ],
                            };
                            push_quad(
                                &mut water_vertices,
                                &mut water_indices,
                                &corners,
                                c,
                            );
                        }
                    }
                    continue;
                }

                // Flowers: opaque plus-shaped decals on top of the cell.
                if b == Block::FlowerRed || b == Block::FlowerYellow {
                    let wx = (origin.0 + rx - MARGIN) as f32;
                    let wy = (origin.1 + ry - MARGIN) as f32 + 0.03;
                    let wz = (origin.2 + rz - MARGIN) as f32;
                    let f = 0.38 + 0.62 * (light[idx(rx, ry, rz)] as f32 / 15.0);
                    let base = b.color_top();
                    let c = [base[0] * f, base[1] * f, base[2] * f];
                    push_quad(
                        &mut vertices,
                        &mut indices,
                        &[
                            (wx + 0.19, wy, wz + 0.56),
                            (wx + 0.81, wy, wz + 0.56),
                            (wx + 0.81, wy, wz + 0.44),
                            (wx + 0.19, wy, wz + 0.44),
                        ],
                        c,
                    );
                    push_quad(
                        &mut vertices,
                        &mut indices,
                        &[
                            (wx + 0.44, wy, wz + 0.19),
                            (wx + 0.44, wy, wz + 0.81),
                            (wx + 0.56, wy, wz + 0.81),
                            (wx + 0.56, wy, wz + 0.19),
                        ],
                        c,
                    );
                    continue;
                }

                if !b.is_solid() {
                    continue;
                }

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

                    let base_color = match face {
                        0 => b.color_top(),
                        1 => b.color_bottom(),
                        _ => b.color_side(),
                    };
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

                    let start = vertices.len() as u32 / 6;
                    for corner in corners {
                        // Ambient occlusion: the 3 blocks around this corner
                        // in the face plane. Corner direction along each
                        // tangent axis: +1 if the corner is on the + side.
                        let signs = (corner.0 * 2 - 1, corner.1 * 2 - 1, corner.2 * 2 - 1);
                        let (s1, s2, sc) = ao_corners(fx, fy, fz, signs, face, &solid);
                        let ao = if s1 && s2 { 0 } else { 3 - s1 as u8 - s2 as u8 - sc as u8 };
                        let ao_f = [0.45f32, 0.62, 0.82, 1.0][ao as usize];

                        let mut br = light_f * face_shade * ao_f;
                        br = br.min(1.0);
                        // rx/ry/rz are region-local (core starts at MARGIN);
                        // convert to chunk-local before adding the origin.
                        let wx = origin.0 + rx - MARGIN + corner.0;
                        let wy = origin.1 + ry - MARGIN + corner.1;
                        let wz = origin.2 + rz - MARGIN + corner.2;
                        vertices.extend_from_slice(&[
                            wx as f32,
                            wy as f32,
                            wz as f32,
                            (base_color[0] * br).min(1.0),
                            (base_color[1] * br).min(1.0),
                            (base_color[2] * br).min(1.0),
                        ]);
                    }
                    indices.extend_from_slice(&[start, start + 1, start + 2, start, start + 2, start + 3]);
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
/// z-fighting). Vertex layout: [x, y, z, r, g, b] (same as chunk meshes).
/// Drawn with a line-list pipeline (client) / `gl.LINES` (shadow renderer).
pub fn highlight_vertices(t: (i32, i32, i32)) -> Vec<f32> {
    const C: [f32; 3] = [0.03, 0.03, 0.03];
    let (x, y, z) = (t.0 as f32 + 0.5, t.1 as f32 + 0.5, t.2 as f32 + 0.5);
    let h = 0.501f32;
    let mut out = Vec::with_capacity(24 * 6);
    let c = |dx: f32, dy: f32, dz: f32| (x + dx, y + dy, z + dz);
    let seg = |out: &mut Vec<f32>, a: (f32, f32, f32), b: (f32, f32, f32)| {
        out.extend_from_slice(&[a.0, a.1, a.2, C[0], C[1], C[2], b.0, b.1, b.2, C[0], C[1], C[2]]);
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

/// One convex quad (CCW as seen from outside) with a baked colour.
fn push_quad(
    verts: &mut Vec<f32>,
    idxs: &mut Vec<u32>,
    corners: &[(f32, f32, f32); 4],
    c: [f32; 3],
) {
    let start = verts.len() as u32 / 6;
    for (px, py, pz) in corners {
        verts.extend_from_slice(&[
            *px,
            *py,
            *pz,
            c[0].min(1.0),
            c[1].min(1.0),
            c[2].min(1.0),
        ]);
    }
    idxs.extend_from_slice(&[start, start + 1, start + 2, start, start + 2, start + 3]);
}

/// For a face corner, evaluate the two side blocks and the corner block in
/// the face plane. `signs` are the corner directions along the two tangent
/// axes (each -1 or +1).
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
    use crate::{CHUNK, CHUNK_BLOCKS, WorldGen};

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
        assert_eq!(mesh.vertices.len() % 6, 0);
        let vcount = mesh.vertices.len() as u32 / 6;
        assert!((mesh.indices.iter().copied().max().unwrap() as usize) < vcount as usize);
        // Vertex colours (last 3 components) should be in [0,1].
        for v in mesh.vertices.chunks(6) {
            for c in &v[3..6] {
                assert!((*c) >= 0.0 && (*c) <= 1.0, "colour out of range: {c}");
            }
        }
        let _ = CHUNK_BLOCKS;
    }

    #[test]
    fn highlight_is_a_24_vertex_cube_around_the_block() {
        let v = highlight_vertices((3, 4, 5));
        assert_eq!(v.len(), 24 * 6);
        // Every corner must hug the block (3..6) from just outside.
        for c in v.chunks(6) {
            assert!((2.99..6.01).contains(&c[0]), "x {} out of range", c[0]);
            assert!((2.99..6.01).contains(&c[1]), "y {} out of range", c[1]);
            assert!((2.99..6.01).contains(&c[2]), "z {} out of range", c[2]);
            for col in &c[3..6] {
                assert!((0.0..=1.0).contains(col));
            }
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
    fn surface_top_faces_are_bright_green_at_full_light() {
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
        for v in mesh.vertices.chunks(6) {
            if v[1] == (h + 1) as f32 && (8.0..10.0).contains(&v[0]) && (8.0..10.0).contains(&v[2]) {
                found += 1;
                // Full sky light + top face + no AO => exactly the grass top
                // colour (0.36, 0.65, 0.28).
                assert!((v[3] - 0.36).abs() < 0.01, "r={} at {v:?}", v[3]);
                assert!((v[4] - 0.65).abs() < 0.01, "g={} at {v:?}", v[4]);
                assert!((v[5] - 0.28).abs() < 0.01, "b={} at {v:?}", v[5]);
            }
        }
        assert!(found >= 4, "expected the grass top face at spawn, found {found} corners");
    }

}
