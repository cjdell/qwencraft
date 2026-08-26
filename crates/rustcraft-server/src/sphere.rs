//! Agent rendering: coloured spheres (baked positions, per-frame upload).

use crate::AgentState;

const RINGS: u32 = 10;
const SEGMENTS: u32 = 16;

/// Generate sphere vertices (pos + baked colour) and indices for one agent.
pub fn sphere_mesh(agent: &AgentState) -> (Vec<f32>, Vec<u32>) {
    let r = agent.radius;
    let c = agent.pos;
    let center = [c.x, c.y + r, c.z];
    let (cr, cg, cb) = (
        agent.color[0] as f32 / 255.0,
        agent.color[1] as f32 / 255.0,
        agent.color[2] as f32 / 255.0,
    );

    let mut verts: Vec<[f32; 3]> = Vec::with_capacity((RINGS + 1) as usize * SEGMENTS as usize);
    let mut indices: Vec<u32> = Vec::with_capacity((RINGS * SEGMENTS * 6) as usize);

    for i in 0..=RINGS {
        let phi = std::f32::consts::PI * i as f32 / RINGS as f32;
        let (sp, cp) = phi.sin_cos();
        for j in 0..SEGMENTS {
            let theta = std::f32::consts::TAU * j as f32 / SEGMENTS as f32;
            let (st, ct) = theta.sin_cos();
            let nx = sp * ct;
            let ny = cp;
            let nz = sp * st;
            verts.push([
                center[0] + r * nx,
                center[1] + r * ny,
                center[2] + r * nz,
            ]);
        }
    }
    // Bake positions + colour (simple lambert shading for a touch of depth).
    let mut out: Vec<f32> = Vec::with_capacity(verts.len() * 6);
    for (i, v) in verts.iter().enumerate() {
        let ny = ((v[1] - center[1]) / r).max(0.0);
        let shade = 0.65 + 0.35 * ny;
        out.extend_from_slice(&[
            v[0],
            v[1],
            v[2],
            (cr * shade).min(1.0),
            (cg * shade).min(1.0),
            (cb * shade).min(1.0),
        ]);
        let _ = i;
    }
    for i in 0..RINGS {
        for j in 0..SEGMENTS {
            let a = i * SEGMENTS + j;
            let b = i * SEGMENTS + (j + 1) % SEGMENTS;
            let c = (i + 1) * SEGMENTS + j;
            let d = (i + 1) * SEGMENTS + (j + 1) % SEGMENTS;
            // Winding: CCW viewed from outside.
            indices.extend_from_slice(&[a, b, c, b, d, c]);
        }
    }
    (out, indices)
}
