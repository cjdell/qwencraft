//! The pipeline WGSL source.
//!
//! Terrain vertices carry world-space position, a baked **light scalar**
//! (voxel lighting + AO, computed at mesh build time), the face UV and a
//! texture id. The fragment stage samples the block's procedural texture
//! (one WGSL function per block — `textures.rs`, registered in
//! `qwencraft_world::block`), multiplies by the light scalar and applies
//! distance fog towards the sky colour.
//!
//! The full module is `SHADER` + `TEXTURES` (concatenated once at renderer
//! init; WGSL function order is irrelevant). The matching uniform-block
//! serialization is `qwencraft_world::camera::uniform_bytes`; the two must
//! stay in lockstep (see the comment in `SHADER`).
//!
//! The agent spheres keep their baked per-vertex colour (they are not
//! blocks): they use the `vs_agent`/`fs_agent` pair with the old
//! pos+colour vertex layout.

pub const SHADER: &str = include_str!("shader.wgsl");
