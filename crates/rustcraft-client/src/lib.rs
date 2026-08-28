//! WebGPU (wgpu) renderer for RustCraft.
//!
//! Chunks are meshed on the CPU (voxel lighting + AO baked into vertex
//! colours, see `rustcraft_world::mesh`) and uploaded as static buffers;
//! agents are spheres re-uploaded each frame. One shared pipeline renders
//! everything (pos+colour vertices, distance fog in the fragment stage).
//!
//! This crate only compiles for wasm32 (WebGPU in the browser).

#![cfg(target_arch = "wasm32")]

use wgpu::{
    Buffer, BufferUsages, Device, DeviceDescriptor, Instance, InstanceDescriptor, PipelineLayout,
    Queue, RenderPipeline, RenderPipelineDescriptor, RequestAdapterOptions, ShaderModule,
    ShaderModuleDescriptor, ShaderSource, Surface, SurfaceConfiguration, SurfaceTarget,
    TextureFormat, TextureViewDescriptor, VertexAttribute, VertexBufferLayout, VertexFormat,
    VertexStepMode,
};

use rustcraft_server::{AgentState, WorldUpdate, VIEW_RADIUS};
use rustcraft_world::camera::{
    uniform_bytes, view_projection, FOG_END, FOG_START, SHADER, SKY, UNIFORM_SIZE,
};
use rustcraft_world::{ChunkPos, REGION_BLOCKS, TERRAIN_POOL_IDX, TERRAIN_POOL_VERTS};
// Pool capacity lives in rustcraft_world so the host-side `pool_measure`
// example can assert the worst-case view stays under ~80% of it. Do not
// fork these numbers here (the old 2M/3M fork caused exactly the
// visible-eviction thrash the walk test guards against).
const IDX_CAP: u32 = TERRAIN_POOL_IDX;
const VERT_CAP: u32 = TERRAIN_POOL_VERTS;

/// Terrain mesh pool. Chunks append into one pre-allocated vertex/index
/// buffer pair, so a frame costs one set_index_buffer + one
/// set_vertex_buffer plus a single draw_indexed per chunk (instead of
/// three state changes per chunk). When the pool fills up, chunks far from
/// the player are dropped and the survivors are compacted to the front
/// (CPU-side copies are kept so this needs no GPU readback).
///
/// `VERT_CAP`/`IDX_CAP` (in `rustcraft_world`) are sized to hold the
/// ENTIRE worst-case streamed view — a view bigger than the pool forces
/// compaction to drop still-visible chunks, which then thrash on the
/// evict/re-send loop. Keep the worst case under ~80% of the caps
/// (measured by rustcraft-server's `pool_measure` example).
/// Chunks at 3D Chebyshev chunk-cell distance >= this are fully inside
/// the fog: their nearest corner is (d-1)*16 > FOG_END blocks from the
/// camera, so they are invisible and can be dropped without a re-send.
/// (FOG_END = 108 blocks -> d >= 8.)
struct TerrainChunk {
    pos: ChunkPos,
    verts: Vec<f32>,
    idxs: Vec<u32>,
    base_v: u32,
    base_i: u32,
    /// Water sub-mesh, appended after the opaque part in the same pool.
    /// Its draw offsets are derived: base_i + opaque index count, and
    /// base_vertex = base_v + opaque vertex count.
    water: Option<(Vec<f32>, Vec<u32>)>,
}

/// One agent's sphere buffers. Keyed by agent id in `Renderer::agents`
/// (the id is the map key, not a field).
struct AgentMesh {
    vertex: Buffer,
    index: Buffer,
    count: u32,
}

/// The renderer.
pub struct Renderer {
    device: Device,
    queue: Queue,
    surface: Surface<'static>,
    surface_format: TextureFormat,
    pipeline: RenderPipeline,
    /// Translucent water pipeline (src-alpha blend, no depth writes);
    /// drawn after all opaque geometry.
    water_pipeline: RenderPipeline,
    /// Wireframe pipeline for the block highlight (line list, no cull).
    line_pipeline: RenderPipeline,
    /// 24-vertex wireframe cube, re-uploaded when the target changes.
    highlight_vbo: Buffer,
    /// The block under the crosshair (server-computed); None = no target.
    highlight: Option<[i32; 3]>,
    bind_group_layout: wgpu::BindGroupLayout,
    uniform_buf: Buffer,
    terrain_vbo: Buffer,
    terrain_ibo: Buffer,
    terrain_v_used: u32,
    terrain_i_used: u32,
    /// Meshed terrain chunks (CPU-side copies + pool offsets).
    terrain: Vec<TerrainChunk>,
    /// Chunks evicted from the pool by pressure (visible or fog-bound);
    /// the app reports them to the streamer, which re-sends the ones that
    /// are visible again (see `take_evicted`).
    evicted: Vec<ChunkPos>,
    /// Every chunk ever meshed (received from the server), including
    /// chunks since evicted by pool pressure. Telemetry only (the
    /// `POOL` log line / `missing_visible`): a chunk that is known but not
    /// in the pool while visible is a hole. (Re-sending itself is the
    /// streamer's job, driven by `note_evicted`.)
    known: std::collections::HashSet<ChunkPos>,
    /// Rate-limits the "pool full" console warning.
    pool_full_warns: u32,
    /// Agent spheres by id (HashMap: the NPC load test can push thousands
    /// of agents, and the per-frame update must stay O(n) not O(n^2)).
    agents: std::collections::HashMap<u32, AgentMesh>,
    /// Chunk updates waiting to be meshed (budgeted per frame).
    backlog: Vec<WorldUpdate>,
    width: u32,
    height: u32,
    camera: [f32; 3],
    yaw: f32,
    pitch: f32,
    first_frame: bool,
}

impl Renderer {
    /// Create the renderer (async: adapter/device requests).
    pub async fn new(canvas: &web_sys::HtmlCanvasElement) -> Result<Renderer, String> {
        // WebGPU is only exposed in *secure contexts* (https:// or
        // localhost). On any other origin `navigator.gpu` is undefined and
        // wgpu's `Instance::new` panics with a misleading message — check
        // first and fail with an actionable error instead.
        let Some(window) = web_sys::window() else {
            return Err("no window available".to_string());
        };
        // `navigator.gpu` is undefined outside secure contexts (check via
        // Reflect: web-sys's typed `Navigator::gpu` needs an unstable cfg).
        let has_gpu = js_sys::Reflect::get(
            &window.navigator().into(),
            &wasm_bindgen::JsValue::from_str("gpu"),
        )
        .map(|v| !v.is_undefined() && !v.is_null())
        .unwrap_or(false);
        if !has_gpu {
            return Err(
                "WebGPU is unavailable: browsers only expose it in secure contexts \
                 (https:// or http://localhost). Open http://localhost:8080 on the \
                 machine running the server, or from another device use \
                 `./scripts/serve.sh --https` (self-signed cert — accept the \
                 browser's security warning)."
                    .to_string(),
            );
        }
        let instance = Instance::new(&InstanceDescriptor::default());
        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str("RustCraft: creating surface"));
        let surface = instance
            .create_surface(SurfaceTarget::Canvas(canvas.clone()))
            .map_err(|e| format!("surface creation failed: {e:?}"))?;

        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str("RustCraft: requesting adapter"));
        let adapter = instance
            .request_adapter(&RequestAdapterOptions::default())
            .await
            .map_err(|e| format!("no WebGPU adapter available: {e:?}"))?;
        log_or_panic_adapter(&adapter);

        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str("RustCraft: requesting device"));
        let (device, queue) = adapter
            .request_device(&DeviceDescriptor {
                label: Some("rustcraft-device"),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("device request failed: {e:?}"))?;

        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str("RustCraft: configuring surface"));
        let caps = surface.get_capabilities(&adapter);
        let surface_format = *caps
            .formats
            .first()
            .ok_or_else(|| "surface has no formats".to_string())?;

        let (width, height) = canvas_size(canvas);
        configure_surface(&surface, &device, surface_format, width, height);

        // Pipeline: pos(3f) + color(3f) vertices, one uniform buffer.
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("rustcraft-shader"),
            source: ShaderSource::Wgsl(SHADER.into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rustcraft-bg-layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rustcraft-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = make_pipeline(&device, &shader, &layout, surface_format, "fs_main", false);
        let water_pipeline =
            make_pipeline(&device, &shader, &layout, surface_format, "fs_water", true);

        // Highlight: same vertices/shader as terrain, but line list with no
        // culling (lines are visible from both sides).
        let line_pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("rustcraft-highlight"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[VertexBufferLayout {
                    array_stride: 24,
                    step_mode: VertexStepMode::Vertex,
                    attributes: &[
                        VertexAttribute {
                            offset: 0,
                            format: VertexFormat::Float32x3,
                            shader_location: 0,
                        },
                        VertexAttribute {
                            offset: 12,
                            format: VertexFormat::Float32x3,
                            shader_location: 1,
                        },
                    ],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::LineList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: true,
                depth_compare: wgpu::CompareFunction::Less,
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });
        let highlight_vbo = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("highlight-vertices"),
            size: (24 * 24) as u64,
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rustcraft-uniforms"),
            size: UNIFORM_SIZE,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let terrain_vbo = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("terrain-vertices"),
            size: (VERT_CAP * 24) as u64,
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let terrain_ibo = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("terrain-indices"),
            size: (IDX_CAP * 4) as u64,
            usage: BufferUsages::INDEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Renderer {
            device,
            queue,
            surface,
            surface_format,
            pipeline,
            water_pipeline,
            line_pipeline,
            highlight_vbo,
            highlight: None,
            bind_group_layout,
            uniform_buf,
            terrain_vbo,
            terrain_ibo,
            terrain_v_used: 0,
            terrain_i_used: 0,
            terrain: Vec::new(),
            evicted: Vec::new(),
            known: std::collections::HashSet::new(),
            pool_full_warns: 0,
            agents: std::collections::HashMap::new(),
            backlog: Vec::new(),
            width,
            height,
            camera: [0.0, 40.0, 0.0],
            yaw: 0.7,
            pitch: -0.15,
            first_frame: true,
        })
    }

    pub fn chunk_count(&self) -> usize {
        self.terrain.len()
    }

    /// Current viewport size in pixels (for screen-space overlays like the
    /// other players' name tags).
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// First frame has been presented (for startup logging).
    pub fn take_first_frame(&mut self) -> bool {
        let f = self.first_frame;
        self.first_frame = false;
        f
    }

    /// Resize the viewport.
    pub fn resize(&mut self, canvas: &web_sys::HtmlCanvasElement) {
        let (w, h) = canvas_size(canvas);
        if w == self.width && h == self.height {
            return;
        }
        self.width = w;
        self.height = h;
        configure_surface(&self.surface, &self.device, self.surface_format, w, h);
    }

    /// Drop all terrain (and its pool occupancy) — used when switching to a
    /// different world (connecting to another server, or back to the
    /// built-in one). The GPU buffers are kept; new chunks stream in fresh.
    pub fn clear_terrain(&mut self) {
        self.terrain.clear();
        self.backlog.clear();
        self.terrain_v_used = 0;
        self.terrain_i_used = 0;
        self.evicted.clear();
        self.known.clear();
        self.pool_full_warns = 0;
    }

    /// Ingest world updates; meshing is budgeted (a few chunks per frame).
    pub fn apply_updates(&mut self, updates: Vec<WorldUpdate>) {
        self.backlog.extend(updates);
        let budget = 4;
        for _ in 0..budget {
            if self.backlog.is_empty() {
                break;
            }
            match self.backlog.remove(0) {
                WorldUpdate::Chunk { pos, data } => self.build_chunk(pos, data),
            }
        }
    }

    fn build_chunk(&mut self, pos: ChunkPos, data: Vec<u8>) {
        if data.len() != REGION_BLOCKS {
            return;
        }
        let mesh = rustcraft_world::mesh::build_chunk_mesh((pos.x * 16, pos.y * 16, pos.z * 16), &data);
        if mesh.is_empty() {
            // A re-sent fully-air chunk (e.g. everything broken) drops the
            // old mesh.
            if let Some(idx) = self.terrain.iter().position(|t| t.pos == pos) {
                self.terrain.remove(idx);
            }
            return;
        }
        let nv = (mesh.vertices.len() + mesh.water_vertices.len()) as u32 / 6;
        let ni = (mesh.indices.len() + mesh.water_indices.len()) as u32;
        // A re-sent chunk (an edit changed it) replaces the old mesh. The
        // old entry's pool space is orphaned (append-only pool); the next
        // compaction reclaims it.
        if let Some(idx) = self.terrain.iter().position(|t| t.pos == pos) {
            self.terrain.remove(idx);
        }
        if self.terrain_v_used + nv > VERT_CAP || self.terrain_i_used + ni > IDX_CAP {
            self.compact_pool(nv, ni);
        }
        if self.terrain_v_used + nv > VERT_CAP || self.terrain_i_used + ni > IDX_CAP {
            // The whole pool is in-view chunks (pathological terrain):
            // report this chunk as evicted so the streamer keeps trying to
            // deliver it, and warn (rate-limited).
            self.evicted.push(pos);
            self.pool_full_warns += 1;
            if self.pool_full_warns == 1 || self.pool_full_warns % 120 == 0 {
                web_sys::console::warn_1(&wasm_bindgen::JsValue::from_str(
                    "RustCraft: terrain pool full — chunk lost, requesting re-send",
                ));
            }
            return;
        }
        let base_v = self.terrain_v_used;
        let base_i = self.terrain_i_used;
        // Indices stay chunk-local; draw_indexed adds base_vertex to each.
        // Water is appended after the opaque part (its draw offsets the
        // base_vertex by the opaque vertex count).
        let ov = mesh.vertices.len() as u32 / 6;
        self.queue.write_buffer(&self.terrain_vbo, (base_v * 24) as u64, f32_bytes(&mesh.vertices));
        if !mesh.water_vertices.is_empty() {
            self.queue.write_buffer(
                &self.terrain_vbo,
                ((base_v + ov) * 24) as u64,
                f32_bytes(&mesh.water_vertices),
            );
        }
        self.queue.write_buffer(&self.terrain_ibo, (base_i * 4) as u64, u32_bytes(&mesh.indices));
        if !mesh.water_indices.is_empty() {
            self.queue.write_buffer(
                &self.terrain_ibo,
                ((base_i + mesh.indices.len() as u32) * 4) as u64,
                u32_bytes(&mesh.water_indices),
            );
        }
        self.terrain.push(TerrainChunk {
            pos,
            verts: mesh.vertices,
            idxs: mesh.indices,
            base_v,
            base_i,
            water: (!mesh.water_vertices.is_empty())
                .then_some((mesh.water_vertices, mesh.water_indices)),
        });
        self.terrain_v_used += nv;
        self.terrain_i_used += ni;
        self.known.insert(pos);
    }

    /// 3D Chebyshev distance in chunk cells between `pos` and the camera.
    fn chunk_dist(&self, pos: ChunkPos) -> i32 {
        let cx = (self.camera[0] / 16.0).floor() as i32;
        let cy = (self.camera[1] / 16.0).floor() as i32;
        let cz = (self.camera[2] / 16.0).floor() as i32;
        (pos.x - cx).abs().max((pos.y - cy).abs()).max((pos.z - cz).abs())
    }

    /// Free pool space for `need_v` extra vertices / `need_i` indices.
    ///
    /// The drop decision is based on the *live* total (not the high-water
    /// mark, which may include orphaned space from replaced chunks that a
    /// plain rewrite reclaims). Chunks are dropped from the farthest (3D
    /// Chebyshev) first, so the first casualties are chunks fully inside
    /// the fog (Chebyshev distance >= FOG_END/16+2: their nearest corner is
    /// (d-1)*16 > FOG_END blocks away — invisible). Every evicted chunk is
    /// reported via `evicted`; the streamer re-sends a chunk only if it is
    /// visible again, so fog-bound evictions cost nothing visually.
    /// Survivors are rewritten to the front of the pool.
    fn compact_pool(&mut self, need_v: u32, need_i: u32) {
        let before = self.terrain.len();
        // (index, distance, vertex count, index count) per chunk — water
        // counts against the same pool.
        let mut ranked: Vec<(usize, i32, u32, u32)> = self
            .terrain
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let wv = t.water.as_ref().map(|(w, _)| w.len() as u32 / 6).unwrap_or(0);
                let wi = t.water.as_ref().map(|(_, w)| w.len() as u32).unwrap_or(0);
                (
                    i,
                    self.chunk_dist(t.pos),
                    t.verts.len() as u32 / 6 + wv,
                    t.idxs.len() as u32 + wi,
                )
            })
            .collect();
        // Live totals (high-water minus orphans).
        let live_v: u32 = ranked.iter().map(|r| r.2).sum();
        let live_i: u32 = ranked.iter().map(|r| r.3).sum();
        // Farthest first; on equal distance the biggest chunk (frees the
        // most space per drop).
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));
        let mut drop = vec![false; self.terrain.len()];
        let mut used_v = live_v;
        let mut used_i = live_i;
        for &(_i, _d, vc, ic) in &ranked {
            if used_v + need_v <= VERT_CAP && used_i + need_i <= IDX_CAP {
                break;
            }
            // Every evicted chunk is reported (visible or fog-bound): the
            // streamer forgets it and re-sends it only if it is visible
            // again. Fog-bound drops cost a little bookkeeping; without
            // them, walking back over evicted terrain would leave holes.
            self.evicted.push(self.terrain[_i].pos);
            drop[_i] = true;
            used_v -= vc;
            used_i -= ic;
        }
        let dropping = drop.iter().any(|&d| d);
        let has_orphans = self.terrain_v_used > live_v || self.terrain_i_used > live_i;
        if !dropping && !has_orphans {
            return; // spurious call: there is room and nothing to reclaim
        }
        let mut nv = 0u32;
        let mut ni = 0u32;
        let mut idx = 0usize;
        self.terrain.retain_mut(|t| {
            let keep = !drop[idx];
            idx += 1;
            if keep {
                let ov = t.verts.len() as u32 / 6;
                let oi = t.idxs.len() as u32;
                t.base_v = nv;
                t.base_i = ni;
                self.queue
                    .write_buffer(&self.terrain_vbo, (nv * 24) as u64, f32_bytes(&t.verts));
                self.queue
                    .write_buffer(&self.terrain_ibo, (ni * 4) as u64, u32_bytes(&t.idxs));
                if let Some((wv, wi)) = &t.water {
                    self.queue
                        .write_buffer(&self.terrain_vbo, ((nv + ov) * 24) as u64, f32_bytes(wv));
                    self.queue
                        .write_buffer(&self.terrain_ibo, ((ni + oi) * 4) as u64, u32_bytes(wi));
                    nv += wv.len() as u32 / 6;
                    ni += wi.len() as u32;
                }
                nv += ov;
                ni += oi;
            }
            keep
        });
        self.terrain_v_used = nv;
        self.terrain_i_used = ni;
        // Keep `known` bounded over long exploration: once it grows past
        // 256K entries (~8 MB), forget chunks far beyond the fog. Trade-off:
        // if the player later returns to such distant terrain, silently
        // evicted chunks there are no longer re-requested (holes until a
        // block edit re-sends them). Everything within ~230 blocks is
        // always kept, so re-entering recently visited terrain (a few km
        // of travel) still works.
        if self.known.len() > 262144 {
            // (The closure can't call self.chunk_dist while `known` is
            // mutably borrowed, so compute the camera cell up front.)
            let cell = [
                (self.camera[0] / 16.0).floor() as i32,
                (self.camera[1] / 16.0).floor() as i32,
                (self.camera[2] / 16.0).floor() as i32,
            ];
            self.known.retain(|c| {
                (c.x - cell[0]).abs().max((c.y - cell[1]).abs()).max((c.z - cell[2]).abs())
                    <= VIEW_RADIUS + 2
            });
        }
        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str(&format!(
            "RustCraft: compacted terrain pool {} -> {} chunks ({} verts, {} dropped)",
            before,
            self.terrain.len(),
            nv,
            before - self.terrain.len()
        )));
    }

    /// Chunks evicted from the pool since the last call (visible or
    /// fog-bound). The app reports them to the streamer/backend via
    /// `report_evicted`; the streamer re-sends the ones that are visible
    /// again, at the normal stream rate.
    pub fn take_evicted(&mut self) -> Vec<ChunkPos> {
        std::mem::take(&mut self.evicted)
    }

    /// Horizontal (x/z) Chebyshev distance in chunk cells to the camera.
    /// (The 3D distance penalises the small y offset between the camera
    /// and the 4 chunk layers of terrain, which would hide chunks that are
    /// clearly visible.)
    fn chunk_dist_h(&self, pos: ChunkPos) -> i32 {
        let cx = (self.camera[0] / 16.0).floor() as i32;
        let cz = (self.camera[2] / 16.0).floor() as i32;
        (pos.x - cx).abs().max((pos.z - cz).abs())
    }

    /// Chunks we have renderable geometry for (`known`) that are NOT in the
    /// pool and within `max_dist` (horizontal Chebyshev) of the camera —
    /// i.e. visible holes. Used by the `POOL` telemetry line (and the walk
    /// test): a sustained non-zero count means the pool is losing visible
    /// chunks (capacity too small, or the eviction->re-stream path is
    /// broken). Note: `known` only ever contains chunks whose mesh was
    /// non-empty, so buried (geometry-less) chunks never count as holes.
    pub fn missing_visible(&self, max_dist: i32) -> Vec<ChunkPos> {
        let in_pool: std::collections::HashSet<ChunkPos> =
            self.terrain.iter().map(|t| t.pos).collect();
        self.known
            .iter()
            .filter(|c| self.chunk_dist_h(**c) <= max_dist && !in_pool.contains(c))
            .copied()
            .collect()
    }

    /// Set the wireframe highlight target (the block under the
    /// crosshair, computed by the server). No-op when unchanged.
    pub fn set_highlight(&mut self, target: Option<[i32; 3]>) {
        if target == self.highlight {
            return;
        }
        self.highlight = target;
        if let Some(t) = target {
            let v = rustcraft_world::mesh::highlight_vertices((t[0], t[1], t[2]));
            self.queue.write_buffer(&self.highlight_vbo, 0, f32_bytes(&v));
        }
    }

    /// Update agent spheres from the latest states. `own_id` is skipped:
    /// in first person the camera *is* that sphere (other players are
    /// rendered like NPCs, with name tags added by the web layer).
    pub fn set_agents(&mut self, states: Vec<AgentState>, own_id: u32) {
        // Remove stale.
        let ids: std::collections::HashSet<u32> = states.iter().map(|s| s.id).collect();
        self.agents.retain(|id, _| ids.contains(id));
        for s in &states {
            if s.id == own_id {
                continue; // first person: the player is the camera
            }
            let (verts, indices) = rustcraft_server::sphere_mesh(s);
            let existing = self.agents.get_mut(&s.id);
            match existing {
                Some(m) => {
                    if (m.vertex.size() as usize) < verts.len() * 4 {
                        // grow (shouldn't happen)
                        let v = self.device.create_buffer(&wgpu::BufferDescriptor {
                            label: Some("agent-vertex"),
                            size: (verts.len() * 4) as u64,
                            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
                            mapped_at_creation: false,
                        });
                        let i = self.device.create_buffer(&wgpu::BufferDescriptor {
                            label: Some("agent-index"),
                            size: (indices.len() * 4) as u64,
                            usage: BufferUsages::INDEX | BufferUsages::COPY_DST,
                            mapped_at_creation: false,
                        });
                        m.vertex = v;
                        m.index = i;
                    }
                    self.queue.write_buffer(&m.vertex, 0, f32_bytes(&verts));
                    self.queue.write_buffer(&m.index, 0, u32_bytes(&indices));
                    m.count = indices.len() as u32;
                }
                None => {
                    let vertex = self.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("agent-vertex"),
                        size: (verts.len() * 4) as u64,
                        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    let index = self.device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("agent-index"),
                        size: (indices.len() * 4) as u64,
                        usage: BufferUsages::INDEX | BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    });
                    self.queue.write_buffer(&vertex, 0, f32_bytes(&verts));
                    self.queue.write_buffer(&index, 0, u32_bytes(&indices));
                    self.agents.insert(s.id, AgentMesh {
                        vertex,
                        index,
                        count: indices.len() as u32,
                    });
                }
            }
        }
    }

    /// Render one frame from the first-person `camera` (eye position, yaw,
    /// pitch in the server's convention).
    pub fn render(&mut self, camera: [f32; 3], yaw: f32, pitch: f32) {
        self.camera = camera;
        self.yaw = yaw;
        self.pitch = pitch;
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(_) => return, // e.g. surface temporarily unavailable
        };
        let view = frame
            .texture
            .create_view(&TextureViewDescriptor {
                format: Some(self.surface_format),
                ..Default::default()
            });
        let depth_view = self.create_depth(self.width, self.height);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        self.render_pass_into(&mut encoder, &view, &depth_view, self.width, self.height);
        self.queue.submit([encoder.finish()]);
        frame.present();
    }

    fn create_depth(&self, w: u32, h: u32) -> wgpu::TextureView {
        let depth = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("depth"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        depth.create_view(&TextureViewDescriptor::default())
    }

    /// Encode the full scene (chunks + agents) into `encoder`, drawing into
    /// `color_view` with `depth_view`. Shared by the main render and the
    /// offscreen verify probe.
    fn render_pass_into(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        color_view: &wgpu::TextureView,
        depth_view: &wgpu::TextureView,
        _w: u32,
        _h: u32,
    ) {
        let camera = self.camera;
        let aspect = (self.width as f32 / self.height as f32).max(0.1);
        let vp = view_projection(camera, self.yaw, self.pitch, aspect, 1.15, 0.1, 300.0);
        self.queue.write_buffer(
            &self.uniform_buf,
            0,
            &uniform_bytes(&vp, camera, FOG_START, FOG_END, SKY),
        );
        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rustcraft-bg"),
            layout: &self.bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: self.uniform_buf.as_entire_binding(),
            }],
        });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("rustcraft-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: color_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: SKY[0] as f64,
                        g: SKY[1] as f64,
                        b: SKY[2] as f64,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &bg, &[]);
        // Terrain: one shared buffer pair, one draw call per chunk.
        if !self.terrain.is_empty() {
            pass.set_index_buffer(self.terrain_ibo.slice(0..), wgpu::IndexFormat::Uint32);
            pass.set_vertex_buffer(0, self.terrain_vbo.slice(..));
            for t in &self.terrain {
                pass.draw_indexed(
                    t.base_i..t.base_i + t.idxs.len() as u32,
                    t.base_v as i32,
                    0..1,
                );
            }
        }
        // Agents: small, per-agent buffers (the NPC load test can push the
        // count into the thousands — expect the draw calls to dominate).
        for m in self.agents.values() {
            pass.set_index_buffer(m.index.slice(..), wgpu::IndexFormat::Uint32);
            pass.set_vertex_buffer(0, m.vertex.slice(..));
            pass.draw_indexed(0..m.count, 0, 0..1);
        }
        // Block highlight: wireframe around the targeted block.
        if self.highlight.is_some() {
            pass.set_pipeline(&self.line_pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.set_vertex_buffer(0, self.highlight_vbo.slice(..));
            pass.draw(0..24, 0..1);
        }
        // Water: translucent pass after all opaque geometry (src-alpha
        // blend, no depth writes). Same pool buffers, different offsets.
        if self.terrain.iter().any(|t| t.water.is_some()) {
            pass.set_pipeline(&self.water_pipeline);
            pass.set_bind_group(0, &bg, &[]);
            pass.set_index_buffer(self.terrain_ibo.slice(0..), wgpu::IndexFormat::Uint32);
            pass.set_vertex_buffer(0, self.terrain_vbo.slice(..));
            for t in &self.terrain {
                if let Some((wv, wi)) = &t.water {
                    let ov = t.verts.len() as u32 / 6;
                    let oi = t.idxs.len() as u32;
                    pass.draw_indexed(
                        t.base_i + oi..t.base_i + oi + wi.len() as u32,
                        (t.base_v + ov) as i32,
                        0..1,
                    );
                    let _ = wv; // length implied by wi (6 indices per 4 verts)
                }
            }
        }
        drop(pass);
    }

}

/// Build a terrain pipeline. `fs_entry` selects the opaque (`fs_main`)
/// or water (`fs_water`) fragment entry; water blends with src-alpha and
/// skips depth writes so it composites over opaque geometry.
fn make_pipeline(
    device: &Device,
    shader: &ShaderModule,
    layout: &PipelineLayout,
    surface_format: TextureFormat,
    fs_entry: &str,
    water: bool,
) -> RenderPipeline {
    device.create_render_pipeline(&RenderPipelineDescriptor {
        label: Some(if water { "rustcraft-water" } else { "rustcraft-pipeline" }),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[VertexBufferLayout {
                array_stride: 24,
                step_mode: VertexStepMode::Vertex,
                attributes: &[
                    VertexAttribute {
                        offset: 0,
                        format: VertexFormat::Float32x3,
                        shader_location: 0,
                    },
                    VertexAttribute {
                        offset: 12,
                        format: VertexFormat::Float32x3,
                        shader_location: 1,
                    },
                ],
            }],
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some(fs_entry),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: surface_format,
                // Standard (non-premultiplied) src-alpha blending.
                blend: if water {
                    Some(wgpu::BlendState::ALPHA_BLENDING)
                } else {
                    None
                },
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: Some(wgpu::Face::Back),
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: !water,
            depth_compare: wgpu::CompareFunction::Less,
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    })
}

fn log_or_panic_adapter(adapter: &wgpu::Adapter) {
    let info = adapter.get_info();
    eprintln!("RustCraft: using adapter {:?}", info.name);
}

fn canvas_size(canvas: &web_sys::HtmlCanvasElement) -> (u32, u32) {
    let dpr = web_sys::window()
        .map(|w| w.device_pixel_ratio())
        .unwrap_or(1.0)
        .max(1.0) as f32;
    let w = (canvas.client_width().max(1) as f32 * dpr) as u32;
    let h = (canvas.client_height().max(1) as f32 * dpr) as u32;
    canvas.set_width(w);
    canvas.set_height(h);
    (w, h)
}

fn configure_surface(
    surface: &Surface<'_>,
    device: &Device,
    format: TextureFormat,
    width: u32,
    height: u32,
) {
    surface.configure(
        device,
        &SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            view_formats: vec![],
            width,
            height,
            desired_maximum_frame_latency: 2,
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
        },
    );
}

/// f32 slice as raw bytes (safe: f32 is a plain 4-byte type).
fn f32_bytes(v: &[f32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 4) }
}

/// u32 slice as raw bytes (safe: u32 is a plain 4-byte type).
fn u32_bytes(v: &[u32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 4) }
}
