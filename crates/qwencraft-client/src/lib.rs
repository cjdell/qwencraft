//! WebGPU (wgpu) renderer for Qwencraft.
//!
//! Chunks are meshed on the CPU (world position + baked light scalar
//! (voxel lighting + AO) + face UV + texture id, see
//! `qwencraft_world::mesh`) and uploaded as static buffers; the fragment
//! stage samples each block's procedural texture (see `textures.rs`).
//! Agents are spheres with baked per-vertex colour, re-uploaded each
//! frame.
//!
//! Pipelines:
//! - `pipeline`: opaque terrain (`fs_main`), vertex layout [pos, light,
//!   uv, tex] (7 floats);
//! - `water_pipeline`: translucent pass for water + glass (`fs_water`,
//!   per-texture alpha, src-alpha blend, no depth writes), same layout;
//! - `line_pipeline`: the block highlight wireframe (`fs_main`, same
//!   layout, line list, no cull);
//! - `agent_pipeline`: agent spheres (`fs_agent`, the old pos+colour
//!   layout — spheres are not blocks).
//!
//! This crate only compiles for wasm32 (WebGPU in the browser).

#![cfg(target_arch = "wasm32")]

mod shader;
mod sphere;
mod textures;

use shader::SHADER;
use textures::TEXTURES;

/// Terrain vertex stride in bytes: [pos(3), light(1), uv(2), tex(1)]
/// (must match `qwencraft_world::mesh::VERT_STRIDE`).
const TERRAIN_VERTEX_STRIDE: u64 = 28;

use wgpu::{
    Buffer, BufferUsages, Device, DeviceDescriptor, Instance, InstanceDescriptor, PipelineLayout,
    Queue, RenderPipeline, RenderPipelineDescriptor, RequestAdapterOptions, ShaderModule,
    ShaderModuleDescriptor, ShaderSource, Surface, SurfaceConfiguration, SurfaceTarget,
    TextureFormat, TextureViewDescriptor, VertexAttribute, VertexBufferLayout, VertexFormat,
    VertexStepMode,
};

use qwencraft_server::{AgentState, WorldUpdate, VIEW_RADIUS};
use qwencraft_world::camera::{uniform_bytes, view_projection, FOG_END, FOG_START, SKY, UNIFORM_SIZE};
use qwencraft_world::{ChunkPos, REGION_BLOCKS, Slot, TERRAIN_POOL_IDX, TERRAIN_POOL_VERTS, TerrainPool};
// Pool capacity lives in qwencraft_world so the host-side `pool_measure`
// example can assert the worst-case view stays under ~80% of it. Do not
// fork these numbers here (the old 2M/3M fork caused exactly the
// visible-eviction thrash the walk test guards against).
const IDX_CAP: u32 = TERRAIN_POOL_IDX;
const VERT_CAP: u32 = TERRAIN_POOL_VERTS;

/// Terrain mesh pool. Chunks own slots in one pre-allocated vertex/index
/// buffer pair, so a frame costs one set_index_buffer + one
/// set_vertex_buffer plus a single draw_indexed per chunk (instead of
/// three state changes per chunk). Slot bookkeeping is the pure
/// `TerrainPool` allocator (free list with coalescing + tail rewind, in
/// `qwencraft_world::pool`): when the pool is full, the farthest
/// (fog-bound) chunk's slot is evicted and reused in place, so a
/// drop+insert costs ONE small buffer upload — never a full-pool
/// re-upload (the old `compact_pool` re-uploaded the entire ~75 MB pool
/// on the main thread, which was the fly-mode stutter).
///
/// `VERT_CAP`/`IDX_CAP` (in `qwencraft_world`) are sized to hold the
/// ENTIRE worst-case streamed view — a view bigger than the pool forces
/// eviction to drop still-visible chunks, which then thrash on the
/// evict/re-send loop. Keep the worst case under ~80% of the caps
/// (measured by qwencraft-server's `pool_measure` example).
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

impl TerrainChunk {
    /// The chunk's pool slot (opaque + water; the water sub-mesh follows
    /// the opaque part in both buffers, so the slot is contiguous).
    fn slot(&self) -> Slot {
        let (wv, wi) = match &self.water {
            Some((w, i)) => (w.len() as u32 / 7, i.len() as u32),
            None => (0, 0),
        };
        Slot {
            base_v: self.base_v,
            base_i: self.base_i,
            v_count: self.verts.len() as u32 / 7 + wv,
            i_count: self.idxs.len() as u32 + wi,
        }
    }
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
    /// Full-screen depth target, sized to the surface. Created ONCE in
    /// `new()` and re-created in `resize()` only — allocating a fresh one
    /// per frame churned ~8 MB of GPU memory per frame (at 1080p) and
    /// exhausted device memory on Intel Xe iGPUs within minutes
    /// (`vkAllocateMemory … OUT_OF_DEVICE_MEMORY` on the "depth" texture,
    /// then a permanently invalid render pass = black screen).
    depth: wgpu::Texture,
    depth_view: wgpu::TextureView,
    pipeline: RenderPipeline,
    /// Translucent pipeline (water + glass; src-alpha blend, per-texture
    /// alpha, no depth writes); drawn after all opaque geometry.
    water_pipeline: RenderPipeline,
    /// Wireframe pipeline for the block highlight (line list, no cull).
    line_pipeline: RenderPipeline,
    /// Agent-sphere pipeline (baked pos+colour vertices, `fs_agent`).
    agent_pipeline: RenderPipeline,
    /// 24-vertex wireframe cube, re-uploaded when the target changes.
    highlight_vbo: Buffer,
    /// The block under the crosshair (server-computed); None = no target.
    highlight: Option<[i32; 3]>,
    bind_group_layout: wgpu::BindGroupLayout,
    uniform_buf: Buffer,
    terrain_vbo: Buffer,
    terrain_ibo: Buffer,
    /// Slot allocator for the terrain pool (high-water mark + coalescing
    /// free list of released slots).
    pool: TerrainPool,
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
    /// CSS-pixel size (the drawing buffer is scaled by devicePixelRatio —
    /// screen-space DOM overlays must be positioned in CSS pixels).
    css_width: u32,
    css_height: u32,
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
        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str("Qwencraft: creating surface"));
        let surface = instance
            .create_surface(SurfaceTarget::Canvas(canvas.clone()))
            .map_err(|e| format!("surface creation failed: {e:?}"))?;

        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str("Qwencraft: requesting adapter"));
        let adapter = instance
            .request_adapter(&RequestAdapterOptions::default())
            .await
            .map_err(|e| format!("no WebGPU adapter available: {e:?}"))?;
        log_or_panic_adapter(&adapter);

        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str("Qwencraft: requesting device"));
        let (device, queue) = adapter
            .request_device(&DeviceDescriptor {
                label: Some("qwencraft-device"),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("device request failed: {e:?}"))?;

        web_sys::console::log_1(&wasm_bindgen::JsValue::from_str("Qwencraft: configuring surface"));
        let caps = surface.get_capabilities(&adapter);
        let surface_format = *caps
            .formats
            .first()
            .ok_or_else(|| "surface has no formats".to_string())?;

        let (width, height, css_width, css_height) = canvas_size(canvas);
        configure_surface(&surface, &device, surface_format, width, height);
        let (depth, depth_view) = make_depth_texture(&device, width, height);

        // Pipeline: the module is the core shader + the procedural block
        // textures (one WGSL function per block, see textures.rs); WGSL
        // function order is irrelevant, so a plain concatenation works.
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("qwencraft-shader"),
            source: ShaderSource::Wgsl(format!("{SHADER}{TEXTURES}").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("qwencraft-bg-layout"),
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
            label: Some("qwencraft-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        // Terrain vertex layout: [pos(3f), light(1f), uv(2f), tex(1f)]
        // — 28 bytes, matching `qwencraft_world::mesh::VERT_STRIDE`.
        let terrain_layout = [VertexBufferLayout {
            array_stride: TERRAIN_VERTEX_STRIDE,
            step_mode: VertexStepMode::Vertex,
            attributes: &[
                VertexAttribute {
                    offset: 0,
                    format: VertexFormat::Float32x3,
                    shader_location: 0,
                },
                VertexAttribute {
                    offset: 12,
                    format: VertexFormat::Float32,
                    shader_location: 1,
                },
                VertexAttribute {
                    offset: 16,
                    format: VertexFormat::Float32x2,
                    shader_location: 2,
                },
                VertexAttribute {
                    offset: 24,
                    format: VertexFormat::Float32,
                    shader_location: 3,
                },
            ],
        }];

        // Agent-sphere layout: the classic [pos(3f), color(3f)] — spheres
        // carry baked shading, not block textures.
        let agent_layout = [VertexBufferLayout {
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
        }];

        let pipeline =
            make_pipeline(&device, &shader, &layout, surface_format, "vs_main", "fs_main", false, &terrain_layout);
        let water_pipeline =
            make_pipeline(&device, &shader, &layout, surface_format, "vs_main", "fs_water", true, &terrain_layout);
        let agent_pipeline = make_pipeline(
            &device,
            &shader,
            &layout,
            surface_format,
            "vs_agent",
            "fs_agent",
            false,
            &agent_layout,
        );

        // Highlight: same vertices/shader as terrain, but line list with no
        // culling (lines are visible from both sides).
        let line_pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("qwencraft-highlight"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &terrain_layout,
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
            size: (24 * TERRAIN_VERTEX_STRIDE) as u64,
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("qwencraft-uniforms"),
            size: UNIFORM_SIZE,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let terrain_vbo = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("terrain-vertices"),
            size: (VERT_CAP as u64) * TERRAIN_VERTEX_STRIDE,
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
            depth,
            depth_view,
            pipeline,
            water_pipeline,
            line_pipeline,
            agent_pipeline,
            highlight_vbo,
            highlight: None,
            bind_group_layout,
            uniform_buf,
            terrain_vbo,
            terrain_ibo,
            pool: TerrainPool::new(VERT_CAP, IDX_CAP),
            terrain: Vec::new(),
            evicted: Vec::new(),
            known: std::collections::HashSet::new(),
            pool_full_warns: 0,
            agents: std::collections::HashMap::new(),
            backlog: Vec::new(),
            width,
            height,
            css_width,
            css_height,
            camera: [0.0, 40.0, 0.0],
            yaw: 0.7,
            pitch: -0.15,
            first_frame: true,
        })
    }

    pub fn chunk_count(&self) -> usize {
        self.terrain.len()
    }

    /// Number of pool chunks in the 3x3 chunk box (xz) centred on the chunk
    /// containing `spawn` — the immediate spawn area. Telemetry for the
    /// "invisible spawn" regression: the streamer sends nearest-first, so
    /// the spawn area is exactly the initial burst. If that burst never
    /// reaches the pool (e.g. dropped while the renderer is still
    /// initialising), this stays 0 while `sent` grows — and nothing re-sends
    /// it, so the hole is permanent.
    pub fn spawn_near_count(&self, spawn: [f32; 2]) -> usize {
        let pcx = (spawn[0] / 16.0).floor() as i32;
        let pcz = (spawn[1] / 16.0).floor() as i32;
        self.terrain
            .iter()
            .filter(|t| (t.pos.x - pcx).abs() <= 1 && (t.pos.z - pcz).abs() <= 1)
            .count()
    }

    /// Number of released slots sitting in the pool's free list (telemetry:
    /// a healthy pool reuses them quickly, so this stays small; a steadily
    /// growing count means fragmentation is outpacing reuse).
    pub fn free_slots(&self) -> usize {
        self.pool.free_slots()
    }

    /// Current drawing-buffer size in device pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Current viewport size in **CSS pixels** (what `left`/`top` on a
    /// positioned DOM element are measured in). On a high-DPI display the
    /// drawing buffer is `devicePixelRatio`× the CSS size — projecting
    /// world points into buffer pixels and using them for CSS offsets
    /// scatters screen-space overlays (name tags) everywhere.
    pub fn css_size(&self) -> (u32, u32) {
        (self.css_width, self.css_height)
    }

    /// First frame has been presented (for startup logging).
    pub fn take_first_frame(&mut self) -> bool {
        let f = self.first_frame;
        self.first_frame = false;
        f
    }

    /// Resize the viewport.
    pub fn resize(&mut self, canvas: &web_sys::HtmlCanvasElement) {
        let (w, h, cw, ch) = canvas_size(canvas);
        if w == self.width && h == self.height {
            return;
        }
        self.width = w;
        self.height = h;
        self.css_width = cw;
        self.css_height = ch;
        configure_surface(&self.surface, &self.device, self.surface_format, w, h);
        // Re-create the depth target for the new size (the reassignment
        // drops the old texture + view, freeing the GPU memory).
        (self.depth, self.depth_view) = make_depth_texture(&self.device, w, h);
    }

    /// Drop all terrain (and its pool occupancy) — used when switching to a
    /// different world (connecting to another server, or back to the
    /// built-in one). The GPU buffers are kept; new chunks stream in fresh.
    pub fn clear_terrain(&mut self) {
        self.terrain.clear();
        self.backlog.clear();
        self.pool.reset();
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
        self.trim_known();
    }

    fn build_chunk(&mut self, pos: ChunkPos, data: Vec<u8>) {
        if data.len() != REGION_BLOCKS {
            return;
        }
        let mesh = qwencraft_world::mesh::build_chunk_mesh((pos.x * 16, pos.y * 16, pos.z * 16), &data);
        let nv = (mesh.vertices.len() + mesh.water_vertices.len()) as u32 / 7;
        let ni = (mesh.indices.len() + mesh.water_indices.len()) as u32;
        if mesh.is_empty() {
            // A re-sent fully-air chunk (e.g. everything broken) drops the
            // old mesh and frees its pool slot.
            if let Some(idx) = self.terrain.iter().position(|t| t.pos == pos) {
                let old = self.terrain.remove(idx);
                self.pool.release(old.slot());
            }
            return;
        }
        // A re-sent chunk (an edit changed it) replaces the old mesh; its
        // slot is freed immediately (tail rewind or free-list slot). The
        // old append-only design orphaned it until a full compaction.
        if let Some(idx) = self.terrain.iter().position(|t| t.pos == pos) {
            let old = self.terrain.remove(idx);
            self.pool.release(old.slot());
        }
        // Allocate pool space: tail headroom → free slot (best fit) →
        // evict the farthest (fog-bound) chunks until a slot fits. Every
        // step costs at most one small buffer upload — never a full-pool
        // re-upload (which is what made fly mode stutter).
        let Some(slot) = self
            .pool
            .alloc(nv, ni)
            .or_else(|| self.drop_for_slot(nv, ni))
        else {
            // The pool cannot hold this chunk even with everything evicted
            // (pathological): report it as evicted so the streamer keeps
            // trying to deliver it, and warn (rate-limited).
            self.evicted.push(pos);
            self.pool_full_warns += 1;
            if self.pool_full_warns == 1 || self.pool_full_warns % 120 == 0 {
                web_sys::console::warn_1(&wasm_bindgen::JsValue::from_str(&format!(
                    "Qwencraft: terrain pool full — chunk ({},{},{}) lost, requesting re-send",
                    pos.x, pos.y, pos.z
                )));
            }
            return;
        };
        let (base_v, base_i) = (slot.base_v, slot.base_i);
        // Indices stay chunk-local; draw_indexed adds base_vertex to each.
        // Water is appended after the opaque part (its draw offsets the
        // base_vertex by the opaque vertex count).
        let ov = mesh.vertices.len() as u32 / 7;
        self.queue
            .write_buffer(&self.terrain_vbo, (base_v as u64) * TERRAIN_VERTEX_STRIDE, f32_bytes(&mesh.vertices));
        if !mesh.water_vertices.is_empty() {
            self.queue.write_buffer(
                &self.terrain_vbo,
                ((base_v + ov) as u64) * TERRAIN_VERTEX_STRIDE,
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
        self.known.insert(pos);
    }

    /// 3D Chebyshev distance in chunk cells between `pos` and the camera.
    fn chunk_dist(&self, pos: ChunkPos) -> i32 {
        let cx = (self.camera[0] / 16.0).floor() as i32;
        let cy = (self.camera[1] / 16.0).floor() as i32;
        let cz = (self.camera[2] / 16.0).floor() as i32;
        (pos.x - cx).abs().max((pos.y - cy).abs()).max((pos.z - cz).abs())
    }

    /// Evict chunks farthest-from-camera first (fog-bound trail before
    /// visible terrain) until one of their slots — or a coalesced set of
    /// them — can hold `need_v`/`need_i`, and return that slot.
    ///
    /// Each eviction frees its slot (coalescing with adjacent free slots);
    /// the retry may then take the coalesced run, another free slot, or the
    /// tail headroom a tail-adjacent eviction rewound. Every evicted chunk
    /// is reported via `evicted`; the streamer re-sends a chunk only if it
    /// is visible again, so fog-bound evictions cost nothing visually
    /// (without the report, walking back over evicted terrain would leave
    /// holes). Returns None when there are no chunks left to evict (the
    /// pool cannot hold the chunk — the caller drops it with a warning).
    fn drop_for_slot(&mut self, need_v: u32, need_i: u32) -> Option<Slot> {
        let mut ranked: Vec<(ChunkPos, i32, u32)> = self
            .terrain
            .iter()
            .map(|t| (t.pos, self.chunk_dist(t.pos), t.slot().v_count))
            .collect();
        // Farthest first; on equal distance the biggest chunk (frees the
        // most space per drop).
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));
        for (pos, _d, _s) in ranked {
            // Re-locate by pos (earlier removals shift indices).
            let idx = self
                .terrain
                .iter()
                .position(|t| t.pos == pos)
                .expect("chunk pos unique in pool");
            let t = self.terrain.remove(idx);
            self.evicted.push(t.pos);
            self.pool.release(t.slot());
            if let Some(s) = self.pool.alloc(need_v, need_i) {
                return Some(s);
            }
        }
        None
    }

    /// Keep the `known` telemetry set bounded over long exploration: once
    /// it grows past 256K entries (~8 MB), forget chunks far beyond the
    /// fog. Trade-off: if the player later returns to such distant terrain,
    /// silently evicted chunks there are no longer re-requested (holes
    /// until a block edit re-sends them). Everything within ~230 blocks is
    /// always kept, so re-entering recently visited terrain (a few km of
    /// travel) still works. (Cheap to run per update: the trim itself only
    /// fires once per ~256K chunk visits.)
    fn trim_known(&mut self) {
        if self.known.len() <= 262144 {
            return;
        }
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
            let v = qwencraft_world::mesh::highlight_vertices((t[0], t[1], t[2]));
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
            let (verts, indices) =
                sphere::sphere_mesh([s.pos.x, s.pos.y, s.pos.z], s.radius, s.color);
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
    /// `time` is wall-clock seconds (drives the water texture's ripples;
    /// pass 0.0 for still water).
    pub fn render(&mut self, camera: [f32; 3], yaw: f32, pitch: f32, time: f32) {
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
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        self.render_pass_into(&mut encoder, &view, &self.depth_view, self.width, self.height, time);
        self.queue.submit([encoder.finish()]);
        frame.present();
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
        time: f32,
    ) {
        let camera = self.camera;
        let aspect = (self.width as f32 / self.height as f32).max(0.1);
        let vp = view_projection(camera, self.yaw, self.pitch, aspect, 1.15, 0.1, 300.0);
        self.queue.write_buffer(
            &self.uniform_buf,
            0,
            &uniform_bytes(&vp, camera, FOG_START, FOG_END, time, SKY),
        );
        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("qwencraft-bg"),
            layout: &self.bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: self.uniform_buf.as_entire_binding(),
            }],
        });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("qwencraft-pass"),
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
        // Spheres carry baked colour, so they use their own pipeline.
        pass.set_pipeline(&self.agent_pipeline);
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
                    let ov = t.verts.len() as u32 / 7;
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

/// Build a pipeline. `vs_entry`/`fs_entry` pick the entry points (`vs_main`
/// + `fs_main` for opaque terrain, `vs_main` + `fs_water` for the
/// translucent pass, `vs_agent` + `fs_agent` for the baked-colour agent
/// spheres); `water` enables src-alpha blending + no depth writes (the
/// translucent pass composites over opaque geometry).
fn make_pipeline(
    device: &Device,
    shader: &ShaderModule,
    layout: &PipelineLayout,
    surface_format: TextureFormat,
    vs_entry: &str,
    fs_entry: &str,
    water: bool,
    vertex_layout: &[VertexBufferLayout<'_>],
) -> RenderPipeline {
    device.create_render_pipeline(&RenderPipelineDescriptor {
        label: Some(match (vs_entry, fs_entry) {
            ("vs_agent", "fs_agent") => "qwencraft-agents",
            (_, "fs_water") => "qwencraft-water",
            _ => "qwencraft-pipeline",
        }),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some(vs_entry),
            compilation_options: Default::default(),
            buffers: vertex_layout,
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
    eprintln!("Qwencraft: using adapter {:?}", info.name);
}

/// The canvas' drawing-buffer size (CSS size × devicePixelRatio) plus the
/// CSS size. Buffer pixels are what the GPU renders into; CSS pixels are
/// what positioned DOM overlays (name tags) use.
fn canvas_size(canvas: &web_sys::HtmlCanvasElement) -> (u32, u32, u32, u32) {
    let dpr = web_sys::window()
        .map(|w| w.device_pixel_ratio())
        .unwrap_or(1.0)
        .max(1.0) as f32;
    let cw = canvas.client_width().max(1) as u32;
    let ch = canvas.client_height().max(1) as u32;
    let w = (cw as f32 * dpr).round() as u32;
    let h = (ch as f32 * dpr).round() as u32;
    canvas.set_width(w);
    canvas.set_height(h);
    (w, h, cw, ch)
}

/// The full-screen depth target (created once per surface size — see the
/// `Renderer::depth` field docs for why it must not be per-frame).
fn make_depth_texture(device: &Device, w: u32, h: u32) -> (wgpu::Texture, wgpu::TextureView) {
    let depth = device.create_texture(&wgpu::TextureDescriptor {
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
    let depth_view = depth.create_view(&TextureViewDescriptor::default());
    (depth, depth_view)
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
